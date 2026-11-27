//! App state, messages and update logic for the GUI.

use crate::engine::Catalog;
use crate::{engine, logs, theme, views};
use crunchyroll_downloader::api::SeasonEpisode;
use crunchyroll_downloader::backoff::parse_duration;
use crunchyroll_downloader::episode::{episode_label, info_from_season_episode, Options};
use crunchyroll_downloader::ffmpeg;
use crunchyroll_downloader::log::Kind;
use crunchyroll_downloader::progress::{self, Bar, EpisodeState, Overall};
use crunchyroll_downloader::ui;
use crunchyroll_downloader::util::{parse_count, parse_langs, parse_url, preferred_request_locale};
use crunchyroll_downloader::vault::{self, Settings, Vault, MIN_PASSWORD_LEN};
use iced::animation::{Animation, Easing};
use iced::keyboard;
use iced::task::Handle;
use iced::window;
use iced::{mouse, Element, Subscription, Task};
use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use zeroize::{Zeroize, Zeroizing};

/// Which language list a chip belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LangField {
    Audio,
    Subs,
    Cc,
}

/// Log lines kept in memory. The file export contains these lines.
const LOG_CAP: usize = 1000;
/// Progress differences smaller than this are treated as already reached.
const PROGRESS_EPS: f32 = 0.0005;
/// How long a dropdown list takes to open or close, in milliseconds.
const DROPDOWN_MS: u64 = 220;
/// How often the idle check runs, in seconds.
const IDLE_CHECK_SECS: u64 = 15;
/// Height of one navigation item in the rail, and the gap between items.
pub const NAV_H: f32 = 64.0;
pub const NAV_GAP: f32 = 8.0;

/// Top of the navigation pill for a tab.
pub fn tab_y(tab: Tab) -> f32 {
    let index = match tab {
        Tab::Main => 0.0,
        Tab::Browse => 1.0,
        Tab::Settings => 2.0,
        Tab::Logs => 3.0,
    };
    index * (NAV_H + NAV_GAP)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Main,
    Browse,
    Settings,
    Logs,
}

/// A poster image for a Browse card.
#[derive(Clone)]
pub enum Poster {
    Loading,
    Ready(iced::widget::image::Handle),
    Failed,
}

/// Everything the Browse tab shows: the grid, its search and order, cached posters, and
/// the series opened from it.
#[derive(Default)]
pub struct BrowseState {
    /// Search text. Empty shows the catalog in `sort` order.
    pub query: String,
    pub sort: engine::Sort,
    /// Series or movies.
    pub show: engine::Show,
    /// A genre filter (its id), or None for every genre.
    pub genre: Option<String>,
    /// Genres offered by the catalog; empty until loaded (or if they can't be read).
    pub genres: Vec<engine::Genre>,
    pub genres_asked: bool,
    pub items: Vec<engine::CatalogItem>,
    /// How many catalog entries exist (for "Load more"); None for search results.
    pub total: Option<usize>,
    pub loading: bool,
    pub error: Option<String>,
    /// Bumped by every new request; a result for an older number is stale and dropped.
    pub seq: u64,
    /// A signed-in client reused across pages and searches.
    pub api: Option<crunchyroll_downloader::api::Api>,
    /// Posters by image URL, loaded once per session.
    pub posters: std::collections::HashMap<String, Poster>,
    /// The series whose detail page is open, if any.
    pub selected: Option<engine::CatalogItem>,
    /// Where the grid was scrolled to, so "Back" returns to the same spot.
    pub grid_scroll_y: f32,
}

impl BrowseState {
    /// More catalog pages exist (search results come in one page).
    pub fn has_more(&self) -> bool {
        self.query.trim().is_empty() && self.total.is_some_and(|t| self.items.len() < t) && self.error.is_none()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// No vault yet: choose a password to create one.
    Create,
    /// A vault exists: enter the password to open it.
    Unlock,
    /// Unlocked: the tabs are available.
    Ready,
}

/// The dropdowns on the Settings page. Only one is open at a time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DropdownId {
    Video,
    Audio,
    Lang(LangField),
    /// The auto-lock (sleep) interval picker.
    AutoLock,
    /// The Browse genre filter.
    Genre,
}

/// Whether ffmpeg can be used, as found by the last check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FfmpegState {
    /// Not checked yet (the vault is locked).
    Unknown,
    Checking,
    /// Found and working, at this path.
    Ready(PathBuf),
    /// Missing or not working. The text says why.
    Broken(String),
}

/// Where a Crunchyroll sign-in currently stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginPhase {
    /// The login window is open; waiting for the user to sign in there.
    Waiting,
    /// A cookie arrived (from the window or a paste) and is being checked with Crunchyroll.
    Verifying,
}

/// Progress of an automatic ffmpeg download.
#[derive(Clone, Debug)]
pub enum FfmpegDlEvent {
    /// Download fraction 0.0..=1.0, or a negative value when the total size is unknown.
    Progress(f32),
    /// Download finished; the archive is being unpacked.
    Extracting,
    /// Finished: Ok(path to the installed ffmpeg) or Err(reason).
    Done(Result<PathBuf, String>),
}

/// One episode in the download queue, with its latest state.
#[derive(Clone, Debug)]
pub struct QueueItem {
    pub ep: SeasonEpisode,
    pub label: String,
    pub state: EpisodeState,
}

impl QueueItem {
    fn waiting(ep: &SeasonEpisode) -> Self {
        Self {
            label: episode_label(&info_from_season_episode(ep)),
            ep: ep.clone(),
            state: EpisodeState::Waiting,
        }
    }
}

pub struct App {
    pub screen: Screen,
    pub vault_path: PathBuf,
    pub vault: Option<Vault>,
    pub tab: Tab,
    pub busy: bool,
    pub notice: Option<(Kind, String)>,
    pub pw: String,
    pub pw2: String,
    pub old_pw: String,
    pub new_pw: String,
    pub new_pw2: String,
    pub url: String,
    pub show_secret: bool,
    /// Clock for animations. Updated on every animation frame.
    pub now: Instant,
    /// 0 → 1 after a tab change; drives the slide-in of the page.
    pub tab_anim: Animation<f32>,
    /// The navigation pill slides from `nav_from` to `nav_to`.
    pub nav_from: f32,
    pub nav_to: f32,
    pub nav_anim: Animation<f32>,
    /// 0 → 1 when the setup panel opens, 1 → 0 when it closes.
    pub setup_anim: Animation<f32>,
    /// The episode list of the last fetch, and what the user picked from it.
    pub catalog: Option<Catalog>,
    pub picked: BTreeSet<String>,
    /// Episodes in the current list whose finished file is already in the output folder.
    pub downloaded: BTreeSet<String>,
    /// The Browse tab.
    pub browse: BrowseState,
    /// The series id the loaded episode list belongs to, when it came from a series link.
    pub catalog_series: Option<String>,
    pub expanded: BTreeSet<i64>,
    pub fetching: bool,
    pub setup_open: bool,
    pub etp_draft: String,
    pub running: bool,
    pub run_handle: Option<Handle>,
    pub current: String,
    pub logs: VecDeque<logs::Entry>,
    /// Overall state of the running download, and the bars drawn for its tracks.
    pub overall: Overall,
    pub bars: Vec<Bar>,
    /// The overall bar eases toward `progress_target`. The target never moves back during one run.
    pub progress_shown: f32,
    pub progress_target: f32,
    pub last_frame: Option<Instant>,
    /// The open dropdown, and the one whose list is drawn (it stays drawn while closing).
    pub dropdown: Option<DropdownId>,
    pub dropdown_last: Option<DropdownId>,
    /// 0 → 1 while a list opens, 1 → 0 while it closes.
    pub dropdown_anim: Animation<f32>,
    /// The option the keyboard is on in the open dropdown.
    pub highlight: usize,
    /// Search text typed into an open language dropdown. Empty when no filter is active.
    pub dropdown_filter: String,
    /// ffmpeg status, from the last check.
    pub ffmpeg: FfmpegState,
    /// Progress of an in-flight ffmpeg download, if one is running or just finished.
    pub ffmpeg_dl: Option<FfmpegDlEvent>,
    /// A sign-in in progress, if any.
    pub login_busy: Option<LoginPhase>,
    /// Crunchyroll refused the saved login on the last fetch; the user must sign in again.
    pub login_expired: bool,
    /// Episodes of the current or last download, with their states.
    pub queue: Vec<QueueItem>,
    /// Folder the last download writes to, for the Open folder button.
    pub last_output: Option<PathBuf>,
    /// When the user last touched the app. The vault locks after a set time without input.
    pub last_activity: Instant,
}

/// One-shot slot that carries an unlock result. Cloneable so `Message` can be Clone,
/// and the vault is moved out exactly once.
pub type Slot = std::sync::Arc<std::sync::Mutex<Option<Result<Vault, String>>>>;

/// One-shot slot for the result of a fetch, shared the same way as `Slot`.
pub type CatalogSlot = std::sync::Arc<std::sync::Mutex<Option<Result<Catalog, String>>>>;

fn catalog_slot(r: Result<Catalog, String>) -> CatalogSlot {
    std::sync::Arc::new(std::sync::Mutex::new(Some(r)))
}

/// Id of the page's scrollable, so a tab change can return to the top.
pub fn page_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("page")
}

/// Id of the password field on the lock screen. It takes focus when the screen appears.
pub fn password_input_id() -> iced::widget::Id {
    iced::widget::Id::new("password")
}

fn focus_password<T>() -> Task<T> {
    iced::widget::operation::focus(password_input_id())
}

#[derive(Clone)]
pub enum Message {
    Tab(Tab),
    PwInput(String),
    Pw2Input(String),
    CreateVault,
    UnlockVault,
    Opened(Slot),
    Lock,
    OldPwInput(String),
    NewPwInput(String),
    NewPw2Input(String),
    ChangePassword,
    PasswordChanged(Slot),
    UrlInput(String),
    FetchPressed,
    Fetched(CatalogSlot),
    ToggleEpisode(String, bool),
    ToggleSeason(i64, bool),
    ToggleExpand(i64),
    SelectAll(bool),
    DownloadSelected,
    Stop,
    RunFinished(i32),
    CancelEpisode(String),
    RetryEpisode(String),
    RetryFailed,
    OpenOutput,
    /// Browse tab: search text typed (debounced), the debounce firing, Enter, clear.
    BrowseQueryInput(String),
    BrowseSearchFire(u64),
    BrowseSubmit,
    BrowseClearSearch,
    BrowseSort(engine::Sort),
    BrowseShow(engine::Show),
    BrowseMore,
    BrowseRetry,
    /// A Browse page arrived: (request number, client to reuse, result).
    BrowseLoaded(u64, Option<crunchyroll_downloader::api::Api>, Result<engine::BrowsePage, String>),
    PosterLoaded(String, Result<Vec<u8>, String>),
    /// Open a series' detail page (and load its seasons and episodes).
    OpenSeries(engine::CatalogItem),
    BrowseBack,
    /// The page was scrolled (remembers the Browse grid spot, loads more near the bottom).
    PageScrolled(iced::widget::scrollable::Viewport),
    /// Download one episode right away.
    DownloadOne(String),
    /// Download every episode of a season that is not already on disk.
    DownloadSeason(i64),
    /// Leftover temp files removed at startup: (count, bytes).
    TempCleaned(usize, u64),
    SetNotify(bool),
    SetPortable(bool),
    Tick,
    Frame(Instant),
    ToggleDropdown(DropdownId),
    CloseDropdown,
    /// Search text typed into an open language dropdown.
    DropdownFilter(String),
    Choose(DropdownId, String),
    KeyPressed(keyboard::Key),
    RemoveLang(LangField, String),
    ToggleSecret,
    SetDelay(String),
    SetDebug(bool),
    SetOutputDir(String),
    SetSegmentWorkers(String),
    SetTrackWorkers(String),
    SetProxyUrl(String),
    BrowseOutput,
    OutputPicked(Option<PathBuf>),
    PickFfmpeg,
    FfmpegPicked(Option<PathBuf>),
    UseAutoFfmpeg,
    FfmpegChecked(Result<PathBuf, String>),
    /// Start downloading and installing ffmpeg automatically.
    DownloadFfmpeg,
    /// A progress event from the running ffmpeg download.
    FfmpegDl(FfmpegDlEvent),
    OpenSetup,
    GoSetup,
    CloseSetup,
    EtpDraftInput(String),
    SaveEtp,
    /// Open the WebView2 login helper and capture etp_rt automatically.
    BrowserLogin,
    /// The login helper finished: Ok(etp_rt) or Err(reason). The cookie is zeroed on drop.
    BrowserLoginDone(Result<Zeroizing<String>, String>),
    /// Crunchyroll answered the check of a new cookie. Ok: it works. Err((rejected, why)).
    LoginVerified(Zeroizing<String>, Result<(), (bool, String)>),
    /// Forget the saved login.
    SignOut,
    PickClientId,
    ClientIdPicked(Option<Vec<u8>>),
    PickPem,
    PemPicked(Option<String>),
    PickWvd,
    WvdPicked(Option<Vec<u8>>),
    ClearDevice,
    ClearLogs,
    ExportLogs,
    Exported(Result<Option<String>, String>),
    DismissNotice,
    /// Any press, scroll or key press. Resets the idle timer.
    Activity,
    /// Periodic check for the auto-lock.
    IdleCheck,
}

impl App {
    pub fn boot() -> (Self, Task<Message>) {
        logs::install_sink();
        progress::set_enabled(false);
        let vault_path = vault::default_path().unwrap_or_else(|_| PathBuf::from("vault.bin"));
        let screen = if vault::exists(&vault_path) { Screen::Unlock } else { Screen::Create };
        let app = Self {
            screen,
            vault_path,
            vault: None,
            tab: Tab::Main,
            busy: false,
            notice: None,
            pw: String::new(),
            pw2: String::new(),
            old_pw: String::new(),
            new_pw: String::new(),
            new_pw2: String::new(),
            url: String::new(),
            show_secret: false,
            now: Instant::now(),
            tab_anim: Animation::new(1.0),
            nav_from: 0.0,
            nav_to: 0.0,
            nav_anim: Animation::new(1.0),
            setup_anim: Animation::new(0.0).duration(Duration::from_millis(340)).easing(Easing::EaseOutExpo),
            catalog: None,
            picked: BTreeSet::new(),
            downloaded: BTreeSet::new(),
            browse: BrowseState::default(),
            catalog_series: None,
            expanded: BTreeSet::new(),
            fetching: false,
            setup_open: false,
            etp_draft: String::new(),
            running: false,
            run_handle: None,
            current: "Idle".into(),
            logs: VecDeque::new(),
            overall: Overall::default(),
            bars: Vec::new(),
            progress_shown: 0.0,
            progress_target: 0.0,
            last_frame: None,
            dropdown: None,
            dropdown_last: None,
            dropdown_anim: Animation::new(0.0)
                .duration(Duration::from_millis(DROPDOWN_MS))
                .easing(Easing::EaseOutCubic),
            highlight: 0,
            dropdown_filter: String::new(),
            ffmpeg: FfmpegState::Unknown,
            ffmpeg_dl: None,
            login_busy: None,
            login_expired: false,
            queue: Vec::new(),
            last_output: None,
            last_activity: Instant::now(),
        };
        // Clear out temp files a crash or forced close left behind, in the background.
        let clean = Task::perform(
            async { tokio::task::spawn_blocking(clean_stale_temp).await.unwrap_or((0, 0)) },
            |(files, bytes)| Message::TempCleaned(files, bytes),
        );
        (app, Task::batch([focus_password(), clean]))
    }

    /// Current top of the navigation pill, at time `now`.
    pub fn nav_y(&self, now: Instant) -> f32 {
        let (from, to) = (self.nav_from, self.nav_to);
        self.nav_anim.interpolate_with(|v| from + (to - from) * v, now)
    }

    /// True while something is moving, so frames are drawn at display rate.
    fn animating(&self) -> bool {
        let now = Instant::now();
        self.tab_anim.is_animating(now)
            || self.setup_anim.is_animating(now)
            || self.nav_anim.is_animating(now)
            || (self.progress_shown - self.progress_target).abs() > PROGRESS_EPS
            || self.dropdown_anim.is_animating(now)
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let mut subs = Vec::new();
        if self.wants_tick() {
            // Progress is read on each tick, so a running download refreshes faster.
            let period = if self.running { 120 } else { 250 };
            subs.push(iced::time::every(Duration::from_millis(period)).map(|_| Message::Tick));
        }
        if self.animating() {
            subs.push(window::frames().map(Message::Frame));
        }
        if self.screen == Screen::Ready {
            subs.push(iced::event::listen_with(activity));
            subs.push(iced::time::every(Duration::from_secs(IDLE_CHECK_SECS)).map(|_| Message::IdleCheck));
        }
        if self.dropdown.is_some() {
            subs.push(iced::event::listen_with(dropdown_key));
        }
        Subscription::batch(subs)
    }

    /// Changes one setting and saves the vault at once.
    fn edit_settings(&mut self, change: impl FnOnce(&mut Settings)) {
        if self.busy {
            return;
        }
        let Some(v) = self.vault.as_mut() else { return };
        change(&mut v.contents.settings);
        if let Err(e) = v.save() {
            self.notice = Some((Kind::Fail, format!("Could not save settings: {e:#}")));
        }
    }

    /// Sends the current etp_rt value to the log filter so it can never appear there.
    fn refresh_secrets(&self) {
        let values = match &self.vault {
            Some(v) if !v.contents.secrets.etp_rt.is_empty() => vec![v.contents.secrets.etp_rt.clone()],
            _ => Vec::new(),
        };
        logs::set_secrets(values);
    }

    fn clear_password_fields(&mut self) {
        for field in [&mut self.pw, &mut self.pw2, &mut self.old_pw, &mut self.new_pw, &mut self.new_pw2] {
            field.zeroize();
        }
    }

    /// Moves queued log lines into the window's log.
    fn drain_logs(&mut self) {
        for entry in logs::take_pending() {
            if entry.kind == Kind::Title {
                self.current = entry.text.clone();
            }
            self.logs.push_back(entry);
        }
        while self.logs.len() > LOG_CAP {
            self.logs.pop_front();
        }
    }

    /// Minutes of inactivity before the vault locks. 0 means never. A bad value counts as never.
    fn auto_lock_minutes(&self) -> u64 {
        self.vault
            .as_ref()
            .and_then(|v| v.contents.settings.auto_lock_minutes.trim().parse::<u64>().ok())
            .unwrap_or(0)
    }

    /// Re-encrypts the vault under the new password in the background.
    fn change_password(&mut self) -> Task<Message> {
        if self.busy {
            return Task::none();
        }
        let Some(v) = self.vault.as_ref() else { return Task::none() };
        if self.new_pw != self.new_pw2 {
            self.notice = Some((Kind::Warn, "The new passwords do not match.".into()));
            return Task::none();
        }
        if let Err(e) = vault::check_password(&self.new_pw) {
            self.notice = Some((Kind::Warn, format!("{e:#}")));
            return Task::none();
        }
        self.busy = true;
        self.notice = None;
        let path = v.path().to_path_buf();
        let old = Zeroizing::new(self.old_pw.clone());
        let new = Zeroizing::new(self.new_pw.clone());
        Task::perform(
            async move {
                let r = tokio::task::spawn_blocking(move || -> Result<Vault, String> {
                    let mut fresh = Vault::open(&path, &old).map_err(|e| format!("{e:#}"))?;
                    fresh.change_password(&new).map_err(|e| format!("{e:#}"))?;
                    Ok(fresh)
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
                slot(r)
            },
            Message::PasswordChanged,
        )
    }

    /// How open the dropdown list is, from 0 (closed) to 1 (open).
    pub fn dropdown_t(&self) -> f32 {
        self.dropdown_anim.interpolate_with(|v| v, self.now)
    }

    /// Checks a new etp_rt with Crunchyroll before it is stored. The result arrives as
    /// `LoginVerified`, carrying the cookie back so only a working one is saved.
    fn verify_login(&mut self, etp: Zeroizing<String>) -> Task<Message> {
        self.login_busy = Some(LoginPhase::Verifying);
        ui::info("Checking the login with Crunchyroll");
        let proxy = self.vault.as_ref().map(|v| v.contents.settings.proxy_url.clone()).unwrap_or_default();
        let check = etp.clone();
        Task::perform(
            async move {
                let result = engine::verify_login(check, proxy).await;
                (etp, result)
            },
            |(etp, result)| Message::LoginVerified(etp, result),
        )
    }

    /// Saves a login into the encrypted vault and clears any leftover draft and expiry flag.
    fn store_login(&mut self, etp: &str) -> Result<(), String> {
        let Some(v) = self.vault.as_mut() else { return Err("The vault is locked.".into()) };
        v.contents.secrets.etp_rt = etp.trim().to_string();
        v.save().map_err(|e| format!("{e:#}"))?;
        self.etp_draft.zeroize();
        self.etp_draft.clear();
        self.login_expired = false;
        self.refresh_secrets();
        Ok(())
    }

    /// Starts closing the open dropdown. Its list stays drawn until the animation ends.
    fn close_dropdown(&mut self) {
        self.dropdown = None;
        self.dropdown_filter.clear();
        self.dropdown_anim.go_mut(0.0, Instant::now());
    }

    /// Reads the run's progress for the window. The target only moves forward during a run.
    fn refresh_progress(&mut self) {
        self.overall = progress::overall();
        self.bars = progress::snapshot();
        let fraction = self.overall.fraction(&self.bars);
        self.progress_target = self.progress_target.max(fraction);
    }

    /// Copies each episode's state from the run into the queue.
    fn sync_queue(&mut self) {
        for (id, state) in progress::run_states() {
            if let Some(item) = self.queue.iter_mut().find(|q| q.ep.id == id) {
                item.state = state;
            }
        }
    }

    /// Marks episodes that never finished (because the run was stopped) as cancelled.
    fn finish_unfinished(&mut self) {
        for item in &mut self.queue {
            if matches!(item.state, EpisodeState::Waiting | EpisodeState::Downloading) {
                item.state = EpisodeState::Cancelled;
            }
        }
    }

    /// Moves the shown progress toward its target. The time constant is 0.12 s, so the
    /// motion looks the same at any frame rate.
    fn ease_progress(&mut self, dt: Duration) {
        let gap = self.progress_target - self.progress_shown;
        if gap.abs() <= PROGRESS_EPS {
            self.progress_shown = self.progress_target;
            return;
        }
        let k = 1.0 - (-dt.as_secs_f32() / 0.12).exp();
        self.progress_shown += gap * k;
    }

    /// Whether the window needs the periodic tick: only while work runs or the log is shown.
    fn wants_tick(&self) -> bool {
        self.running || self.fetching || self.tab == Tab::Logs
    }

    fn lock(&mut self) {
        if let Some(handle) = self.run_handle.take() {
            handle.abort();
            progress::end_episode();
        }
        self.running = false;
        self.vault = None;
        self.setup_open = false;
        self.etp_draft.zeroize();
        self.clear_password_fields();
        logs::set_secrets(Vec::new());
        self.screen = Screen::Unlock;
        self.tab = Tab::Main;
        self.notice = None;
        self.catalog = None;
        self.catalog_series = None;
        self.picked.clear();
        self.expanded.clear();
        self.fetching = false;
        self.dropdown = None;
        self.dropdown_last = None;
        self.queue.clear();
        // The cached Browse session holds a live access token: drop it when locking.
        self.browse = BrowseState::default();
        self.last_output = None;
        self.ffmpeg = FfmpegState::Unknown;
        self.highlight = 0;
    }

    /// Loads the Browse grid: the next page of the catalog (`append`), or a fresh first page or
    /// search for the current text. Needs a saved login; without one the tab shows a prompt.
    fn browse_load(&mut self, append: bool) -> Task<Message> {
        let Some(v) = self.vault.as_ref() else { return Task::none() };
        let etp = v.contents.secrets.etp_rt.clone();
        if etp.is_empty() {
            return Task::none();
        }
        let st = &v.contents.settings;
        let locale = preferred_request_locale(&parse_langs(&st.subs_langs), "en-US");
        let text = self.browse.query.trim().to_string();
        let query = if text.is_empty() {
            let start = if append { self.browse.items.len() } else { 0 };
            engine::BrowseQuery::Page { sort: self.browse.sort, show: self.browse.show, genre: self.browse.genre.clone(), start }
        } else {
            engine::BrowseQuery::Search(text)
        };
        if !append {
            self.browse.seq += 1;
        }
        let seq = self.browse.seq;
        self.browse.loading = true;
        self.browse.error = None;
        let want_genres = !self.browse.genres_asked;
        self.browse.genres_asked = true;
        let input = engine::BrowseInput {
            api: self.browse.api.clone(),
            etp: Zeroizing::new(etp),
            proxy_url: st.proxy_url.clone(),
            locale,
            query,
            want_genres,
        };
        Task::perform(engine::browse(input), move |(api, result)| Message::BrowseLoaded(seq, api, result))
    }

    /// Starts loading every poster on screen that isn't cached or loading yet.
    fn load_posters(&mut self) -> Task<Message> {
        let mut wanted: Vec<String> = self.browse.items.iter().filter_map(|i| i.poster.clone()).collect();
        if let Some(sel) = &self.browse.selected {
            wanted.extend(sel.poster_large.clone().or_else(|| sel.poster.clone()));
        }
        let mut tasks = Vec::new();
        for url in wanted {
            if self.browse.posters.contains_key(&url) {
                continue;
            }
            self.browse.posters.insert(url.clone(), Poster::Loading);
            let key = url.clone();
            tasks.push(Task::perform(engine::poster(url), move |r| Message::PosterLoaded(key.clone(), r)));
        }
        Task::batch(tasks)
    }

    /// Recomputes which episodes of the current list already have their finished file in the
    /// output folder, using the same naming rule the downloader uses.
    fn refresh_downloaded(&mut self) {
        let mut found = BTreeSet::new();
        if let (Some(catalog), Some(v)) = (&self.catalog, &self.vault) {
            let st = &v.contents.settings;
            let dir = resolve_output(st);
            for ep in catalog.seasons.iter().flat_map(|g| g.episodes.iter()) {
                let file = crunchyroll_downloader::episode::output_path(
                    &dir,
                    &ep.series_title,
                    ep.season_number,
                    ep.episode_number,
                    &ep.title,
                    &st.video_quality,
                    ep.is_movie,
                );
                if file.is_file() {
                    found.insert(ep.id.clone());
                }
            }
        }
        self.downloaded = found;
    }

    fn open_vault(&mut self, mut vault: Vault) {
        // Older vaults saved "." (wherever the program ran from) as the output folder, which
        // scattered videos into the build folder. Move them to Downloads\Crunchyroll.
        if vault::output_is_unset(&vault.contents.settings.output_dir) {
            vault.contents.settings.output_dir = vault::default_output_dir().display().to_string();
            let _ = vault.save();
        }
        // The last link is restored, so the next fetch is one click away.
        self.url = vault.contents.settings.last_url.clone();
        self.vault = Some(vault);
        self.screen = Screen::Ready;
        self.clear_password_fields();
        self.refresh_secrets();
        self.notice = None;
        self.last_activity = Instant::now();
        ui::info("Vault unlocked");
    }

    /// Looks for ffmpeg, using the path from Settings when one is set, and verifies it runs.
    /// The check runs in the background, so the window stays responsive.
    fn check_ffmpeg(&mut self) -> Task<Message> {
        let Some(v) = self.vault.as_ref() else { return Task::none() };
        let custom = v.contents.settings.ffmpeg_path.clone();
        self.ffmpeg = FfmpegState::Checking;
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || -> Result<PathBuf, String> {
                    let path = ffmpeg::locate_with(&custom).map_err(|e| format!("{e:#}"))?;
                    ffmpeg::verify(&path).map_err(|e| format!("{e:#}"))?;
                    Ok(path)
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            },
            Message::FfmpegChecked,
        )
    }

    /// Handles one message, then moves any new log lines into the window. This way the last
    /// lines of a run appear even though the periodic tick has already stopped.
    pub fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.apply(message);
        self.drain_logs();
        task
    }

    fn apply(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tab(t) => {
                if t != self.tab {
                    let now = Instant::now();
                    self.now = now;
                    let current = self.nav_y(now);
                    self.nav_from = current;
                    self.nav_to = tab_y(t);
                    // One animation per moving part. The pill uses a smooth ease-out with no overshoot.
                    self.nav_anim = Animation::new(0.0)
                        .duration(Duration::from_millis(340))
                        .easing(Easing::EaseOutQuart)
                        .go(1.0, now);
                    self.tab_anim = Animation::new(0.0)
                        .duration(Duration::from_millis(280))
                        .easing(Easing::EaseOutCubic)
                        .go(1.0, now);
                }
                self.tab = t;
                // A list does not stay open when the user leaves the page.
                self.close_dropdown();
                self.drain_logs();
                // Every page starts at the top, so a stale scroll position never hides the page.
                let top = iced::widget::operation::scroll_to(
                    page_scroll_id(),
                    iced::widget::scrollable::AbsoluteOffset { x: Some(0.0), y: Some(0.0) },
                );
                // The first visit to Browse fills the grid.
                if t == Tab::Browse && self.browse.items.is_empty() && !self.browse.loading {
                    return Task::batch([top, self.browse_load(false)]);
                }
                return top;
            }
            Message::Frame(at) => {
                let dt = self
                    .last_frame
                    .map_or(Duration::from_millis(16), |last| at.saturating_duration_since(last))
                    .min(Duration::from_millis(100));
                self.last_frame = Some(at);
                self.now = at;
                self.ease_progress(dt);
                // Once a list has finished closing, it is no longer drawn.
                if self.dropdown.is_none() && !self.dropdown_anim.is_animating(at) {
                    self.dropdown_last = None;
                }
            }
            Message::Activity => self.last_activity = Instant::now(),
            Message::IdleCheck => {
                let minutes = self.auto_lock_minutes();
                // A download in progress keeps the vault open, since it needs the key.
                if self.screen == Screen::Ready && !self.running && idle_expired(self.last_activity, Instant::now(), minutes) {
                    self.lock();
                    self.notice = Some((Kind::Info, format!("Locked after {minutes} minute(s) without activity.")));
                    return focus_password();
                }
            }
            Message::PwInput(s) => self.pw = s,
            Message::Pw2Input(s) => self.pw2 = s,
            Message::CreateVault => {
                if self.pw.chars().count() < MIN_PASSWORD_LEN {
                    self.notice = Some((Kind::Warn, format!("The password must be at least {MIN_PASSWORD_LEN} characters.")));
                    return Task::none();
                }
                if self.pw != self.pw2 {
                    self.notice = Some((Kind::Warn, "The two passwords do not match.".into()));
                    return Task::none();
                }
                if self.busy {
                    return Task::none();
                }
                self.busy = true;
                let path = self.vault_path.clone();
                let pw = Zeroizing::new(self.pw.clone());
                return Task::perform(
                    async move {
                        let r = tokio::task::spawn_blocking(move || Vault::create(&path, &pw).map_err(|e| format!("{e:#}")))
                            .await
                            .unwrap_or_else(|e| Err(e.to_string()));
                        slot(r)
                    },
                    Message::Opened,
                );
            }
            Message::UnlockVault => {
                // Enter and the button both come here. Ignore a second press while the key is derived.
                if self.pw.is_empty() || self.busy {
                    return Task::none();
                }
                self.busy = true;
                let path = self.vault_path.clone();
                let pw = Zeroizing::new(self.pw.clone());
                return Task::perform(
                    async move {
                        let r = tokio::task::spawn_blocking(move || Vault::open(&path, &pw).map_err(|e| format!("{e:#}")))
                            .await
                            .unwrap_or_else(|e| Err(e.to_string()));
                        slot(r)
                    },
                    Message::Opened,
                );
            }
            Message::Opened(cell) => {
                self.busy = false;
                let result = cell.lock().ok().and_then(|mut g| g.take());
                let Some(result) = result else { return Task::none() };
                match result {
                    Ok(vault) => {
                        self.open_vault(vault);
                        return self.check_ffmpeg();
                    }
                    Err(e) => {
                        self.pw.zeroize();
                        self.notice = Some((Kind::Fail, e));
                        return focus_password();
                    }
                }
            }
            Message::Lock => {
                ui::info("Vault locked");
                self.lock();
                return focus_password();
            }
            Message::OldPwInput(s) => self.old_pw = s,
            Message::NewPwInput(s) => self.new_pw = s,
            Message::NewPw2Input(s) => self.new_pw2 = s,
            Message::ChangePassword => return self.change_password(),
            Message::PasswordChanged(cell) => {
                self.busy = false;
                let result = cell.lock().ok().and_then(|mut g| g.take());
                // The vault was locked while this ran. Drop the result, never reopen it.
                if self.screen != Screen::Ready {
                    return Task::none();
                }
                match result {
                    Some(Ok(new_vault)) => {
                        // Replacing the vault drops the old key, which is zeroed.
                        self.vault = Some(new_vault);
                        self.clear_password_fields();
                        self.notice = Some((Kind::Ok, "Password changed. Everything was re-encrypted.".into()));
                        ui::ok("Vault password changed");
                    }
                    Some(Err(e)) => self.notice = Some((Kind::Fail, e)),
                    None => {}
                }
            }
            Message::UrlInput(s) => self.url = s,
            Message::FetchPressed => return self.fetch(),
            Message::Fetched(cell) => {
                self.fetching = false;
                let result = cell.lock().ok().and_then(|mut g| g.take());
                // Locked while fetching: ignore the list rather than show it on the lock screen.
                if self.screen != Screen::Ready {
                    return Task::none();
                }
                match result {
                    Some(Ok(catalog)) => {
                        let seasons = catalog.seasons.len();
                        let episodes: usize = catalog.seasons.iter().map(|g| g.episodes.len()).sum();
                        if seasons == 1 {
                            self.expanded.insert(catalog.seasons[0].number);
                        }
                        // Episodes picked in the last download are picked again, when they are in
                        // this list — except ones that are already on disk.
                        let saved: Vec<String> = self
                            .vault
                            .as_ref()
                            .map(|v| v.contents.settings.last_picks.clone())
                            .unwrap_or_default();
                        self.catalog = Some(catalog);
                        self.catalog_series = parse_url(&self.url)
                            .filter(|(kind, _)| matches!(*kind, "series" | "movie_listing"))
                            .map(|(_, id)| id.to_string());
                        self.refresh_downloaded();
                        let catalog = self.catalog.as_ref().expect("just set");
                        self.picked = catalog
                            .seasons
                            .iter()
                            .flat_map(|g| g.episodes.iter())
                            .filter(|e| saved.contains(&e.id) && !self.downloaded.contains(&e.id))
                            .map(|e| e.id.clone())
                            .collect();
                        let have = self.downloaded.len();
                        let note = if have > 0 { format!(" {have} already downloaded.") } else { String::new() };
                        self.notice = Some((
                            Kind::Ok,
                            format!("{} · {seasons} season(s), {episodes} episode(s).{note} Pick what to download.", catalog.title),
                        ));
                        // A successful fetch proves the login works.
                        self.login_expired = false;
                    }
                    Some(Err(e)) if engine::is_login_expired(&e) => {
                        // Show a clear "log in again" prompt instead of a raw API error.
                        self.login_expired = true;
                        self.notice = Some((Kind::Warn, "Your Crunchyroll login has expired. Press Log in to sign in again.".into()));
                    }
                    Some(Err(e)) => self.notice = Some((Kind::Fail, e)),
                    None => {}
                }
            }
            Message::ToggleEpisode(id, on) => {
                if on {
                    self.picked.insert(id);
                } else {
                    self.picked.remove(&id);
                }
            }
            Message::ToggleSeason(number, on) => {
                let ids: Vec<String> = self
                    .catalog
                    .as_ref()
                    .and_then(|c| c.seasons.iter().find(|g| g.number == number))
                    .map(|g| g.episodes.iter().map(|e| e.id.clone()).collect())
                    .unwrap_or_default();
                for id in ids {
                    if on {
                        self.picked.insert(id);
                    } else {
                        self.picked.remove(&id);
                    }
                }
            }
            Message::ToggleExpand(number) => {
                if !self.expanded.remove(&number) {
                    self.expanded.insert(number);
                }
            }
            Message::SelectAll(on) => {
                let ids: Vec<String> = self
                    .catalog
                    .as_ref()
                    .map(|c| c.seasons.iter().flat_map(|g| g.episodes.iter().map(|e| e.id.clone())).collect())
                    .unwrap_or_default();
                if on {
                    // Already-downloaded episodes stay unticked; they can still be ticked by hand.
                    self.picked.extend(ids.into_iter().filter(|id| !self.downloaded.contains(id)));
                } else {
                    self.picked.clear();
                }
            }
            Message::DownloadSelected => return self.download_selected(),
            Message::Stop => {
                if let Some(handle) = self.run_handle.take() {
                    handle.abort();
                }
                progress::end_episode();
                self.running = false;
                self.finish_unfinished();
                self.last_activity = Instant::now();
                self.current = "Stopped".into();
                self.notice = Some((Kind::Warn, "Stopped. Episodes that finished are kept.".into()));
                ui::warn("Stopped by you. Unfinished files are kept as *.part.mkv and removed on the next run.");
            }
            Message::RunFinished(_code) => {
                self.sync_queue();
                self.running = false;
                self.run_handle = None;
                self.finish_unfinished();
                self.refresh_progress();
                self.last_activity = Instant::now();
                let (done, failed, skipped) = tally(&self.queue);
                let text = format!("Finished: {done} downloaded, {failed} failed, {skipped} skipped.");
                if failed == 0 {
                    self.progress_target = 1.0;
                    self.current = "Finished".into();
                    self.notice = Some((Kind::Ok, text.clone()));
                } else {
                    self.current = "Finished with errors".into();
                    self.notice = Some((Kind::Warn, text.clone()));
                }
                // New files are on disk now: mark them in the episode list.
                self.refresh_downloaded();
                // Flash the taskbar, so a finished download is noticed when the window is in the background.
                let flash = window::latest().and_then(|id| {
                    window::request_user_attention(id, Some(window::UserAttention::Informational))
                });
                let wants_toast = self.vault.as_ref().is_some_and(|v| v.contents.settings.notify_done);
                if !wants_toast || self.queue.is_empty() {
                    return flash;
                }
                let title = if failed == 0 { "Downloads finished" } else { "Downloads finished with errors" };
                let toast = Task::perform(
                    async move {
                        let _ = tokio::task::spawn_blocking(move || show_toast(title, &text)).await;
                    },
                    |()| Message::Tick,
                );
                return Task::batch([flash, toast]);
            }
            Message::CancelEpisode(id) => {
                progress::cancel_episode(&id);
                if let Some(item) = self.queue.iter_mut().find(|q| q.ep.id == id) {
                    if item.state == EpisodeState::Waiting {
                        item.state = EpisodeState::Cancelled;
                    }
                }
            }
            Message::RetryEpisode(id) => {
                let episodes: Vec<SeasonEpisode> = self.queue.iter().filter(|q| q.ep.id == id).map(|q| q.ep.clone()).collect();
                return self.start_run(episodes, false);
            }
            Message::RetryFailed => {
                let episodes: Vec<SeasonEpisode> = self
                    .queue
                    .iter()
                    .filter(|q| matches!(q.state, EpisodeState::Failed(_) | EpisodeState::Cancelled))
                    .map(|q| q.ep.clone())
                    .collect();
                return self.start_run(episodes, false);
            }
            Message::OpenOutput => {
                // The last run's folder, or the configured one when nothing ran yet.
                let path = self.last_output.clone().or_else(|| self.vault.as_ref().map(|v| resolve_output(&v.contents.settings)));
                if let Some(path) = path {
                    let _ = std::fs::create_dir_all(&path);
                    if let Err(e) = open_folder(&path) {
                        self.notice = Some((Kind::Fail, format!("Could not open {}: {e}", path.display())));
                    }
                }
            }
            Message::BrowseQueryInput(text) => {
                self.browse.query = text;
                // Wait for a short pause in typing before searching.
                self.browse.seq += 1;
                let seq = self.browse.seq;
                return Task::perform(tokio::time::sleep(Duration::from_millis(400)), move |()| Message::BrowseSearchFire(seq));
            }
            Message::BrowseSearchFire(seq) => {
                if seq == self.browse.seq {
                    return self.browse_load(false);
                }
            }
            Message::BrowseSubmit | Message::BrowseRetry => return self.browse_load(false),
            Message::BrowseClearSearch => {
                self.browse.query.clear();
                return self.browse_load(false);
            }
            Message::BrowseSort(sort) => {
                self.browse.sort = sort;
                self.browse.query.clear();
                return self.browse_load(false);
            }
            Message::BrowseShow(show) => {
                self.browse.show = show;
                self.browse.query.clear();
                return self.browse_load(false);
            }
            Message::BrowseMore => {
                if !self.browse.loading {
                    return self.browse_load(true);
                }
            }
            Message::BrowseLoaded(seq, api, result) => {
                if seq != self.browse.seq {
                    return Task::none(); // an older request; a newer one is on its way
                }
                self.browse.loading = false;
                match result {
                    Ok(page) => {
                        self.browse.api = api;
                        self.browse.error = None;
                        if page.append {
                            for item in page.items {
                                if !self.browse.items.iter().any(|i| i.id == item.id) {
                                    self.browse.items.push(item);
                                }
                            }
                        } else {
                            self.browse.items = page.items;
                        }
                        self.browse.total = page.total;
                        if let Some(genres) = page.genres {
                            self.browse.genres = genres;
                        }
                        return self.load_posters();
                    }
                    Err(e) => {
                        // Sign in afresh next time; the cached session may be what failed.
                        self.browse.api = None;
                        // Try the genre list again with the next request.
                        if self.browse.genres.is_empty() {
                            self.browse.genres_asked = false;
                        }
                        if engine::is_login_expired(&e) {
                            self.login_expired = true;
                            self.browse.error = Some("Your Crunchyroll login has expired. Log in again to browse.".into());
                        } else {
                            self.browse.error = Some(e);
                        }
                    }
                }
            }
            Message::PosterLoaded(url, result) => {
                let poster = match result {
                    Ok(bytes) => Poster::Ready(iced::widget::image::Handle::from_bytes(bytes)),
                    Err(_) => Poster::Failed,
                };
                self.browse.posters.insert(url, poster);
            }
            Message::OpenSeries(item) => {
                self.url = item.link();
                let already = self.catalog.is_some() && self.catalog_series.as_deref() == Some(item.id.as_str());
                self.browse.selected = Some(item);
                let posters = self.load_posters();
                let top = iced::widget::operation::scroll_to(
                    page_scroll_id(),
                    iced::widget::scrollable::AbsoluteOffset { x: Some(0.0), y: Some(0.0) },
                );
                if already || self.running {
                    // Already loaded, or a download is running (the list can't change mid-run).
                    return Task::batch([posters, top]);
                }
                return Task::batch([self.fetch(), posters, top]);
            }
            Message::BrowseBack => {
                self.browse.selected = None;
                return iced::widget::operation::scroll_to(
                    page_scroll_id(),
                    iced::widget::scrollable::AbsoluteOffset { x: Some(0.0), y: Some(self.browse.grid_scroll_y) },
                );
            }
            Message::PageScrolled(viewport) => {
                // Only the grid counts; the detail page and other tabs scroll on their own.
                if self.tab == Tab::Browse && self.browse.selected.is_none() {
                    let y = viewport.absolute_offset().y;
                    self.browse.grid_scroll_y = y;
                    let left = viewport.content_bounds().height - viewport.bounds().height - y;
                    if left < 600.0 && self.browse.has_more() && !self.browse.loading {
                        return self.browse_load(true);
                    }
                }
            }
            Message::DownloadOne(id) => {
                let episode = self
                    .catalog
                    .as_ref()
                    .and_then(|c| c.seasons.iter().flat_map(|g| g.episodes.iter()).find(|e| e.id == id).cloned());
                if let Some(ep) = episode {
                    return self.start_run(vec![ep], true);
                }
            }
            Message::DownloadSeason(number) => {
                let season: Vec<SeasonEpisode> = self
                    .catalog
                    .as_ref()
                    .and_then(|c| c.seasons.iter().find(|g| g.number == number))
                    .map(|g| g.episodes.clone())
                    .unwrap_or_default();
                // Skip what is already on disk; if that is everything, let the downloader say so.
                let missing: Vec<SeasonEpisode> = season.iter().filter(|e| !self.downloaded.contains(&e.id)).cloned().collect();
                let episodes = if missing.is_empty() { season } else { missing };
                if !episodes.is_empty() {
                    return self.start_run(episodes, true);
                }
            }
            Message::TempCleaned(files, bytes) => {
                if files > 0 {
                    ui::info(format!(
                        "Cleaned up {files} leftover temp file(s) from an interrupted run ({:.1} MB freed)",
                        bytes as f64 / 1_048_576.0
                    ));
                }
            }
            Message::SetNotify(on) => self.edit_settings(|st| st.notify_done = on),
            Message::SetPortable(on) => {
                let Some(v) = self.vault.as_mut() else { return Task::none() };
                match vault::set_portable(v, on) {
                    Ok(path) => {
                        self.vault_path = path.clone();
                        let text = if on {
                            format!("Portable mode on. The vault now lives next to the program: {}", path.display())
                        } else {
                            format!("Portable mode off. The vault moved back to {}", path.display())
                        };
                        ui::ok(&text);
                        self.notice = Some((Kind::Ok, text));
                    }
                    Err(e) => self.notice = Some((Kind::Fail, format!("Could not switch portable mode: {e:#}"))),
                }
            }
            Message::Tick => {
                self.drain_logs();
                self.refresh_progress();
                if self.running {
                    self.sync_queue();
                }
            }
            Message::ToggleDropdown(id) => {
                if self.dropdown == Some(id) {
                    self.close_dropdown();
                } else {
                    // Opening starts from closed, so switching between lists animates too.
                    // The keyboard starts on the current value. A fresh open clears any filter.
                    self.dropdown_filter.clear();
                    self.highlight = views::highlight_for_open(self, id);
                    self.dropdown = Some(id);
                    self.dropdown_last = Some(id);
                    self.dropdown_anim = Animation::new(0.0)
                        .duration(Duration::from_millis(DROPDOWN_MS))
                        .easing(Easing::EaseOutCubic)
                        .go(1.0, Instant::now());
                }
            }
            Message::CloseDropdown => {
                if self.dropdown.is_some() {
                    self.close_dropdown();
                }
            }
            Message::DropdownFilter(text) => {
                self.dropdown_filter = text;
                // The filtered list changes length, so start the keyboard at the top.
                self.highlight = 0;
            }
            Message::KeyPressed(key) => return self.dropdown_key(key),
            Message::Choose(id, value) => {
                match id {
                    DropdownId::Video => {
                        self.edit_settings(|st| st.video_quality = value);
                        // The quality is part of the file name, so "already downloaded" changes.
                        self.refresh_downloaded();
                    }
                    DropdownId::Audio => self.edit_settings(|st| st.audio_quality = value),
                    DropdownId::Lang(field) => self.edit_settings(|st| {
                        let slot = lang_slot(st, field);
                        let next = add_lang(slot.as_str(), &value);
                        *slot = next;
                    }),
                    DropdownId::AutoLock => self.edit_settings(|st| st.auto_lock_minutes = value),
                    DropdownId::Genre => {
                        self.browse.genre = (!value.is_empty()).then_some(value);
                        self.browse.query.clear();
                        self.close_dropdown();
                        return self.browse_load(false);
                    }
                }
                // Quality lists close after a pick. Language lists stay open for several picks.
                if !matches!(id, DropdownId::Lang(_)) {
                    self.close_dropdown();
                }
            }
            Message::RemoveLang(field, code) => {
                self.edit_settings(|st| {
                    let slot = lang_slot(st, field);
                    let next = remove_lang(slot.as_str(), &code);
                    *slot = next;
                });
            }
            Message::ToggleSecret => self.show_secret = !self.show_secret,
            Message::SetDelay(s) => self.edit_settings(|st| st.download_delay = s),
            Message::SetDebug(b) => self.edit_settings(|st| st.debug = b),
            Message::SetOutputDir(s) => {
                self.edit_settings(|st| st.output_dir = s);
                self.refresh_downloaded();
            }
            Message::SetSegmentWorkers(s) => self.edit_settings(|st| st.segment_workers = s),
            Message::SetTrackWorkers(s) => self.edit_settings(|st| st.track_workers = s),
            Message::SetProxyUrl(s) => self.edit_settings(|st| st.proxy_url = s),
            Message::BrowseOutput => {
                return Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .set_title("Choose the download folder")
                            .pick_folder()
                            .await
                            .map(|h| h.path().to_path_buf())
                    },
                    Message::OutputPicked,
                );
            }
            Message::OutputPicked(Some(path)) => {
                let text = path.display().to_string();
                self.edit_settings(|st| st.output_dir = text);
                self.refresh_downloaded();
            }
            Message::OutputPicked(None) => {}
            Message::PickFfmpeg => {
                return Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .set_title("Choose ffmpeg (ffmpeg.exe)")
                            .pick_file()
                            .await
                            .map(|h| h.path().to_path_buf())
                    },
                    Message::FfmpegPicked,
                );
            }
            Message::FfmpegPicked(Some(path)) => {
                let text = path.display().to_string();
                self.edit_settings(|st| st.ffmpeg_path = text);
                return self.check_ffmpeg();
            }
            Message::FfmpegPicked(None) => {}
            Message::UseAutoFfmpeg => {
                self.edit_settings(|st| st.ffmpeg_path.clear());
                return self.check_ffmpeg();
            }
            Message::FfmpegChecked(result) => {
                // A result that arrives after the vault locked is dropped.
                if self.screen == Screen::Ready {
                    self.ffmpeg = match result {
                        Ok(path) => FfmpegState::Ready(path),
                        Err(e) => FfmpegState::Broken(e),
                    };
                }
            }
            Message::DownloadFfmpeg => {
                if matches!(self.ffmpeg_dl, Some(FfmpegDlEvent::Progress(_)) | Some(FfmpegDlEvent::Extracting)) {
                    return Task::none(); // already running
                }
                self.ffmpeg_dl = Some(FfmpegDlEvent::Progress(0.0));
                ui::info("Downloading ffmpeg...");
                return Task::run(download_ffmpeg_stream(), Message::FfmpegDl);
            }
            Message::FfmpegDl(event) => match event {
                FfmpegDlEvent::Done(Ok(path)) => {
                    self.ffmpeg_dl = None;
                    ui::ok(format!("ffmpeg installed at {}", path.display()));
                    // Portable: leave the path empty so the search finds it next to the program
                    // on any drive letter. Otherwise remember the exact path.
                    let text = if vault::is_portable() { String::new() } else { path.display().to_string() };
                    self.edit_settings(|st| st.ffmpeg_path = text);
                    return self.check_ffmpeg();
                }
                FfmpegDlEvent::Done(Err(e)) => {
                    ui::warn(&e);
                    self.ffmpeg_dl = Some(FfmpegDlEvent::Done(Err(e)));
                }
                other => self.ffmpeg_dl = Some(other),
            },
            Message::OpenSetup => {
                self.setup_open = true;
                self.setup_anim.go_mut(1.0, Instant::now());
            }
            Message::GoSetup => {
                let switch = self.update(Message::Tab(Tab::Settings));
                self.setup_open = true;
                self.setup_anim.go_mut(1.0, Instant::now());
                return switch;
            }
            Message::CloseSetup => {
                self.setup_open = false;
                self.setup_anim.go_mut(0.0, Instant::now());
                self.etp_draft.zeroize();
            }
            Message::EtpDraftInput(s) => self.etp_draft = s,
            Message::SaveEtp => {
                // A pasted cookie goes through the same check as one from the login window.
                let value = Zeroizing::new(self.etp_draft.trim().to_string());
                if value.is_empty() {
                    self.notice = Some((Kind::Warn, "Paste the etp_rt value first.".into()));
                    return Task::none();
                }
                if self.login_busy.is_some() {
                    return Task::none();
                }
                return self.verify_login(value);
            }
            Message::BrowserLogin => {
                if self.login_busy.is_some() {
                    return Task::none(); // a login window is already open
                }
                self.login_busy = Some(LoginPhase::Waiting);
                self.notice = Some((Kind::Info, "Sign in in the window that just opened. It closes by itself when you're done.".into()));
                ui::info("Opening the Crunchyroll login window");
                return Task::perform(run_login_helper(), Message::BrowserLoginDone);
            }
            Message::BrowserLoginDone(result) => match result {
                Ok(etp) => return self.verify_login(etp),
                Err(e) => {
                    self.login_busy = None;
                    self.notice = Some((Kind::Warn, e.clone()));
                    ui::warn(&e);
                }
            },
            Message::LoginVerified(etp, result) => {
                self.login_busy = None;
                match result {
                    Ok(()) => match self.store_login(&etp) {
                        Ok(()) => {
                            self.notice = Some((Kind::Ok, "Signed in to Crunchyroll. Your login is saved encrypted.".into()));
                            ui::ok("Signed in: the login works and is saved in the vault");
                        }
                        Err(e) => self.notice = Some((Kind::Fail, e)),
                    },
                    Err((true, why)) => {
                        // Crunchyroll refused it: never store a cookie that cannot work.
                        self.notice = Some((
                            Kind::Fail,
                            "Crunchyroll did not accept that login. Please sign in again.".into(),
                        ));
                        ui::warn(&format!("Login rejected, not saved: {why}"));
                    }
                    Err((false, why)) => {
                        // Could not reach Crunchyroll to check. The cookie itself may be fine.
                        match self.store_login(&etp) {
                            Ok(()) => {
                                self.notice = Some((
                                    Kind::Warn,
                                    format!("Login saved, but it could not be checked right now ({why}). It will be checked on your next fetch."),
                                ));
                                ui::warn(&format!("Login saved without a check: {why}"));
                            }
                            Err(e) => self.notice = Some((Kind::Fail, e)),
                        }
                    }
                }
            }
            Message::SignOut => {
                let Some(v) = self.vault.as_mut() else { return Task::none() };
                v.contents.secrets.etp_rt.zeroize();
                v.contents.secrets.etp_rt.clear();
                match v.save() {
                    Ok(()) => {
                        self.login_expired = false;
                        self.notice = Some((Kind::Info, "Signed out. The saved login was removed from the vault.".into()));
                        ui::info("Signed out: login removed from the vault");
                    }
                    Err(e) => self.notice = Some((Kind::Fail, format!("{e:#}"))),
                }
                self.refresh_secrets();
            }
            Message::PickClientId => {
                return Task::perform(
                    async {
                        match rfd::AsyncFileDialog::new().set_title("Choose client_id.bin").pick_file().await {
                            Some(h) => Some(h.read().await),
                            None => None,
                        }
                    },
                    Message::ClientIdPicked,
                );
            }
            Message::ClientIdPicked(Some(bytes)) => {
                if let Some(v) = self.vault.as_mut() {
                    v.contents.secrets.client_id = Some(bytes);
                    self.save_device_change("client_id.bin saved (encrypted).");
                }
            }
            Message::ClientIdPicked(None) => {}
            Message::PickPem => {
                return Task::perform(
                    async {
                        match rfd::AsyncFileDialog::new().set_title("Choose private_key.pem").pick_file().await {
                            Some(h) => String::from_utf8(h.read().await).ok(),
                            None => None,
                        }
                    },
                    Message::PemPicked,
                );
            }
            Message::PemPicked(Some(pem)) => {
                if let Some(v) = self.vault.as_mut() {
                    v.contents.secrets.private_key_pem = Some(pem);
                    self.save_device_change("private_key.pem saved (encrypted).");
                }
            }
            Message::PemPicked(None) => {
                self.notice = Some((Kind::Warn, "That file is not a text PEM key.".into()));
            }
            Message::PickWvd => {
                return Task::perform(
                    async {
                        match rfd::AsyncFileDialog::new().set_title("Choose a .wvd file").pick_file().await {
                            Some(h) => Some(h.read().await),
                            None => None,
                        }
                    },
                    Message::WvdPicked,
                );
            }
            Message::WvdPicked(Some(bytes)) => {
                if let Some(v) = self.vault.as_mut() {
                    v.contents.secrets.wvd = Some(bytes);
                    self.save_device_change(".wvd saved (encrypted).");
                }
            }
            Message::WvdPicked(None) => {}
            Message::ClearDevice => {
                if let Some(v) = self.vault.as_mut() {
                    v.contents.secrets.client_id = None;
                    v.contents.secrets.private_key_pem = None;
                    v.contents.secrets.wvd = None;
                    self.save_device_change("Device removed from the vault.");
                }
            }
            Message::ClearLogs => self.logs.clear(),
            Message::ExportLogs => {
                let text = logs::to_text(&self.logs.iter().cloned().collect::<Vec<_>>());
                let name = format!("crunchyroll-log-{}.txt", chrono::Local::now().format("%Y%m%d-%H%M%S"));
                return Task::perform(
                    async move {
                        match rfd::AsyncFileDialog::new().set_file_name(name).save_file().await {
                            Some(handle) => {
                                let shown = handle.path().display().to_string();
                                handle
                                    .write(text.as_bytes())
                                    .await
                                    .map(|_| Some(shown))
                                    .map_err(|e| e.to_string())
                            }
                            None => Ok(None),
                        }
                    },
                    Message::Exported,
                );
            }
            Message::Exported(Ok(Some(path))) => {
                self.notice = Some((Kind::Ok, format!("Log exported to {path}")));
            }
            Message::Exported(Ok(None)) => {}
            Message::Exported(Err(e)) => self.notice = Some((Kind::Fail, format!("Export failed: {e}"))),
            Message::DismissNotice => self.notice = None,
        }
        Task::none()
    }

    fn save_device_change(&mut self, ok_text: &str) {
        let Some(v) = self.vault.as_mut() else { return };
        match v.save() {
            Ok(()) => {
                self.notice = Some((Kind::Ok, ok_text.to_string()));
                ui::ok(ok_text);
            }
            Err(e) => self.notice = Some((Kind::Fail, format!("{e:#}"))),
        }
    }

    /// Keys while a dropdown is open: Escape closes it, the arrows move the highlight,
    /// and Enter picks the highlighted option.
    fn dropdown_key(&mut self, key: keyboard::Key) -> Task<Message> {
        use keyboard::key::Named;
        let Some(id) = self.dropdown else { return Task::none() };
        let count = views::dropdown_options(self, id).len();
        self.highlight = self.highlight.min(count.saturating_sub(1));
        match key {
            keyboard::Key::Named(Named::Escape) => self.close_dropdown(),
            keyboard::Key::Named(Named::ArrowDown) if count > 0 => {
                self.highlight = (self.highlight + 1) % count;
                return views::scroll_to_highlight(self.highlight);
            }
            keyboard::Key::Named(Named::ArrowUp) if count > 0 => {
                self.highlight = (self.highlight + count - 1) % count;
                return views::scroll_to_highlight(self.highlight);
            }
            keyboard::Key::Named(Named::Enter) => {
                let picked = views::dropdown_options(self, id).get(self.highlight).map(|o| o.value.clone());
                if let Some(value) = picked {
                    return self.update(Message::Choose(id, value));
                }
            }
            _ => {}
        }
        Task::none()
    }

    /// Logs in and reads the episode list of the pasted link.
    fn fetch(&mut self) -> Task<Message> {
        if self.fetching || self.running {
            return Task::none();
        }
        let Some(v) = self.vault.as_ref() else { return Task::none() };
        let secrets = &v.contents.secrets;
        if secrets.etp_rt.is_empty() {
            self.notice = Some((Kind::Warn, "Set up your etp_rt cookie first (Settings > Set up).".into()));
            return Task::none();
        }
        let Some(url) = self.url.split_whitespace().find(|u| u.starts_with("http")).map(str::to_string) else {
            self.notice = Some((Kind::Warn, "Paste an episode or series link first.".into()));
            return Task::none();
        };
        if parse_url(&url).is_none() {
            self.notice = Some((Kind::Warn, "The link must contain /watch/, /series/ or /movie_listing/.".into()));
            return Task::none();
        }
        let st = &v.contents.settings;
        let input = engine::FetchInput {
            etp: Zeroizing::new(secrets.etp_rt.clone()),
            url: url.clone(),
            audio: preferred_request_locale(&parse_langs(&st.audio_langs), "ja-JP"),
            subs: preferred_request_locale(&parse_langs(&st.subs_langs), "en-US"),
            proxy_url: st.proxy_url.clone(),
        };
        // Remember the link, so the next unlock starts from it.
        self.edit_settings(|st| st.last_url = url);
        self.fetching = true;
        self.catalog = None;
        self.picked.clear();
        self.expanded.clear();
        self.notice = None;
        self.current = "Fetching the episode list".into();
        Task::perform(
            async move { catalog_slot(engine::fetch(input).await) },
            Message::Fetched,
        )
    }

    /// Downloads only the episodes the user picked.
    fn download_selected(&mut self) -> Task<Message> {
        if self.running || self.fetching {
            return Task::none();
        }
        let episodes: Vec<SeasonEpisode> = match &self.catalog {
            Some(c) => c
                .seasons
                .iter()
                .flat_map(|g| g.episodes.iter())
                .filter(|e| self.picked.contains(&e.id))
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        if episodes.is_empty() {
            self.notice = Some((Kind::Warn, "Pick at least one episode or season first.".into()));
            return Task::none();
        }
        // Remember the picks, so they are selected again if the same link is fetched later.
        let ids: Vec<String> = episodes.iter().map(|e| e.id.clone()).collect();
        self.edit_settings(|st| st.last_picks = ids);
        self.start_run(episodes, true)
    }

    /// Starts a download of `episodes`. Everything is checked first, and the queue only changes
    /// once the start is allowed, so a refused start leaves the queue as it was. With
    /// `new_queue` the queue is replaced by these episodes; otherwise they are queued again
    /// in place (retries).
    fn start_run(&mut self, episodes: Vec<SeasonEpisode>, new_queue: bool) -> Task<Message> {
        if self.running || self.fetching || episodes.is_empty() {
            return Task::none();
        }
        match &self.ffmpeg {
            FfmpegState::Ready(_) => {}
            FfmpegState::Broken(msg) => {
                let text = format!("ffmpeg is not ready: {msg}");
                self.notice = Some((Kind::Fail, text));
                return Task::none();
            }
            FfmpegState::Unknown | FfmpegState::Checking => {
                self.notice = Some((Kind::Warn, "Still checking ffmpeg. Try again in a moment.".into()));
                return Task::none();
            }
        }
        let Some(v) = self.vault.as_ref() else { return Task::none() };
        let secrets = &v.contents.secrets;
        if !secrets.has_device() {
            self.notice = Some((Kind::Warn, "Set up the Widevine device first (Settings > Set up).".into()));
            return Task::none();
        }
        let st = &v.contents.settings;
        let delay = match parse_duration(&st.download_delay) {
            Ok(d) => d,
            Err(_) => {
                self.notice = Some((Kind::Warn, format!("\"{}\" is not a valid delay, for example 20s.", st.download_delay)));
                return Task::none();
            }
        };
        let segment_workers = match parse_count(&st.segment_workers, 1, 16) {
            Ok(n) => n,
            Err(e) => {
                self.notice = Some((Kind::Warn, format!("Segments at once: {e}.")));
                return Task::none();
            }
        };
        let track_workers = match parse_count(&st.track_workers, 1, 4) {
            Ok(n) => n,
            Err(e) => {
                self.notice = Some((Kind::Warn, format!("Dubs at once: {e}.")));
                return Task::none();
            }
        };
        let mut audio_langs = parse_langs(&st.audio_langs);
        if audio_langs.is_empty() {
            audio_langs = vec!["ja-JP".into()];
        }
        let device = match &secrets.wvd {
            Some(w) => engine::DeviceInput::Wvd(Zeroizing::new(w.clone())),
            None => engine::DeviceInput::Parts {
                client_id: Zeroizing::new(secrets.client_id.clone().unwrap_or_default()),
                pem: Zeroizing::new(secrets.private_key_pem.clone().unwrap_or_default()),
            },
        };
        let output = resolve_output(st);
        let opts = Options {
            audio_langs,
            subs_langs: parse_langs(&st.subs_langs),
            cc_langs: parse_langs(&st.cc_langs),
            video_quality: st.video_quality.clone(),
            audio_quality: st.audio_quality.clone(),
            debug: st.debug,
            output_dir: output.clone(),
            segment_workers,
            track_workers,
        };
        let input = engine::RunInput {
            etp: Zeroizing::new(secrets.etp_rt.clone()),
            device,
            opts,
            delay,
            ffmpeg_path: st.ffmpeg_path.clone(),
            episodes: episodes.clone(),
            proxy_url: st.proxy_url.clone(),
        };
        let ids: Vec<String> = episodes.iter().map(|e| e.id.clone()).collect();

        // Every check passed: the queue changes now.
        if new_queue {
            self.queue = episodes.iter().map(QueueItem::waiting).collect();
        } else {
            for ep in &episodes {
                if let Some(item) = self.queue.iter_mut().find(|q| q.ep.id == ep.id) {
                    item.state = EpisodeState::Waiting;
                }
            }
        }
        self.running = true;
        self.last_output = Some(output);
        self.current = "Starting".into();
        // Set the run's state now, so the first tick never shows the previous run's progress.
        progress::begin_run(&ids);
        progress::set_episode(0, 0, "");
        self.overall = Overall::default();
        self.bars.clear();
        self.progress_shown = 0.0;
        self.progress_target = 0.0;
        self.last_frame = None;
        self.notice = None;
        self.last_activity = Instant::now();
        let (task, handle) = Task::perform(engine::run(input), Message::RunFinished).abortable();
        self.run_handle = Some(handle);
        task
    }

    pub fn view(&self) -> Element<'_, Message> {
        views::root(self)
    }
}

/// True once `minutes` have passed since `last` with no input. Zero minutes never expires.
pub fn idle_expired(last: Instant, now: Instant, minutes: u64) -> bool {
    minutes > 0 && now.saturating_duration_since(last) >= Duration::from_secs(minutes * 60)
}

/// Counts of finished episodes in a queue: (downloaded, failed, skipped or cancelled).
pub fn tally(queue: &[QueueItem]) -> (usize, usize, usize) {
    queue.iter().fold((0, 0, 0), |(done, failed, skipped), item| match item.state {
        EpisodeState::Done => (done + 1, failed, skipped),
        EpisodeState::Failed(_) => (done, failed + 1, skipped),
        EpisodeState::Skipped(_) | EpisodeState::Cancelled => (done, failed, skipped + 1),
        EpisodeState::Waiting | EpisodeState::Downloading => (done, failed, skipped),
    })
}

/// Input events that count as activity for the auto-lock. Mouse moves do not, so the idle
/// timer runs down while the pointer is only moving over the window.
fn activity(event: iced::Event, _status: iced::event::Status, _window: window::Id) -> Option<Message> {
    match event {
        iced::Event::Mouse(mouse::Event::ButtonPressed(_))
        | iced::Event::Mouse(mouse::Event::WheelScrolled { .. })
        | iced::Event::Keyboard(keyboard::Event::KeyPressed { .. }) => Some(Message::Activity),
        _ => None,
    }
}

/// Keys the open dropdown uses. Keys a focused text field already took are ignored.
fn dropdown_key(event: iced::Event, status: iced::event::Status, _window: window::Id) -> Option<Message> {
    match (event, status) {
        (iced::Event::Keyboard(keyboard::Event::KeyPressed { key, .. }), iced::event::Status::Ignored) => {
            Some(Message::KeyPressed(key))
        }
        _ => None,
    }
}

/// Opens a folder in the system file browser.
#[cfg(windows)]
fn open_folder(path: &Path) -> std::io::Result<()> {
    std::process::Command::new("explorer").arg(path).spawn().map(|_| ())
}

#[cfg(target_os = "macos")]
fn open_folder(path: &Path) -> std::io::Result<()> {
    std::process::Command::new("open").arg(path).spawn().map(|_| ())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_folder(path: &Path) -> std::io::Result<()> {
    std::process::Command::new("xdg-open").arg(path).spawn().map(|_| ())
}

fn lang_slot(st: &mut Settings, field: LangField) -> &mut String {
    match field {
        LangField::Audio => &mut st.audio_langs,
        LangField::Subs => &mut st.subs_langs,
        LangField::Cc => &mut st.cc_langs,
    }
}

/// Adds a locale to a comma-separated list. "ALL" replaces everything, and picking a
/// single locale removes "ALL" first.
fn add_lang(current: &str, code: &str) -> String {
    let mut list = parse_langs(current);
    if code.eq_ignore_ascii_case("all") {
        return "ALL".into();
    }
    list.retain(|l| !l.eq_ignore_ascii_case("all") && l != code);
    list.push(code.to_string());
    list.join(",")
}

fn remove_lang(current: &str, code: &str) -> String {
    parse_langs(current).into_iter().filter(|l| l != code).collect::<Vec<_>>().join(",")
}

fn slot(r: Result<Vault, String>) -> Slot {
    std::sync::Arc::new(std::sync::Mutex::new(Some(r)))
}

fn theme_of(_: &App) -> iced::Theme {
    theme::theme()
}

/// Runs the external WebView2 login helper on a blocking thread and returns the captured
/// etp_rt, or a human-readable reason it didn't. The helper is a separate process so its
/// browser event loop never tangles with iced's.
async fn run_login_helper() -> Result<Zeroizing<String>, String> {
    tokio::task::spawn_blocking(run_login_helper_blocking)
        .await
        .map_err(|e| format!("login task failed: {e}"))?
}

#[cfg(windows)]
fn run_login_helper_blocking() -> Result<Zeroizing<String>, String> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    // CREATE_NO_WINDOW: belt-and-suspenders so no console flashes even if the helper is ever
    // built as a console binary.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe = locate_helper();
    let out = Command::new(&exe)
        .arg("--timeout-secs")
        .arg("300")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("could not start the login helper ({}): {e}", exe.display()))?;
    match out.status.code() {
        Some(0) => {
            let mut stdout = out.stdout;
            let found = String::from_utf8_lossy(&stdout)
                .lines()
                .find_map(|l| l.strip_prefix("ETP_RT="))
                .map(|s| Zeroizing::new(s.trim().to_string()))
                .filter(|s| !s.is_empty());
            // The raw pipe buffer held the cookie too; wipe it.
            stdout.zeroize();
            found.ok_or_else(|| "Login finished but no login cookie was found. Please try again.".to_string())
        }
        Some(2) => Err("Login window was closed before sign-in.".to_string()),
        Some(3) => Err("Login timed out. Try again.".to_string()),
        _ => {
            let err = String::from_utf8_lossy(&out.stderr);
            Err(format!("Login helper failed: {}", err.trim()))
        }
    }
}

#[cfg(not(windows))]
fn run_login_helper_blocking() -> Result<Zeroizing<String>, String> {
    Err("Browser login uses WebView2 and is available on Windows only. Paste etp_rt manually instead.".to_string())
}

/// The login helper sits next to the app's own executable when they ship together; fall
/// back to the plain name so PATH can resolve it in a dev build.
#[cfg(windows)]
fn locate_helper() -> PathBuf {
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        let candidate = dir.join("cr-login-helper.exe");
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("cr-login-helper.exe")
}

/// Where a downloaded ffmpeg is installed: the app's per-user data folder, which is always
/// writable (unlike next to the .exe, which may be in Program Files).
const FFMPEG_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";

/// The folder downloads go to: the setting, or Downloads\Crunchyroll when it is unset.
pub(crate) fn resolve_output(st: &Settings) -> PathBuf {
    if vault::output_is_unset(&st.output_dir) {
        vault::default_output_dir()
    } else {
        PathBuf::from(st.output_dir.trim())
    }
}

/// Temp files older than this are leftovers: a live download rewrites its files constantly.
const STALE_TEMP: Duration = Duration::from_secs(6 * 60 * 60);

/// Removes this program's leftovers from the temp folder — decrypted tracks and subtitles
/// (`crdl-*`), half-downloaded ffmpeg archives (`ffmpeg-dl-*`) and login-window profiles
/// (`cr-login-*`) — that a crash or forced close left behind. Only touches our own name
/// prefixes, and only items untouched for hours, so a download running in another window
/// is never disturbed. Returns (items removed, bytes freed).
fn clean_stale_temp() -> (usize, u64) {
    fn size_of(path: &Path) -> u64 {
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.is_dir() => std::fs::read_dir(path)
                .map(|rd| rd.flatten().map(|e| size_of(&e.path())).sum())
                .unwrap_or(0),
            Ok(m) => m.len(),
            Err(_) => 0,
        }
    }
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return (0, 0) };
    let now = std::time::SystemTime::now();
    let (mut count, mut bytes) = (0usize, 0u64);
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ours = ["crdl-", "ffmpeg-dl-", "cr-login-"].iter().any(|p| name.starts_with(p));
        if !ours {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let old = meta.modified().ok().and_then(|m| now.duration_since(m).ok()).is_some_and(|age| age >= STALE_TEMP);
        if !old {
            continue;
        }
        let path = entry.path();
        let size = size_of(&path);
        let removed = if meta.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        if removed.is_ok() {
            count += 1;
            bytes += size;
        }
    }
    (count, bytes)
}

/// Shows a desktop notification. Failures (no notification service) are ignored: the
/// taskbar flash still happens.
fn show_toast(title: &str, body: &str) {
    let _ = notify_rust::Notification::new()
        .appname("Crunchyroll Downloader")
        .summary(title)
        .body(body)
        .show();
}

fn ffmpeg_install_path() -> Result<PathBuf, String> {
    // Portable mode keeps ffmpeg next to the program, where the automatic search finds it
    // even when the drive letter changes.
    if vault::is_portable() {
        if let Some(dir) = vault::exe_dir() {
            return Ok(dir.join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" }));
        }
    }
    let base = dirs::data_local_dir().ok_or_else(|| "no data directory on this system".to_string())?;
    let dir = base.join("crunchyroll-downloader").join("ffmpeg");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    Ok(dir.join(name))
}

/// Pulls ffmpeg(.exe) out of a downloaded build zip (gyan/BtbN nest it under a versioned
/// `.../bin/ffmpeg.exe`) and writes it to `out`.
fn extract_ffmpeg(zip_path: &std::path::Path, out: &std::path::Path) -> Result<PathBuf, String> {
    let file = std::fs::File::open(zip_path).map_err(|e| format!("opening the archive: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("reading the archive: {e}"))?;
    let want = if cfg!(windows) { "bin/ffmpeg.exe" } else { "bin/ffmpeg" };
    let index = (0..archive.len())
        .find(|&i| {
            archive
                .by_index(i)
                .map(|f| f.name().replace('\\', "/").ends_with(want))
                .unwrap_or(false)
        })
        .ok_or_else(|| "the archive did not contain ffmpeg".to_string())?;
    let mut entry = archive.by_index(index).map_err(|e| format!("opening ffmpeg in the archive: {e}"))?;
    let mut dest = std::fs::File::create(out).map_err(|e| format!("creating {}: {e}", out.display()))?;
    std::io::copy(&mut entry, &mut dest).map_err(|e| format!("extracting ffmpeg: {e}"))?;
    drop(dest);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o755));
    }
    Ok(out.to_path_buf())
}

/// Streams an ffmpeg download: Progress while fetching, Extracting while unpacking, then
/// Done(path|error). Progress is throttled to whole-percent steps so it doesn't flood the UI.
fn download_ffmpeg_stream() -> impl futures::Stream<Item = FfmpegDlEvent> {
    async_stream::stream! {
        let install = match ffmpeg_install_path() {
            Ok(p) => p,
            Err(e) => { yield FfmpegDlEvent::Done(Err(e)); return; }
        };
        let tmp_zip = std::env::temp_dir().join(format!("ffmpeg-dl-{}.zip", std::process::id()));

        let client = wreq::Client::new();
        let resp = match client.get(FFMPEG_URL).send().await.and_then(|r| r.error_for_status()) {
            Ok(r) => r,
            Err(e) => { yield FfmpegDlEvent::Done(Err(format!("download failed: {e}"))); return; }
        };
        let total = resp.content_length();

        let file = match tokio::fs::File::create(&tmp_zip).await {
            Ok(f) => f,
            Err(e) => { yield FfmpegDlEvent::Done(Err(format!("cannot write a temp file: {e}"))); return; }
        };
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;
        let mut writer = tokio::io::BufWriter::new(file);
        let mut body = resp.bytes_stream();
        let mut downloaded: u64 = 0;
        let mut last_pct: i32 = -1;
        let mut last_bytes: u64 = 0;
        yield FfmpegDlEvent::Progress(0.0);
        while let Some(item) = body.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(e) => { yield FfmpegDlEvent::Done(Err(format!("download interrupted: {e}"))); return; }
            };
            if let Err(e) = writer.write_all(&chunk).await {
                yield FfmpegDlEvent::Done(Err(format!("write error: {e}"))); return;
            }
            downloaded += chunk.len() as u64;
            match total {
                Some(t) if t > 0 => {
                    let pct = (downloaded * 100 / t) as i32;
                    if pct != last_pct {
                        last_pct = pct;
                        yield FfmpegDlEvent::Progress(downloaded as f32 / t as f32);
                    }
                }
                _ => {
                    // Unknown size: nudge the bar every ~2 MB with a negative marker.
                    if downloaded - last_bytes >= 2_000_000 {
                        last_bytes = downloaded;
                        yield FfmpegDlEvent::Progress(-1.0);
                    }
                }
            }
        }
        if let Err(e) = writer.flush().await {
            yield FfmpegDlEvent::Done(Err(format!("finishing the download: {e}"))); return;
        }
        drop(writer);

        yield FfmpegDlEvent::Extracting;
        let zip_path = tmp_zip.clone();
        let out = install.clone();
        let extracted = tokio::task::spawn_blocking(move || extract_ffmpeg(&zip_path, &out)).await;
        let _ = tokio::fs::remove_file(&tmp_zip).await;
        match extracted {
            Ok(Ok(path)) => yield FfmpegDlEvent::Done(Ok(path)),
            Ok(Err(e)) => yield FfmpegDlEvent::Done(Err(e)),
            Err(e) => yield FfmpegDlEvent::Done(Err(format!("the extract step failed: {e}"))),
        }
    }
}

/// Decodes the embedded PNG logo into a window icon. Returns None if decoding fails, so a
/// bad asset never stops the app from starting.
fn app_icon() -> Option<iced::window::Icon> {
    let bytes = include_bytes!("assets/logo.png");
    let decoder = png::Decoder::new(bytes.as_slice());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    // The asset is saved as 8-bit RGBA, which is exactly what from_rgba wants.
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(info.buffer_size());
    iced::window::icon::from_rgba(buf, info.width, info.height).ok()
}

pub fn run() -> iced::Result {
    let window = iced::window::Settings {
        size: iced::Size::new(1040.0, 680.0),
        icon: app_icon(),
        ..Default::default()
    };
    iced::application(App::boot, App::update, App::view)
        .title("Crunchyroll Downloader")
        .theme(theme_of)
        .subscription(App::subscription)
        .font(include_bytes!("assets/Roboto_400Regular.ttf").as_slice())
        .font(include_bytes!("assets/Roboto_500Medium.ttf").as_slice())
        .font(include_bytes!("assets/Roboto_700Bold.ttf").as_slice())
        .font(include_bytes!("assets/MaterialSymbolsRounded-400.ttf").as_slice())
        .default_font(iced::Font::with_name("Roboto"))
        .window(window)
        .run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_lock_timing() {
        let t0 = Instant::now();
        // Zero minutes means never, however long it has been.
        assert!(!idle_expired(t0, t0 + Duration::from_secs(3600), 0));
        assert!(!idle_expired(t0, t0 + Duration::from_secs(599), 10));
        assert!(idle_expired(t0, t0 + Duration::from_secs(600), 10));
        // Activity moves the reference point forward, so the countdown starts again.
        let active = t0 + Duration::from_secs(500);
        assert!(!idle_expired(active, t0 + Duration::from_secs(900), 10));
    }

    #[test]
    fn queue_tally_counts_each_outcome() {
        let ep = SeasonEpisode {
            id: "x".into(),
            versions: vec![],
            season_number: 1,
            episode_number: 1,
            series_title: "S".into(),
            audio_locale: "ja-JP".into(),
            title: "T".into(),
            availability_starts: String::new(),
            is_movie: false,
        };
        let item = |state| QueueItem { ep: ep.clone(), label: String::new(), state };
        let queue = vec![
            item(EpisodeState::Done),
            item(EpisodeState::Done),
            item(EpisodeState::Failed("x".into())),
            item(EpisodeState::Skipped("already downloaded".into())),
            item(EpisodeState::Cancelled),
            item(EpisodeState::Waiting),
        ];
        assert_eq!(tally(&queue), (2, 1, 2));
    }
}
