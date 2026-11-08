//! Work the GUI runs in the background: fetching the episode list, and downloading
//! the episodes the user picked. Secrets arrive by value and are zeroed when dropped.

use crunchyroll_downloader::api::{Api, EpisodeInfo, SeasonEpisode};
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
}

/// Logs the failure with a hint, and returns the message for the window.
fn report(what: &str, e: &anyhow::Error) -> String {
    ui::error_block(what, format!("{e:#}"), hint_for(e));
    format!("{what}: {e:#}")
}

/// Logs in and reads the season and episode list of a link.
pub async fn fetch(input: FetchInput) -> Result<Catalog, String> {
    ui::title("Fetching the episode list");
    let api = match Api::login(input.etp.to_string()).await {
        Ok(api) => api,
        Err(e) => {
            let err = anyhow::Error::new(e);
            return Err(report("Login failed", &err));
        }
    };
    drop(input.etp);
    ui::ok("Logged in");

    let Some((kind, id)) = parse_url(&input.url) else {
        let msg = "This is not a Crunchyroll episode or series link.";
        ui::error_block("Unsupported link", msg, Some("Use a link that contains /watch/ or /series/."));
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
        };
        Catalog {
            title: m.series_title.clone(),
            seasons: vec![SeasonGroup { number: m.season_number, episodes: vec![ep] }],
        }
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
    let api = match Api::login(input.etp.to_string()).await {
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
