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
        Tab::Settings => 1.0,
        Tab::Logs => 2.0,
    };
    index * (NAV_H + NAV_GAP)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Main,
    Settings,
    Logs,
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
    /// ffmpeg status, from the last check.
    pub ffmpeg: FfmpegState,
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
    Tick,
    Frame(Instant),
    ToggleDropdown(DropdownId),
    CloseDropdown,
    Choose(DropdownId, String),
    KeyPressed(keyboard::Key),
    RemoveLang(LangField, String),
    ToggleSecret,
    SetDelay(String),
    SetDebug(bool),
    SetOutputDir(String),
    SetSegmentWorkers(String),
    SetTrackWorkers(String),
    SetAutoLock(String),
    BrowseOutput,
    OutputPicked(Option<PathBuf>),
    PickFfmpeg,
    FfmpegPicked(Option<PathBuf>),
    UseAutoFfmpeg,
    FfmpegChecked(Result<PathBuf, String>),
    OpenSetup,
    GoSetup,
    CloseSetup,
    EtpDraftInput(String),
    SaveEtp,
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
            ffmpeg: FfmpegState::Unknown,
            queue: Vec::new(),
            last_output: None,
            last_activity: Instant::now(),
        };
        (app, focus_password())
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

    /// Starts closing the open dropdown. Its list stays drawn until the animation ends.
    fn close_dropdown(&mut self) {
        self.dropdown = None;
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
        self.picked.clear();
        self.expanded.clear();
        self.fetching = false;
        self.dropdown = None;
        self.dropdown_last = None;
        self.queue.clear();
        self.last_output = None;
        self.ffmpeg = FfmpegState::Unknown;
        self.highlight = 0;
    }

    fn open_vault(&mut self, vault: Vault) {
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
                return iced::widget::operation::scroll_to(
                    page_scroll_id(),
                    iced::widget::scrollable::AbsoluteOffset { x: Some(0.0), y: Some(0.0) },
                );
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
                        // Episodes picked in the last download are picked again, when they are in this list.
                        let saved: Vec<String> = self
                            .vault
                            .as_ref()
                            .map(|v| v.contents.settings.last_picks.clone())
                            .unwrap_or_default();
                        self.picked = catalog
                            .seasons
                            .iter()
                            .flat_map(|g| g.episodes.iter())
                            .filter(|e| saved.contains(&e.id))
                            .map(|e| e.id.clone())
                            .collect();
                        self.notice = Some((Kind::Ok, format!("{} · {seasons} season(s), {episodes} episode(s). Pick what to download.", catalog.title)));
                        self.catalog = Some(catalog);
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
                    self.picked.extend(ids);
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
                    self.notice = Some((Kind::Ok, text));
                } else {
                    self.current = "Finished with errors".into();
                    self.notice = Some((Kind::Warn, text));
                }
                // Flash the taskbar, so a finished download is noticed when the window is in the background.
                return window::latest().and_then(|id| {
                    window::request_user_attention(id, Some(window::UserAttention::Informational))
                });
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
                if let Some(path) = self.last_output.clone() {
                    if let Err(e) = open_folder(&path) {
                        self.notice = Some((Kind::Fail, format!("Could not open {}: {e}", path.display())));
                    }
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
                    // The keyboard starts on the current value.
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
            Message::KeyPressed(key) => return self.dropdown_key(key),
            Message::Choose(id, value) => {
                match id {
                    DropdownId::Video => self.edit_settings(|st| st.video_quality = value),
                    DropdownId::Audio => self.edit_settings(|st| st.audio_quality = value),
                    DropdownId::Lang(field) => self.edit_settings(|st| {
                        let slot = lang_slot(st, field);
                        let next = add_lang(slot.as_str(), &value);
                        *slot = next;
                    }),
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
            Message::SetOutputDir(s) => self.edit_settings(|st| st.output_dir = s),
            Message::SetSegmentWorkers(s) => self.edit_settings(|st| st.segment_workers = s),
            Message::SetTrackWorkers(s) => self.edit_settings(|st| st.track_workers = s),
            Message::SetAutoLock(s) => self.edit_settings(|st| st.auto_lock_minutes = s),
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
                let value = std::mem::take(&mut self.etp_draft);
                let Some(v) = self.vault.as_mut() else { return Task::none() };
                if value.trim().is_empty() {
                    self.notice = Some((Kind::Warn, "Paste the etp_rt value first.".into()));
                    return Task::none();
                }
                v.contents.secrets.etp_rt = value.trim().to_string();
                match v.save() {
                    Ok(()) => {
                        self.notice = Some((Kind::Ok, "etp_rt saved (encrypted).".into()));
                        ui::ok("etp_rt saved to the encrypted vault");
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
            self.notice = Some((Kind::Warn, "The link must contain /watch/ or /series/.".into()));
            return Task::none();
        }
        let st = &v.contents.settings;
        let input = engine::FetchInput {
            etp: Zeroizing::new(secrets.etp_rt.clone()),
            url: url.clone(),
            audio: preferred_request_locale(&parse_langs(&st.audio_langs), "ja-JP"),
            subs: preferred_request_locale(&parse_langs(&st.subs_langs), "en-US"),
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
        let output = PathBuf::from(&st.output_dir);
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

pub fn run() -> iced::Result {
    iced::application(App::boot, App::update, App::view)
        .title("Crunchyroll Downloader")
        .theme(theme_of)
        .subscription(App::subscription)
        .font(include_bytes!("assets/Roboto_400Regular.ttf").as_slice())
        .font(include_bytes!("assets/Roboto_500Medium.ttf").as_slice())
        .font(include_bytes!("assets/Roboto_700Bold.ttf").as_slice())
        .font(include_bytes!("assets/MaterialSymbolsRounded-400.ttf").as_slice())
        .default_font(iced::Font::with_name("Roboto"))
        .window_size((1040.0, 680.0))
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
