//! The GUI's log store. Engine lines arrive through the shared log sink, have any
//! secret removed, and wait in a bounded queue until the window picks them up.

use crunchyroll_downloader::log::{self, Kind};
use std::collections::VecDeque;
use std::sync::Mutex;

/// Most lines kept between two screen updates. Older lines are dropped first.
const MAX_PENDING: usize = 1000;
/// Secrets shorter than this are not searched for, to avoid hiding ordinary words.
const MIN_SECRET_LEN: usize = 8;

/// One line of the log.
#[derive(Debug, Clone)]
pub struct Entry {
    pub time: String,
    pub kind: Kind,
    pub text: String,
}

static PENDING: Mutex<VecDeque<Entry>> = Mutex::new(VecDeque::new());
static SECRETS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Connects the engine's output to the log queue. Call once at startup.
pub fn install_sink() {
    log::set_sink(Box::new(|kind, text| {
        let time = chrono::Local::now().format("%H:%M:%S").to_string();
        let clean = redact(text);
        let mut queue = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        for line in clean.lines() {
            if queue.len() >= MAX_PENDING {
                queue.pop_front();
            }
            queue.push_back(Entry { time: time.clone(), kind, text: line.to_string() });
        }
    }));
}

/// Sets the secret values that must never appear in the log. Pass an empty list to forget them.
pub fn set_secrets(values: Vec<String>) {
    let mut guard = SECRETS.lock().unwrap_or_else(|e| e.into_inner());
    *guard = values.into_iter().filter(|v| v.len() >= MIN_SECRET_LEN).collect();
}

/// Replaces every known secret in `text` with a placeholder.
pub fn redact(text: &str) -> String {
    let guard = SECRETS.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = text.to_string();
    for secret in guard.iter() {
        out = out.replace(secret.as_str(), "[hidden]");
    }
    out
}

/// Takes every line queued since the last call.
pub fn take_pending() -> Vec<Entry> {
    let mut queue = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    queue.drain(..).collect()
}

/// Plain text of the given entries, one per line, for export.
pub fn to_text(entries: &[Entry]) -> String {
    let mut out = String::new();
    for e in entries {
        out.push_str(&format!("[{}] {:<6} {}\n", e.time, label(e.kind), e.text));
    }
    out
}

pub fn label(kind: Kind) -> &'static str {
    match kind {
        Kind::Title => "STEP",
        Kind::Ok => "OK",
        Kind::Info => "INFO",
        Kind::Warn => "WARN",
        Kind::Fail => "FAIL",
        Kind::Detail => "DETAIL",
        Kind::Hint => "HINT",
        Kind::Plain => "LOG",
    }
}
