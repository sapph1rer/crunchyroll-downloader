//! Terminal progress bars. Port of progress.go.
//!
//! A single global renderer draws every active bar, one per line, and rewrites
//! them in place. Parallel video and audio downloads can both report progress
//! without clobbering each other's lines.

use std::collections::BTreeSet;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Columns occupied by the bar itself, excluding brackets and the percentage.
pub const PROGRESS_WIDTH: i64 = 40;
/// ANSI escape prefix for cursor movement and line clearing.
const ESC: &str = "\x1b";
/// Sliding window over which the download rate is averaged.
const RATE_WINDOW: Duration = Duration::from_secs(2);
/// Minimum spacing between recorded rate samples.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
pub struct ByteSample {
    pub at: Instant,
    pub bytes: i64,
}

/// One bar's state. Also used directly by the renderer tests.
#[derive(Debug, Clone, Default)]
pub struct Bar {
    /// What is being downloaded, e.g. "video" or "audio 日本語".
    pub title: String,
    /// Unit shown after the count, e.g. "segments". Empty hides the count.
    pub label: String,
    pub total: i64,
    pub current: i64,
    pub samples: Vec<ByteSample>,
}

struct Renderer {
    bars: Vec<(u64, Bar)>,
    lines: usize,
    next_id: u64,
}

/// Whether bars are drawn on the terminal. The GUI turns this off, since it has its own display.
static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Turns terminal progress bars on or off.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
}

static RENDERER: LazyLock<Mutex<Renderer>> = LazyLock::new(|| {
    Mutex::new(Renderer {
        bars: Vec::new(),
        lines: 0,
        next_id: 1,
    })
});

/// Where the whole run is: which episode is current, and how many of its tracks have finished.
#[derive(Debug, Clone, Default)]
pub struct Overall {
    /// 0-based index of the episode being downloaded. Equals `total` once all are done.
    pub index: usize,
    pub total: usize,
    /// Name of the current episode, for display.
    pub label: String,
    /// Tracks the current episode downloads: one video plus one per audio version.
    pub tracks_expected: usize,
    /// Tracks of the current episode that have finished.
    pub tracks_done: usize,
}

static OVERALL: Mutex<Overall> = Mutex::new(Overall {
    index: 0,
    total: 0,
    label: String::new(),
    tracks_expected: 0,
    tracks_done: 0,
});

/// Marks which episode of the run is downloading now, and resets its track count.
pub fn set_episode(index: usize, total: usize, label: &str) {
    let mut o = OVERALL.lock().unwrap_or_else(|e| e.into_inner());
    o.index = index;
    o.total = total;
    o.label = label.to_string();
    o.tracks_expected = 0;
    o.tracks_done = 0;
}

/// Sets how many tracks the current episode will download.
pub fn set_tracks_expected(n: usize) {
    OVERALL.lock().unwrap_or_else(|e| e.into_inner()).tracks_expected = n;
}

/// A copy of the run's overall state.
pub fn overall() -> Overall {
    OVERALL.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Copies of the bars being drawn now, for the GUI.
pub fn snapshot() -> Vec<Bar> {
    let r = RENDERER.lock().unwrap_or_else(|e| e.into_inner());
    r.bars.iter().map(|(_, b)| b.clone()).collect()
}

/// Where one episode of a run stands. The run updates these, and the GUI shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EpisodeState {
    Waiting,
    Downloading,
    Done,
    /// Not downloaded on purpose: already present, or no requested language exists.
    Skipped(String),
    Failed(String),
    /// Stopped by the user before it finished.
    Cancelled,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Episode IDs of the current run, in run order, each with its state.
static RUN: Mutex<Vec<(String, EpisodeState)>> = Mutex::new(Vec::new());
/// IDs the user cancelled. An episode in this set is skipped when its turn comes.
static CANCELLED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
/// The episode downloading now, if any.
static CURRENT: Mutex<Option<String>> = Mutex::new(None);
/// Set when the user cancels the episode that is downloading. The download loops check it.
static CURRENT_CANCEL: AtomicBool = AtomicBool::new(false);

/// Starts a run over `ids`. Every episode is waiting, and earlier cancellations are forgotten.
pub fn begin_run(ids: &[String]) {
    *locked(&RUN) = ids.iter().map(|id| (id.clone(), EpisodeState::Waiting)).collect();
    locked(&CANCELLED).clear();
}

/// Sets the state of the episode at `index` in the current run.
pub fn set_state(index: usize, state: EpisodeState) {
    if let Some(slot) = locked(&RUN).get_mut(index) {
        slot.1 = state;
    }
}

/// A copy of every episode of the current run with its state. An episode the user cancelled
/// shows as cancelled at once, even before the run reaches it.
pub fn run_states() -> Vec<(String, EpisodeState)> {
    let cancelled = locked(&CANCELLED).clone();
    locked(&RUN)
        .iter()
        .map(|(id, state)| {
            let state = if *state == EpisodeState::Waiting && cancelled.contains(id) {
                EpisodeState::Cancelled
            } else {
                state.clone()
            };
            (id.clone(), state)
        })
        .collect()
}

/// Marks `id` as the episode downloading now.
pub fn start_episode(id: &str) {
    *locked(&CURRENT) = Some(id.to_string());
    CURRENT_CANCEL.store(false, Ordering::SeqCst);
}

/// Marks that no episode is downloading.
pub fn end_episode() {
    *locked(&CURRENT) = None;
    CURRENT_CANCEL.store(false, Ordering::SeqCst);
}

/// True once the user has cancelled the episode that is downloading. Download loops stop on it.
pub fn cancel_requested() -> bool {
    CURRENT_CANCEL.load(Ordering::SeqCst)
}

/// Whether the user cancelled this episode before its turn.
pub fn is_cancelled(id: &str) -> bool {
    locked(&CANCELLED).contains(id)
}

/// Cancels one episode. If it is the one downloading, its download stops at the next step.
pub fn cancel_episode(id: &str) {
    locked(&CANCELLED).insert(id.to_string());
    if locked(&CURRENT).as_deref() == Some(id) {
        CURRENT_CANCEL.store(true, Ordering::SeqCst);
    }
}

/// Sleeps for `d`, but returns early once the user cancels the running episode.
/// Waits are split into short steps so a cancel takes effect quickly.
pub async fn pause(d: Duration) {
    let end = Instant::now() + d;
    loop {
        if cancel_requested() {
            return;
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        tokio::time::sleep(left.min(Duration::from_millis(250))).await;
    }
}

/// Handle to a registered bar. Dropping it (or calling `finish`) removes the bar
/// from the display.
pub struct ProgressBar {
    id: u64,
}

impl ProgressBar {
    /// Registers a bar. Pass total 0 and call `set_total` once it is known.
    pub fn new(title: &str, total: i64, label: &str) -> Self {
        let mut r = RENDERER.lock().unwrap();
        let id = r.next_id;
        r.next_id += 1;
        r.bars.push((
            id,
            Bar {
                title: title.to_string(),
                label: label.to_string(),
                total,
                ..Default::default()
            },
        ));
        render_locked(&mut r);
        ProgressBar { id }
    }

    fn with<T>(&self, f: impl FnOnce(&mut Bar) -> T) -> Option<T> {
        let mut r = RENDERER.lock().unwrap();
        r.bars.iter_mut().find(|(id, _)| *id == self.id).map(|(_, b)| f(b))
    }

    /// Records the total once known. Later calls are ignored so a partial
    /// response cannot shrink it.
    pub fn set_total(&self, total: i64) {
        self.with(|b| {
            if b.total == 0 && total > 0 {
                b.total = total;
            }
        });
    }

    /// Advances the bar. Out-of-order updates never move it backwards.
    pub fn update(&self, current: i64, bytes: i64) {
        let mut r = RENDERER.lock().unwrap();
        let Some((_, b)) = r.bars.iter_mut().find(|(id, _)| *id == self.id) else {
            return;
        };
        let mut current = current.max(0);
        if current <= b.current {
            return;
        }
        if b.total > 0 && current > b.total {
            current = b.total;
        }
        b.current = current;
        record_bytes(b, bytes);
        render_locked(&mut r);
    }

    /// Removes the bar from the display and redraws the rest.
    pub fn finish(self) {
        drop(self);
    }
}

/// Removing the bar on drop keeps a cancelled download (for example one whose
/// sibling track failed) from leaving a stale line on screen.
impl Drop for ProgressBar {
    fn drop(&mut self) {
        let removed = {
            let mut r = RENDERER.lock().unwrap();
            let before = r.bars.len();
            r.bars.retain(|(id, _)| *id != self.id);
            let removed = r.bars.len() != before;
            if removed {
                render_locked(&mut r);
            }
            removed
        };
        // A bar that leaves the display has finished (or failed), so count it for the episode.
        if removed {
            OVERALL.lock().unwrap_or_else(|e| e.into_inner()).tracks_done += 1;
        }
    }
}

/// Appends a rate sample, pruning samples older than the rate window.
fn record_bytes(b: &mut Bar, bytes: i64) {
    let now = Instant::now();
    if let Some(last) = b.samples.last_mut() {
        if now.duration_since(last.at) < SAMPLE_INTERVAL {
            last.bytes = bytes;
            return;
        }
    }
    b.samples.push(ByteSample { at: now, bytes });
    let cutoff = now.checked_sub(RATE_WINDOW).unwrap_or(now);
    while b.samples.len() > 2 && b.samples[1].at < cutoff {
        b.samples.remove(0);
    }
}

impl Bar {
    /// Download rate in megabits per second over the rate window. 0 until enough
    /// samples have accumulated.
    pub fn rate_mbps(&self) -> f64 {
        if self.samples.len() < 2 {
            return 0.0;
        }
        let first = &self.samples[0];
        let last = &self.samples[self.samples.len() - 1];
        let elapsed = last.at.duration_since(first.at).as_secs_f64();
        if elapsed <= 0.0 {
            return 0.0;
        }
        let bytes_per_sec = (last.bytes - first.bytes) as f64 / elapsed;
        bytes_per_sec * 8.0 / 1e6
    }

    /// One line of text, title padded to `title_width` columns.
    pub fn render_line(&self, title_width: usize) -> String {
        let mut current = self.current;
        let total = self.total;
        let mut percent = 0i64;
        let mut filled = 0i64;
        if total > 0 {
            if current > total {
                current = total;
            }
            percent = current * 100 / total;
            filled = current * PROGRESS_WIDTH / total;
        }

        let bar = if filled > 0 {
            format!("{}>", "=".repeat((filled - 1) as usize))
        } else {
            String::new()
        };
        let pad = (PROGRESS_WIDTH - filled).max(0) as usize;
        let mut line = format!("[{bar}{}] {percent:>3}%", " ".repeat(pad));
        if !self.label.is_empty() {
            line.push_str(&format!(" ({current}/{total} {})", self.label));
        }
        let rate = self.rate_mbps();
        if rate > 0.0 {
            line.push_str(&format!(" {rate:.1} Mbps"));
        }
        if !self.title.is_empty() {
            let pad = title_width.saturating_sub(display_width(&self.title));
            line = format!("{}{}: {line}", self.title, " ".repeat(pad));
        }
        line
    }
}

impl Overall {
    /// Fraction of the whole run finished, from 0.0 to 1.0. `bars` are the bars drawn now.
    pub fn fraction(&self, bars: &[Bar]) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        let episode = if self.tracks_expected == 0 {
            0.0
        } else {
            let active: f32 = bars
                .iter()
                .filter(|b| b.total > 0)
                .map(|b| (b.current as f32 / b.total as f32).min(1.0))
                .sum();
            ((self.tracks_done as f32 + active) / self.tracks_expected as f32).min(1.0)
        };
        ((self.index as f32 + episode) / self.total as f32).min(1.0)
    }
}

/// Redraws every bar, moving the cursor back over the previous frame first.
fn render_locked(r: &mut Renderer) {
    let title_width = r.bars.iter().map(|(_, b)| display_width(&b.title)).max().unwrap_or(0);
    let prev = r.lines;
    let mut out = String::new();
    if prev > 0 {
        out.push_str(&format!("{ESC}[{prev}A"));
    }
    for (_, b) in &r.bars {
        out.push_str(&format!("{ESC}[2K\r{}\n", b.render_line(title_width)));
    }
    let now = r.bars.len();
    for _ in now..prev {
        out.push_str(&format!("{ESC}[2K\r\n"));
    }
    if prev > now {
        out.push_str(&format!("{ESC}[{}A", prev - now));
    }
    if ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
    }
    r.lines = now;
}

/// Terminal columns used by `s`, counting CJK and other wide runes as two.
pub fn display_width(s: &str) -> usize {
    s.chars().map(|c| if double_width(c) { 2 } else { 1 }).sum()
}

/// Whether `r` occupies two terminal columns.
pub fn double_width(r: char) -> bool {
    let c = r as u32;
    c >= 0x1100
        && (c <= 0x115F
            || c == 0x2329
            || c == 0x232A
            || (0x2E80..=0xA4CF).contains(&c) && c != 0x303F
            || (0xAC00..=0xD7A3).contains(&c)
            || (0xF900..=0xFAFF).contains(&c)
            || (0xFE10..=0xFE19).contains(&c)
            || (0xFE30..=0xFE6F).contains(&c)
            || (0xFF00..=0xFF60).contains(&c)
            || (0xFFE0..=0xFFE6).contains(&c)
            || (0x20000..=0x2FFFD).contains(&c)
            || (0x30000..=0x3FFFD).contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_line_includes_percentage_and_count() {
        let bar = Bar { title: "video".into(), label: "segments".into(), total: 100, current: 42, ..Default::default() };
        let line = bar.render_line(display_width("video"));
        assert!(line.contains("42%"), "{line}");
        assert!(line.contains("(42/100 segments)"), "{line}");
        assert!(line.starts_with("video: "), "{line}");
    }

    #[test]
    fn render_line_clamps_and_hides_detail() {
        let bar = Bar { title: "video".into(), label: "segments".into(), total: 100, current: 150, ..Default::default() };
        let line = bar.render_line(display_width("video"));
        assert!(line.contains("100%") && line.contains("(100/100 segments)"), "{line}");

        let bar = Bar { total: 100, current: 50, ..Default::default() };
        let line = bar.render_line(0);
        assert!(line.ends_with("50%"), "{line}");
    }

    #[test]
    fn render_line_aligns_titles() {
        let titles = ["Downloading video", "Downloading English audio", "Downloading 日本語 audio"];
        let width = titles.iter().map(|t| display_width(t)).max().unwrap();
        for t in titles {
            let bar = Bar { title: t.into(), ..Default::default() };
            let line = bar.render_line(width);
            let idx = line.find(": ").expect("title separator");
            assert_eq!(display_width(&line[..idx]), width, "{line}");
        }
    }

    #[test]
    fn run_fraction_counts_finished_tracks() {
        // Second of two episodes, two tracks expected, one finished and one half done.
        let o = Overall { index: 1, total: 2, tracks_expected: 2, tracks_done: 1, ..Default::default() };
        let bars = [Bar { total: 100, current: 50, ..Default::default() }];
        assert!((o.fraction(&bars) - 0.875).abs() < 1e-6);
        assert_eq!(Overall::default().fraction(&[]), 0.0);
        let done_half = Overall { index: 2, total: 4, ..Default::default() };
        assert!((done_half.fraction(&[]) - 0.5).abs() < 1e-6);
    }

    /// One test for all of the run state: the statics are shared, so separate tests
    /// would race with each other.
    #[tokio::test]
    async fn run_state_and_cancellation() {
        let ids: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        begin_run(&ids);
        assert!(run_states().iter().all(|(_, s)| *s == EpisodeState::Waiting));

        set_state(0, EpisodeState::Done);
        set_state(1, EpisodeState::Failed("boom".into()));
        set_state(9, EpisodeState::Done); // out of range: ignored
        let states = run_states();
        assert_eq!(states[0], ("a".to_string(), EpisodeState::Done));
        assert_eq!(states[1].1, EpisodeState::Failed("boom".into()));
        assert_eq!(states.len(), 3);

        // Cancelling a waiting episode marks it; the current one sets the stop flag.
        cancel_episode("c");
        assert!(is_cancelled("c"));
        assert!(!is_cancelled("b"));
        start_episode("b");
        assert!(!cancel_requested());
        cancel_episode("b");
        assert!(cancel_requested());
        end_episode();
        assert!(!cancel_requested());

        // A new run forgets earlier cancellations.
        begin_run(&ids);
        assert!(!is_cancelled("c"));

        // A pause in the running episode ends as soon as it is cancelled.
        start_episode("a");
        cancel_episode("a");
        let started = Instant::now();
        pause(Duration::from_secs(30)).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        end_episode();
        begin_run(&[]);
    }

    #[test]
    fn double_width_detection() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("日本語"), 6);
    }
}
