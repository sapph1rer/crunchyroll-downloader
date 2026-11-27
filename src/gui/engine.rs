//! Work the GUI runs in the background: fetching the episode list, and downloading
//! the episodes the user picked. Secrets arrive by value and are zeroed when dropped.

use crunchyroll_downloader::api::{Api, ApiError, EpisodeInfo, SeasonEpisode};
use crunchyroll_downloader::backoff::Backoff;
use crunchyroll_downloader::episode::{download_episodes, hint_for, Options, Summary};
use crunchyroll_downloader::util::parse_url;
use crunchyroll_downloader::{cdm, ffmpeg, ui};
use std::sync::Arc;
use std::time::Duration;
use zeroize::Zeroizing;

/// The Widevine device, in whichever form the vault stores it.
pub enum DeviceInput {
    Wvd(Zeroizing<Vec<u8>>),
    Parts { client_id: Zeroizing<Vec<u8>>, pem: Zeroizing<String> },
}

/// Everything a download needs.
pub struct RunInput {
    pub etp: Zeroizing<String>,
    pub device: DeviceInput,
    pub opts: Options,
    pub delay: Duration,
    /// The ffmpeg path from Settings. Empty means search as usual.
    pub ffmpeg_path: String,
    /// The episodes the user picked, in the order they are shown.
    pub episodes: Vec<SeasonEpisode>,
    /// Optional proxy URL for API calls (empty = direct).
    pub proxy_url: String,
}

/// One season and its episodes.
#[derive(Debug, Clone)]
pub struct SeasonGroup {
    pub number: i64,
    pub episodes: Vec<SeasonEpisode>,
}

/// What a link contains: a series with its seasons, or a single episode.
#[derive(Debug, Clone)]
pub struct Catalog {
    pub title: String,
    pub seasons: Vec<SeasonGroup>,
}

pub struct FetchInput {
    pub etp: Zeroizing<String>,
    pub url: String,
    /// Locales sent with the season and episode requests.
    pub audio: String,
    pub subs: String,
    /// Optional proxy URL for API calls (empty = direct).
    pub proxy_url: String,
}

/// Logs the failure with a hint, and returns the message for the window.
fn report(what: &str, e: &anyhow::Error) -> String {
    ui::error_block(what, format!("{e:#}"), hint_for(e));
    format!("{what}: {e:#}")
}

pub use crunchyroll_downloader::browse::{CatalogItem, Genre, Kind, Show, Sort};

/// How many cards one Browse page loads.
pub const BROWSE_PAGE: usize = 36;

/// What the Browse page asks for: a page of the catalog, or a search.
#[derive(Clone, Debug)]
pub enum BrowseQuery {
    Page { sort: Sort, show: Show, genre: Option<String>, start: usize },
    Search(String),
}

pub struct BrowseInput {
    /// A signed-in client from an earlier request, reused so paging and searching don't
    /// sign in again every time.
    pub api: Option<Api>,
    pub etp: Zeroizing<String>,
    pub proxy_url: String,
    pub locale: String,
    pub query: BrowseQuery,
    /// Also load the genre list (done once per session).
    pub want_genres: bool,
}

/// Cards for the Browse grid. `total` is known for catalog pages, not for searches.
#[derive(Clone, Debug)]
pub struct BrowsePage {
    pub items: Vec<CatalogItem>,
    pub total: Option<usize>,
    pub append: bool,
    /// The genre list, when it was asked for and could be read.
    pub genres: Option<Vec<Genre>>,
}

/// Loads a Browse page or search. Returns the signed-in client (to reuse next time, or None
/// when sign-in failed) along with the result.
pub async fn browse(input: BrowseInput) -> (Option<Api>, Result<BrowsePage, String>) {
    let api = match input.api {
        Some(api) => api,
        None => {
            let proxy_opt = if input.proxy_url.is_empty() { None } else { Some(input.proxy_url.as_str()) };
            match Api::login_at(crunchyroll_downloader::api::TOKEN_URL, input.etp.to_string(), proxy_opt).await {
                Ok(api) => api,
                Err(e) => {
                    let msg = if matches!(e, ApiError::AuthRejected(_)) {
                        format!("{LOGIN_EXPIRED}.")
                    } else {
                        format!("Could not reach Crunchyroll: {e}")
                    };
                    return (None, Err(msg));
                }
            }
        }
    };
    let result = match &input.query {
        BrowseQuery::Page { sort, show, genre, start } => api
            .browse(*sort, *show, genre.as_deref(), *start, BROWSE_PAGE, &input.locale)
            .await
            .map(|(items, total)| BrowsePage { items, total: Some(total), append: *start > 0, genres: None }),
        BrowseQuery::Search(q) => api
            .search(q, 40, &input.locale)
            .await
            .map(|items| BrowsePage { items, total: None, append: false, genres: None }),
    }
    .map_err(|e| format!("{e:#}"));
    // Genres are a nicety: a failure just hides the genre filter.
    let result = match result {
        Ok(mut page) if input.want_genres => {
            page.genres = api.genres(&input.locale).await.ok().filter(|g| !g.is_empty());
            Ok(page)
        }
        other => other,
    };
    (Some(api), result)
}

/// Downloads a poster image. Posters come from a public CDN, so this goes direct (no proxy,
/// no sign-in) through one shared client with a browser fingerprint.
pub async fn poster(url: String) -> Result<Vec<u8>, String> {
    static CLIENT: std::sync::OnceLock<wreq::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        crunchyroll_downloader::api::build_client(None, true).unwrap_or_else(|_| wreq::Client::new())
    });
    let resp = client
        .get(&url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?;
    resp.bytes().await.map(|b| b.to_vec()).map_err(|e| e.to_string())
}

/// Starts an error message when Crunchyroll refused the saved login (expired or signed out
/// elsewhere). The window checks for it to offer "Log in again" instead of a raw error.
pub const LOGIN_EXPIRED: &str = "Your Crunchyroll login has expired or was signed out";

/// Whether an error message from `fetch` means the saved login no longer works.
pub fn is_login_expired(message: &str) -> bool {
    message.starts_with(LOGIN_EXPIRED)
}

/// Checks a freshly captured or pasted etp_rt by asking Crunchyroll for a token with it.
/// Ok: it works. Err((true, why)): Crunchyroll refused it. Err((false, why)): it could not be
/// checked right now (network, Cloudflare), which says nothing about the cookie itself.
pub async fn verify_login(etp: Zeroizing<String>, proxy_url: String) -> Result<(), (bool, String)> {
    let proxy_opt = if proxy_url.is_empty() { None } else { Some(proxy_url.as_str()) };
    match Api::login_at(crunchyroll_downloader::api::TOKEN_URL, etp.to_string(), proxy_opt).await {
        Ok(_) => Ok(()),
        Err(e) => {
            let rejected = matches!(e, ApiError::AuthRejected(_));
            Err((rejected, e.to_string()))
        }
    }
}

/// Logs in and reads the season and episode list of a link.
pub async fn fetch(input: FetchInput) -> Result<Catalog, String> {
    ui::title("Fetching the episode list");
    let proxy_opt = if input.proxy_url.is_empty() { None } else { Some(input.proxy_url.as_str()) };
    let api = match Api::login_at(crunchyroll_downloader::api::TOKEN_URL, input.etp.to_string(), proxy_opt).await {
        Ok(api) => api,
        Err(e) => {
            let rejected = matches!(e, ApiError::AuthRejected(_));
            let err = anyhow::Error::new(e);
            let msg = report("Login failed", &err);
            return Err(if rejected { format!("{LOGIN_EXPIRED}. {msg}") } else { msg });
        }
    };
    drop(input.etp);
    ui::ok("Logged in");

    let Some((kind, id)) = parse_url(&input.url) else {
        let msg = "This is not a Crunchyroll episode, series or movie link.";
        ui::error_block("Unsupported link", msg, Some("Use a link that contains /watch/, /series/ or /movie_listing/."));
        return Err(msg.to_string());
    };

    let catalog = if kind == "watch" {
        let info: EpisodeInfo = api
            .episode_info(id)
            .await
            .map_err(|e| report("Could not read the episode", &e))?;
        let m = &info.episode_metadata;
        let ep = SeasonEpisode {
            id: id.to_string(),
            versions: m.versions.clone(),
            season_number: m.season_number,
            episode_number: m.episode_number,
            series_title: m.series_title.clone(),
            audio_locale: m.audio_locale.clone(),
            title: info.title.clone(),
            availability_starts: m.availability_starts.clone(),
            is_movie: m.is_movie,
        };
        Catalog {
            title: m.series_title.clone(),
            seasons: vec![SeasonGroup { number: m.season_number, episodes: vec![ep] }],
        }
    } else if kind == "movie_listing" {
        // A movie listing holds one or more films; they are shown as one group.
        let (title, films) = api
            .movie_listing(id, &input.audio, &input.subs)
            .await
            .map_err(|e| report("Could not read the movie", &e))?;
        Catalog { title, seasons: vec![SeasonGroup { number: 0, episodes: films }] }
    } else {
        let seasons = api
            .seasons(id, &input.audio, &input.subs)
            .await
            .map_err(|e| report("Could not read the seasons", &e))?;
        let mut groups = Vec::new();
        let mut title = String::new();
        for s in seasons {
            let episodes = api
                .season_episodes(&s.id, &input.audio, &input.subs)
                .await
                .map_err(|e| report("Could not read the episodes", &e))?;
            if episodes.is_empty() {
                continue;
            }
            if title.is_empty() {
                title = episodes[0].series_title.clone();
            }
            groups.push(SeasonGroup { number: s.season_number, episodes });
        }
        groups.sort_by_key(|g| g.number);
        Catalog { title, seasons: groups }
    };

    let total: usize = catalog.seasons.iter().map(|g| g.episodes.len()).sum();
    if total == 0 {
        let msg = "No episodes were found for this link.";
        ui::warn(msg);
        return Err(msg.to_string());
    }
    ui::ok(format!(
        "Found {} season(s) and {} episode(s) of {}",
        catalog.seasons.len(),
        total,
        catalog.title
    ));
    Ok(catalog)
}

/// Runs the download for the picked episodes. Returns 0 when nothing failed, 1 otherwise.
pub async fn run(input: RunInput) -> i32 {
    let device = match &input.device {
        DeviceInput::Wvd(bytes) => cdm::parse_wvd(bytes),
        DeviceInput::Parts { client_id, pem } => cdm::device_from_parts(client_id.to_vec(), pem.as_str()),
    };
    match device {
        Ok(d) => cdm::install(Arc::new(d)),
        Err(e) => {
            ui::error_block(
                "Could not load the Widevine device",
                format!("{e:#}"),
                Some("Set the device up again in Settings > Set up."),
            );
            return 1;
        }
    }

    ui::title("Checking ffmpeg");
    let ffmpeg_path = match ffmpeg::locate_with(&input.ffmpeg_path) {
        Ok(p) => p,
        Err(e) => {
            ui::error_block(
                "ffmpeg not found",
                format!("{e:#}"),
                Some("Install ffmpeg, put ffmpeg.exe next to this program, or choose it in Settings > Downloads."),
            );
            return 1;
        }
    };
    if let Err(e) = ffmpeg::verify(&ffmpeg_path) {
        ui::error_block("ffmpeg does not work", format!("{e:#}"), None);
        return 1;
    }
    ui::ok(format!("ffmpeg ready: {}", ffmpeg_path.display()));

    ui::title("Logging in to Crunchyroll");
    let proxy_opt = if input.proxy_url.is_empty() { None } else { Some(input.proxy_url.as_str()) };
    let api = match Api::login_at(crunchyroll_downloader::api::TOKEN_URL, input.etp.to_string(), proxy_opt).await {
        Ok(api) => api,
        Err(e) => {
            let err = anyhow::Error::new(e);
            ui::error_block("Login failed", format!("{err:#}"), hint_for(&err));
            return 1;
        }
    };
    drop(input.etp);
    ui::ok("Logged in");

    let backoff = Backoff::new(input.delay);
    let mut summary = Summary::default();
    download_episodes(&api, &backoff, &input.opts, &input.episodes, &mut summary).await;
    summary.print();
    summary.exit_code()
}
