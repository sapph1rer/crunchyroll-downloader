//! Core of the Crunchyroll downloader. The CLI and the GUI both use this crate.

pub mod api;
pub mod backoff;
pub mod cdm;
pub mod cenc;
pub mod download;
pub mod drm;
pub mod episode;
pub mod ffmpeg;
pub mod help;
pub mod log;
pub mod mpd;
pub mod progress;
pub mod runner;
pub mod ui;
pub mod util;
pub mod vault;
