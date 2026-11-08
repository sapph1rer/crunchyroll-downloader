//! `crunchyroll-downloader help [topic]`: detailed help for each option, plus
//! reference pages for languages and errors. Topic names accept common aliases,
//! and a misspelled topic gets a suggestion.

use crate::ui;
use crate::util::{LANGUAGE_CODES, LANGUAGE_NAMES};

struct Topic {
    name: &'static str,
    aliases: &'static [&'static str],
    summary: &'static str,
    usage: &'static str,
    details: &'static [&'static str],
    examples: &'static [&'static str],
}

const TOPICS: &[Topic] = &[
    Topic {
        name: "url",
        aliases: &["link", "watch", "series"],
        summary: "The episode or series page to download",
        usage: "--url <URL>",
        details: &[
            "A /watch/ link downloads one episode.",
            "A /series/ link downloads the whole series, or one season with --season.",
            "Wrap the link in quotes so PowerShell does not split it.",
        ],
        examples: &["--url \"https://www.crunchyroll.com/series/XXXX/title\""],
    },
    Topic {
        name: "file",
        aliases: &["list", "batch", "urls"],
        summary: "A text file with one URL per line",
        usage: "--file <PATH>",
        details: &[
            "Each line that starts with http is downloaded in order. Other lines are ignored.",
            "A failed URL is reported in the summary and the rest still download.",
        ],
        examples: &["--file urls.txt"],
    },
    Topic {
        name: "season",
        aliases: &["season-number"],
        summary: "Which season of a series to download",
        usage: "--season <NUMBER>",
        details: &[
            "0 (the default) downloads every season of the series.",
            "Only used with a /series/ link. Ignored for a single /watch/ episode.",
        ],
        examples: &["--season 2"],
    },
    Topic {
        name: "audio-lang",
        aliases: &["audios-lang", "audio", "audio-language", "dub", "dubs"],
        summary: "Which audio languages (dubs) to download",
        usage: "--audio-lang <CODE[,CODE...] | ALL>",
        details: &[
            "Comma-separated locale codes. Each one becomes its own audio track in the MKV.",
            "ALL downloads every dub the episode offers.",
            "A language the episode does not have is skipped with a note in the summary.",
            "Run `help languages` for the list of codes. Default: ja-JP.",
        ],
        examples: &["--audio-lang ja-JP,en-US", "--audio-lang ALL"],
    },
    Topic {
        name: "subs-lang",
        aliases: &["subs", "sub-lang", "subtitles", "subtitle-lang", "subs-languages"],
        summary: "Which subtitle languages to embed",
        usage: "--subs-lang <CODE[,CODE...] | ALL>",
        details: &[
            "Subtitles are embedded as separate tracks. You can switch them on or off in your player.",
            "ALL embeds every subtitle the episode offers.",
            "Default: en-US. Run `help languages` for the list of codes.",
        ],
        examples: &["--subs-lang th-TH,en-US", "--subs-lang ALL"],
    },
    Topic {
        name: "cc-lang",
        aliases: &["cc", "captions", "closed-captions"],
        summary: "Which closed-caption languages to embed",
        usage: "--cc-lang <CODE[,CODE...] | ALL>",
        details: &[
            "Closed captions transcribe the audio of one specific dub, so they exist only on some dubs.",
            "Empty by default, which means no captions.",
        ],
        examples: &["--cc-lang en-US"],
    },
    Topic {
        name: "video-quality",
        aliases: &["quality", "resolution", "video"],
        summary: "The video height to download",
        usage: "--video-quality <HEIGHT>p",
        details: &[
            "Values: best (highest available), 1080p, 720p, 480p, 360p. The episode must offer the height you pick.",
            "Default: 1080p.",
        ],
        examples: &["--video-quality 720p"],
    },
    Topic {
        name: "audio-quality",
        aliases: &["audio-bitrate", "bitrate"],
        summary: "The audio bitrate to download",
        usage: "--audio-quality <BITRATE>k",
        details: &["Values: 192k, 128k, 96k. Default: 192k."],
        examples: &["--audio-quality 128k"],
    },
    Topic {
        name: "etp-rt",
        aliases: &["cookie", "etp_rt", "login", "token"],
        summary: "Your Crunchyroll login cookie (required)",
        usage: "--etp-rt <VALUE>   or   set CR_ETP_RT",
        details: &[
            "1. Log in to crunchyroll.com in your browser.",
            "2. Press F12, open Application (or Storage) > Cookies > https://www.crunchyroll.com.",
            "3. Copy the value of etp_rt. Paste it after --etp-rt.",
            "Keep the value private. Anyone with it can use your account.",
            "To keep it out of the command line and shell history, set the environment variable instead:",
            "  PowerShell:  $env:CR_ETP_RT = \"<value>\"",
        ],
        examples: &["--etp-rt <value>"],
    },
    Topic {
        name: "cdm",
        aliases: &["device", "wvd", "widevine", "drm", "keys", "key"],
        summary: "The Widevine device needed for decryption (required)",
        usage: "a .wvd file, or client_id.bin + private_key.pem",
        details: &[
            "Crunchyroll encrypts video. A Widevine device is needed to get the decryption keys.",
            "Provide one of:",
            "  - a .wvd file (any name ending in .wvd), or",
            "  - client_id.bin and private_key.pem together.",
            "The program searches the folder you run it from first, then the folder that holds the program.",
            "A missing device is reported before anything is downloaded.",
        ],
        examples: &[],
    },
    Topic {
        name: "download-delay",
        aliases: &["delay", "wait", "sleep"],
        summary: "Pause between episodes",
        usage: "--download-delay <DURATION>",
        details: &[
            "Examples: 30s, 2m, 1h. Default: 0s.",
            "The program waits this long before each episode after the first one.",
            "After a rate-limit error, the wait grows on its own, separate from this setting.",
        ],
        examples: &["--download-delay 20s"],
    },
    Topic {
        name: "output-dir",
        aliases: &["output", "out", "dir", "folder", "destination"],
        summary: "Where downloads are saved",
        usage: "--output-dir <PATH>",
        details: &[
            "Each series gets its own subfolder. Default: the current folder.",
            "Finished files have their real name. An unfinished download is named *.part.mkv and is removed on the next run.",
        ],
        examples: &["--output-dir \"D:\\Anime\""],
    },
    Topic {
        name: "debug-manifest",
        aliases: &["debug", "verbose", "manifest"],
        summary: "Print raw data and full error details",
        usage: "--debug-manifest",
        details: &[
            "Prints the raw playback JSON and manifest XML, and shows the full error chain.",
            "Use it when reporting a bug.",
        ],
        examples: &["--debug-manifest"],
    },
];

const ERRORS: &[(&str, &str)] = &[
    ("No etp_rt cookie given", "Pass --etp-rt or set CR_ETP_RT. See `help etp-rt`."),
    ("Login failed: rejected the etp_rt cookie", "The cookie is wrong or expired. Copy a fresh one from the browser."),
    ("Blocked by Cloudflare's bot check", "Crunchyroll's bot protection stopped the request. Wait, change network, try later."),
    ("No Widevine device (CDM) found", "Add a .wvd file, or client_id.bin and private_key.pem. See `help cdm`."),
    ("Rate limited by Crunchyroll", "The program waits and retries on its own. A longer --download-delay helps."),
    ("getLicense ... refused", "The license server refused the device. Try later. The device file may be revoked."),
    ("PSSH not found", "That audio language has no DRM yet. Choose another with --audio-lang."),
    ("video/audio base URL, or quality wrong", "The episode does not offer that quality. See `help video-quality` and `help audio-quality`."),
    ("ffmpeg not found / ffmpeg does not work", "Install ffmpeg or put ffmpeg.exe next to the program. Check with `ffmpeg -version`."),
    ("ffmpeg failed", "ffmpeg could not merge the files. Read the ffmpeg message printed above it."),
    ("Invalid --download-delay", "Use a number with a unit: 30s, 2m, 1h. See `help download-delay`."),
];

/// Runs the help command. `args` are the words after "help". Returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let Some(raw) = args.first() else {
        overview();
        return 0;
    };
    let word = raw.trim().to_lowercase().replace('_', "-");
    match word.as_str() {
        "languages" | "language" | "langs" | "lang" | "locales" => {
            languages();
            return 0;
        }
        "errors" | "error" => {
            errors();
            return 0;
        }
        _ => {}
    }
    if let Some(t) = TOPICS.iter().find(|t| t.name == word || t.aliases.contains(&word.as_str())) {
        show(t);
        return 0;
    }

    let mut names: Vec<&str> = TOPICS.iter().map(|t| t.name).collect();
    names.extend(["languages", "errors"]);
    let mut ranked: Vec<(usize, &str)> = names.iter().map(|n| (distance(&word, n), *n)).collect();
    ranked.sort();
    let suggestion = ranked
        .first()
        .filter(|(d, _)| *d <= 3)
        .map(|(_, n)| format!("Did you mean `help {n}`?"));
    ui::error_block(
        format!("Unknown help topic \"{raw}\""),
        format!("Topics: {}", names.join(", ")),
        suggestion.as_deref(),
    );
    2
}

fn overview() {
    ui::title("crunchyroll-downloader help");
    ui::detail("Run `help <topic>` for details on one option. Examples: help audio-lang, help subs-lang");
    ui::title("Options");
    for t in TOPICS {
        println!("  {:<16} {}", t.name, t.summary);
    }
    ui::title("Reference");
    println!("  {:<16} Language codes accepted by --audio-lang, --subs-lang and --cc-lang", "languages");
    println!("  {:<16} Common errors and how to fix them", "errors");
    ui::title("Example");
    println!("  crunchyroll-downloader --url \"<link>\" --etp-rt <cookie> --audio-lang ja-JP,en-US --subs-lang th-TH,en-US");
}

fn show(t: &Topic) {
    ui::title(format!("{} — {}", t.name, t.summary));
    if !t.aliases.is_empty() {
        ui::detail(format!("Also written as: {}", t.aliases.join(", ")));
    }
    ui::info(format!("Usage: {}", t.usage));
    for d in t.details {
        println!("  {d}");
    }
    if !t.examples.is_empty() {
        ui::title("Examples");
        for e in t.examples {
            println!("  crunchyroll-downloader {e}");
        }
    }
}

fn languages() {
    ui::title("Language codes");
    let mut codes: Vec<&&str> = LANGUAGE_CODES.keys().collect();
    codes.sort();
    println!("  {:<8} {:<6} {}", "code", "iso", "name");
    for code in codes {
        let iso = LANGUAGE_CODES.get(*code).copied().unwrap_or("");
        let name = LANGUAGE_NAMES.get(*code).copied().unwrap_or("");
        println!("  {:<8} {:<6} {}", code, iso, name);
    }
    ui::detail("Use ALL in --audio-lang, --subs-lang or --cc-lang to take every one the episode offers.");
}

fn errors() {
    ui::title("Common errors");
    for (what, fix) in ERRORS {
        ui::fail(what);
        ui::hint(fix);
    }
}

/// Levenshtein edit distance, used to suggest a topic for a typo.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typo_suggests_the_right_topic() {
        assert_eq!(distance("audios-lang", "audio-lang"), 1);
        assert_eq!(distance("subs", "subs-lang"), 5);
    }

    #[test]
    fn aliases_resolve_to_topics() {
        let found = |w: &str| TOPICS.iter().find(|t| t.name == w || t.aliases.contains(&w)).map(|t| t.name);
        assert_eq!(found("audios-lang"), Some("audio-lang"));
        assert_eq!(found("cookie"), Some("etp-rt"));
        assert_eq!(found("nope"), None);
    }

    #[test]
    fn every_option_has_a_topic() {
        for flag in ["url", "file", "season", "audio-lang", "subs-lang", "cc-lang", "video-quality", "audio-quality", "etp-rt", "download-delay", "output-dir", "debug-manifest"] {
            assert!(TOPICS.iter().any(|t| t.name == flag), "missing topic {flag}");
        }
    }
}
