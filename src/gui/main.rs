#![cfg_attr(windows, windows_subsystem = "windows")]
//! Desktop window for Crunchyroll Downloader. Build with `--features gui`.

mod app;
mod dropdown;
mod engine;
mod logs;
mod theme;
mod views;

fn main() -> iced::Result {
    app::run()
}
