//! Crunchyroll downloader: Rust port of CuteTenshii/crunchyroll-downloader.
//! This file mirrors main.go (flags, URL handling, batch files, season selection).

use crunchyroll_downloader::api::Api;
use crunchyroll_downloader::backoff::{parse_duration, Backoff};
use crunchyroll_downloader::episode::{hint_for, Options, Summary};
use crunchyroll_downloader::runner::{self, Plan};
use crunchyroll_downloader::{cdm, ffmpeg, help, ui, util};
use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::Parser;
use std::path::PathBuf;
use util::parse_langs;

/// Colors for `--help`. Clap turns them off automatically when output is not a terminal.
const STYLES: Styles = Styles::styled()
    .header(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Yellow.on_default());

/// Long flag names. Go's flag package accepts both "-name" and "--name", so
/// single-dash forms are rewritten to double-dash before clap sees them.
const LONG_FLAGS: &[&str] = &[
    "url",
    "file",
    "season",
    "audio-lang",
    "subs-lang",
    "cc-lang",
    "video-quality",
    "audio-quality",
    "etp-rt",
    "debug-manifest",
    "download-delay",
    "output-dir",
];

/// Rewrites "-name" to "--name" for known long flags, leaving values and "--" alone.
pub fn normalize_args<I: IntoIterator<Item = String>>(args: I) -> Vec<String> {
    let mut out = Vec::new();
    let mut after_terminator = false;
    for (i, arg) in args.into_iter().enumerate() {
        if i == 0 || after_terminator {
            if arg == "--" {
                after_terminator = true;
            }
            out.push(arg);
            continue;
        }
        if arg == "--" {
            after_terminator = true;
            out.push(arg);
            continue;
        }
        if let Some(rest) = arg.strip_prefix('-') {
            if !rest.starts_with('-') {
                let name = rest.split('=').next().unwrap_or("");
                if LONG_FLAGS.contains(&name) {
                    out.push(format!("-{arg}"));
                    continue;
                }
            }
        }
        out.push(arg);
    }
    out
}

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Downloads anime from Crunchyroll and saves it as MKV",
    styles = STYLES,
    after_help = "Run `crunchyroll-downloader help` for details on each option, for example `help audio-lang`."
)]
struct Cli {
    /// Episode or series link to download
    #[arg(long, default_value = "", help_heading = "Target")]
    url: String,
    /// Text file with one link per line
    #[arg(long, default_value = "", help_heading = "Target")]
    file: String,
    /// Season number (0 = every season). Used with series links only
    #[arg(long, default_value_t = 0, help_heading = "Target")]
    season: i64,
    /// Audio languages, comma-separated, or ALL
    #[arg(long = "audio-lang", default_value = "ja-JP", help_heading = "Languages")]
    audio_lang: String,
    /// Subtitle languages, comma-separated, or ALL
    #[arg(long = "subs-lang", default_value = "en-US", help_heading = "Languages")]
    subs_lang: String,
    /// Closed-caption languages, comma-separated, or ALL
    #[arg(long = "cc-lang", default_value = "", help_heading = "Languages")]
    cc_lang: String,
    /// Video height, e.g. 1080p
    #[arg(long = "video-quality", default_value = "1080p", help_heading = "Quality")]
    video_quality: String,
    /// Audio bitrate, e.g. 192k
    #[arg(long = "audio-quality", default_value = "192k", help_heading = "Quality")]
    audio_quality: String,
    /// Your etp_rt login cookie (or set CR_ETP_RT)
    #[arg(long = "etp-rt", default_value = "", env = "CR_ETP_RT", help_heading = "Account")]
    etp_rt: String,
    /// Minimum pause between episodes, e.g. 30s or 2m
    #[arg(long = "download-delay", default_value = "0s", help_heading = "Network")]
    download_delay: String,
    /// Folder to save into
    #[arg(long = "output-dir", default_value = ".", help_heading = "Output")]
    output_dir: PathBuf,
    /// Print raw playback JSON, manifest XML and full error details
    #[arg(long = "debug-manifest", help_heading = "Debug")]
    debug: bool,
}

const FFMPEG_HELP: &str = "FFmpeg wasn't found. Install it and add it to your PATH, or put ffmpeg.exe in the same folder as this program.\nDownload it from https://www.ffmpeg.org/download.html";

/// Returns the process exit code.
async fn run() -> i32 {
    ui::install_cli_sink();
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("help") {
        return help::run(&argv[2..]);
    }

    let cli = Cli::parse_from(normalize_args(argv));

    let delay = match parse_duration(&cli.download_delay) {
        Ok(d) => d,
        Err(_) => {
            ui::error_block(
                "Invalid --download-delay",
                format!("\"{}\" is not a duration.", cli.download_delay),
                Some("Use a number and a unit, for example 30s, 2m or 1h (see `help download-delay`)."),
            );
            return 2;
        }
    };

    if cli.url.is_empty() && cli.file.is_empty() {
        ui::error_block(
            "Nothing to download",
            "Pass --url with an episode or series link, or --file with a text file of links.",
            Some("Run `crunchyroll-downloader help` to see every option."),
        );
        return 1;
    }

    if cli.etp_rt.is_empty() {
        ui::error_block(
            "No etp_rt cookie given",
            "1. Log in to crunchyroll.com in your browser.\n2. Press F12 and open Application > Cookies.\n3. Copy the value of etp_rt.",
            Some("Pass it with --etp-rt <value>, or set the CR_ETP_RT environment variable (see `help etp-rt`)."),
        );
        return 1;
    }

    // Checked up front so a missing or broken dependency fails before anything is downloaded.
    let ffmpeg_path = match ffmpeg::locate() {
        Ok(p) => p,
        Err(_) => {
            ui::error_block("ffmpeg not found", FFMPEG_HELP, None);
            return 1;
        }
    };
    if let Err(e) = ffmpeg::verify(&ffmpeg_path) {
        ui::error_block("ffmpeg does not work", format!("{e:#}"), Some("Run `ffmpeg -version` in this terminal to see the problem."));
        return 1;
    }
    if let Err(e) = cdm::device() {
        ui::error_block(
            "No Widevine device (CDM) found",
            format!("{e:#}"),
            Some("Copy client_id.bin and private_key.pem, or a .wvd file, into one of the folders above (see `help cdm`)."),
        );
        return 1;
    }

    ui::info("Logging in to Crunchyroll...");
    let api = match Api::login(cli.etp_rt.clone()).await {
        Ok(api) => api,
        Err(e) => {
            let err = anyhow::Error::new(e);
            ui::error_block("Login failed", format!("{err:#}"), hint_for(&err));
            return 1;
        }
    };
    ui::ok("Logged in");

    let audio_langs = {
        let langs = parse_langs(&cli.audio_lang);
        if langs.is_empty() {
            vec!["ja-JP".to_string()]
        } else {
            langs
        }
    };
    let subs_langs = parse_langs(&cli.subs_lang);
    let cc_langs = parse_langs(&cli.cc_lang);

    let opts = Options {
        audio_langs,
        subs_langs,
        cc_langs,
        video_quality: cli.video_quality,
        audio_quality: cli.audio_quality,
        debug: cli.debug,
        output_dir: cli.output_dir,
        segment_workers: crunchyroll_downloader::download::MAX_WORKERS,
        track_workers: 2,
    };
    let plan = Plan::new(api, Backoff::new(delay), opts, cli.season);

    let urls: Vec<String> = if !cli.file.is_empty() {
        let text = match std::fs::read_to_string(&cli.file) {
            Ok(t) => t,
            Err(e) => {
                ui::error_block(
                    "Could not read the URL file",
                    format!("{}: {e}", cli.file),
                    Some("Check the path. It is relative to the folder you run the program from."),
                );
                return 1;
            }
        };
        let urls: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && l.starts_with("http"))
            .map(str::to_string)
            .collect();
        ui::info(format!("Found {} URLs to download", urls.len()));
        urls
    } else {
        vec![cli.url.clone()]
    };

    let summary: Summary = runner::run_urls(&plan, &urls).await;
    summary.print();
    summary.exit_code()
}

#[tokio::main]
async fn main() {
    std::process::exit(run().await);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn single_dash_flags_are_accepted() {
        let out = normalize_args(args(&["bin", "-url", "x", "-season=2", "--etp-rt", "c", "-debug-manifest"]));
        assert_eq!(out, args(&["bin", "--url", "x", "--season=2", "--etp-rt", "c", "--debug-manifest"]));
    }

    #[test]
    fn values_after_terminator_are_untouched() {
        let out = normalize_args(args(&["bin", "--", "-url"]));
        assert_eq!(out, args(&["bin", "--", "-url"]));
    }

    #[test]
    fn unknown_single_dash_flags_are_left_for_clap() {
        let out = normalize_args(args(&["bin", "-h", "-nope"]));
        assert_eq!(out, args(&["bin", "-h", "-nope"]));
    }
}
