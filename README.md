A Rust tool that downloads Crunchyroll episodes and whole seasons and saves them as MKV files,
with the audio languages, subtitles and closed captions you choose.

It comes in two forms:

- **A command-line tool** (`crunchyroll-downloader`) for scripts, batch files and servers.
- **A desktop app** (`crunchyroll-downloader-gui`) with an encrypted settings vault, an episode picker,
  a download queue and a live log.

This is a Rust port of [CuteTenshii/crunchyroll-downloader](https://github.com/CuteTenshii/crunchyroll-downloader).
The command-line flags match the original, so existing commands and batch files still work.

> **Use it responsibly.** Crunchyroll's terms of service restrict downloading content outside its app.
> Only use this tool for content you have the right to download. You are responsible for how you use it.

---

## Contents

- [Features](#features)
- [What you need](#what-you-need)
- [Build from source](#build-from-source)
- [Install](#install)
- [Get your credentials](#get-your-credentials)
- [Command-line usage](#command-line-usage)
- [Desktop app usage](#desktop-app-usage)
- [Settings reference](#settings-reference)
- [Output and file names](#output-and-file-names)
- [Troubleshooting](#troubleshooting)
- [How it works](#how-it-works)
- [Project layout](#project-layout)
- [Testing](#testing)
- [Differences from the Go version](#differences-from-the-go-version)
- [License](#license)

---

## Features

- Download a single episode, a whole series, or one season at a time.
- Read many links from a text file in one run.
- Choose audio languages (dubs), subtitle languages and closed-caption languages. Use `ALL` to take every one.
- Choose video height (for example `1080p`) and audio bitrate (for example `192k`).
- Segments are downloaded in parallel and written in order, so a file is never stored out of sequence.
- Decryption (Widevine CENC) happens inside the program. ffmpeg is used only to merge the final MKV.
- Automatic retries with backoff when Crunchyroll rate-limits or a request fails.
- A pause between episodes (`--download-delay`) to be gentle on the service.
- Clear error messages, each with a suggested fix, and a summary at the end of every run.
- Desktop app: encrypted settings (Argon2id + AES-256-GCM), per-episode queue with retry and cancel,
  auto-lock after inactivity, and an onboarding checklist when something is missing.

---

## What you need

| Requirement | Why | Notes |
|---|---|---|
| A Crunchyroll account with an active subscription | To access the content | Your account must be able to watch the title. |
| Your `etp_rt` login cookie | Logs the tool in | See [Get your credentials](#get-your-credentials). |
| A Widevine device | Needed to decrypt the video | Either a `.wvd` file, or `client_id.bin` plus `private_key.pem`. |
| ffmpeg | Merges video, audio and subtitles into MKV | Must run `ffmpeg -version`. |
| Rust (stable, 2021 edition or newer) | To build from source | Install with [rustup](https://rustup.rs). |

On Windows, the desktop app and the command-line tool both work. The desktop app uses a native window
through [iced](https://iced.rs), so it does not need a browser.

---

## Build from source

Clone the repository and build in release mode:

```bash
git clone <your-repo-url> crunchyroll-downloader-rs
cd crunchyroll-downloader-rs
cargo build --release
```

This builds **both** programs by default:

| Program | Path after build | Purpose |
|---|---|---|
| `crunchyroll-downloader` | `target/release/crunchyroll-downloader` (`.exe` on Windows) | Command-line tool |
| `crunchyroll-downloader-gui` | `target/release/crunchyroll-downloader-gui` (`.exe` on Windows) | Desktop app |

### Build only the command-line tool

The desktop app depends on extra libraries. If you only need the command line, skip them:

```bash
cargo build --release --no-default-features
```

### Run the tests

```bash
cargo test                         # with the desktop app (default)
cargo test --no-default-features   # command-line tool only
```

### Linux notes

The desktop app uses a GPU-backed window and a native file dialog. You need a C toolchain and `pkg-config`
(on Debian or Ubuntu: `sudo apt install build-essential pkg-config`). If the build fails on a missing system
library, install that library's development package and build again.

---

## Install

The program does not need an installer. Copy the binary to any folder you like, and make sure
ffmpeg is available (see below).

**Windows**

1. Copy `crunchyroll-downloader.exe` and/or `crunchyroll-downloader-gui.exe` to a folder such as
   `C:\Tools\crunchyroll-downloader`.
2. Install ffmpeg. Either add it to your `PATH`, or place `ffmpeg.exe` in the same folder as the program.
   Use a complete build such as the "essentials" zip from [gyan.dev](https://www.gyan.dev/ffmpeg/builds/).
3. Put your Widevine device files (see below) in the same folder or in the folder you run the tool from.

**Linux and macOS**

1. Copy the binaries to a folder on your `PATH`, for example `~/.local/bin`.
2. Install ffmpeg with your package manager, for example `sudo apt install ffmpeg` or `brew install ffmpeg`.

### Where ffmpeg is searched for

The tool looks in this order:

1. The custom path you set in the desktop app's **Settings > ffmpeg** (if any).
2. Your `PATH`.
3. The current folder.
4. The folder that contains the program.

It runs `ffmpeg -version` before any download starts, so a broken install is reported before you wait.

---

## Get your credentials

### The `etp_rt` cookie

The tool logs in with the `etp_rt` cookie from your browser, the same way the website does.

1. Log in to crunchyroll.com in your browser.
2. Open the developer tools (usually `F12`).
3. Go to the cookies storage for `crunchyroll.com` (in Chrome and Edge: **Application > Cookies**;
   in Firefox: **Storage > Cookies**).
4. Find the cookie named `etp_rt` and copy its **Value**.

Treat this cookie like a password. Anyone who has it can use your account. Do not share it or commit it to Git.

### The Widevine device

Decryption needs a Widevine device, which you must provide yourself. The tool accepts either form:

- a single **`.wvd`** file, or
- **`client_id.bin`** and **`private_key.pem`** together.

If both are present, the `.wvd` file is used. The tool looks in the folder you run it from first, then in the folder that
contains the program.

---

## Command-line usage

### Quick start

Download one episode with your cookie:

```bash
crunchyroll-downloader --url "https://www.crunchyroll.com/watch/XXXXXXXX/episode-title" --etp-rt "<your etp_rt value>"
```

Download a season of a series:

```bash
crunchyroll-downloader --url "https://www.crunchyroll.com/series/GJ0H7Q5ZJ/hells-paradise" --season 1 --etp-rt "<your etp_rt value>"
```

To keep the cookie out of your shell history, set it as an environment variable instead:

```powershell
# PowerShell
$env:CR_ETP_RT = "<your etp_rt value>"
crunchyroll-downloader --url "https://www.crunchyroll.com/series/GJ0H7Q5ZJ/hells-paradise" --season 1
```

```bash
# bash / zsh
export CR_ETP_RT="<your etp_rt value>"
crunchyroll-downloader --url "https://www.crunchyroll.com/series/GJ0H7Q5ZJ/hells-paradise" --season 1
```

If both `--etp-rt` and `CR_ETP_RT` are set, the flag wins.

### Options

| Option | Default | What it does |
|---|---|---|
| `--url <link>` | | An episode (`/watch/`) or series (`/series/`) link. |
| `--file <path>` | | A text file with one link per line. |
| `--season <n>` | `0` | Season number to download from a series. `0` means every season. |
| `--audio-lang <codes>` | `ja-JP` | Audio languages, comma-separated, or `ALL`. |
| `--subs-lang <codes>` | `en-US` | Subtitle languages, comma-separated, or `ALL`. |
| `--cc-lang <codes>` | none | Closed-caption languages, comma-separated, or `ALL`. |
| `--video-quality <height>` | `1080p` | Video height, for example `720p`. |
| `--audio-quality <bitrate>` | `192k` | Audio bitrate, for example `128k`. |
| `--etp-rt <cookie>` | | Your `etp_rt` cookie. Or set `CR_ETP_RT`. |
| `--download-delay <time>` | `0s` | Minimum pause between episodes, for example `30s` or `2m`. |
| `--output-dir <folder>` | `.` | Folder to save into. |
| `--debug-manifest` | off | Print raw playback data and error details. Useful for bug reports. |

Single-dash forms such as `-url` and `-etp-rt` also work.

Get help at any time:

```bash
crunchyroll-downloader --help          # all options, grouped by topic
crunchyroll-downloader help            # list the help topics
crunchyroll-downloader help audio-lang # explain one option with examples
crunchyroll-downloader help languages  # every language code
crunchyroll-downloader help errors     # common errors and how to fix them
```

### Language codes

Use the codes from `crunchyroll-downloader help languages`. Common ones:

| Code | Language |
|---|---|
| `ja-JP` | Japanese |
| `en-US` | English (US) |
| `en-IN` | English (India) |
| `es-419` | Spanish (Latin America) |
| `pt-BR` | Portuguese (Brazil) |
| `fr-FR` | French |
| `de-DE` | German |
| `th-TH` | Thai |

Separate several codes with commas, for example `--subs-lang en-US,th-TH`.

### Examples

Dubbed in English, with English and Thai subtitles, saved to a folder:

```bash
crunchyroll-downloader --url "<series link>" --season 2 --audio-lang en-US --subs-lang en-US,th-TH --output-dir ~/Anime
```

Every language, 720p, 30 seconds between episodes:

```bash
crunchyroll-downloader --url "<series link>" --audio-lang ALL --subs-lang ALL --video-quality 720p --download-delay 30s
```

Many links from a file, one per line:

```text
https://www.crunchyroll.com/watch/AAAAAAAA/episode-1
https://www.crunchyroll.com/series/BBBBBBBB/another-show
```

```bash
crunchyroll-downloader --file links.txt --etp-rt "<your etp_rt value>" --download-delay 60s
```

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Everything succeeded. |
| `1` | At least one episode failed, or a fatal error stopped the run. |
| `2` | Invalid option, for example a bad `--download-delay`. |

A single failed episode does not stop the run. The tool waits for the delay, continues with the next episode,
and lists every failure in the summary at the end.

---

## Desktop app usage

Start `crunchyroll-downloader-gui`. The first time you open it, you create a vault:

1. **Create a password** (at least 8 characters). Your settings and secrets are stored in an encrypted vault file.
   **If you forget this password, the data cannot be recovered.**
2. Open **Settings > Account and device** and paste your `etp_rt` value, and add your Widevine device.
   The checklist on the Download page shows what is still missing.
3. Go to the **Download** page, paste an episode or series link and press **Fetch**.
4. Tick the episodes or whole seasons you want, then press **Download**.
5. Watch the progress for each track. Each episode appears in the **Queue**, where you can cancel a waiting
   episode, retry a failed one, or open the output folder.

### Pages

- **Download:** the setup checklist, the live progress, the queue, the link box and the episode list.
- **Settings:** languages, quality, output folder, delay, speed, ffmpeg, auto-lock, debug output, account and password.
- **Logs:** every step the tool takes. Export the full log to a text file from here.

### Keyboard

- In an open dropdown: `↑` and `↓` move, `Enter` picks, `Esc` closes.
- On the lock screen, the password field is focused when the app starts. `Enter` unlocks.

### Auto-lock

The vault locks after a period with no activity (10 minutes by default). Clicks, scrolling and key presses count as
activity. Moving the mouse alone does not. Auto-lock never happens during a download. You can change or turn it off
in **Settings > Security and advanced** (`0` means never).

### Where the vault is stored

| Operating system | Path |
|---|---|
| Windows | `%APPDATA%\crunchyroll-downloader\vault.bin` |
| Linux | `~/.config/crunchyroll-downloader/vault.bin` |
| macOS | `~/Library/Application Support/crunchyroll-downloader/vault.bin` |

The file is encrypted with a key derived from your password (Argon2id, then AES-256-GCM). Writes are atomic,
so an interrupted save does not corrupt it.

---

## Settings reference

These settings are available in the desktop app. Command-line equivalents are noted where they exist.

| Setting | Default | Notes |
|---|---|---|
| Audio languages | `ja-JP` | Same as `--audio-lang`. |
| Subtitle languages | `en-US` | Same as `--subs-lang`. |
| Caption languages | none | Same as `--cc-lang`. |
| Video quality | `1080p` | Same as `--video-quality`. The desktop app also offers "Best available". |
| Audio quality | `192k` | Same as `--audio-quality`. |
| Output folder | your chosen folder | Same as `--output-dir`. |
| Delay between episodes | `0s` | Same as `--download-delay`. |
| Segments at once | `10` | Parallel segment downloads per track. Range 1 to 16. Lower it to be gentler on the service. |
| Audio dubs at once | `2` | Dubs downloaded at the same time. Range 1 to 4. |
| ffmpeg path | automatic | Choose `ffmpeg.exe` or a folder, or go back to automatic search. |
| Auto-lock | `10` minutes | `0` turns it off. |
| Debug output | off | Same as `--debug-manifest`. |

---

## Output and file names

- Each episode is saved as an **MKV** file in the output folder.
- While an episode is downloading, it is written to `<name>.part.mkv`. The file is renamed to its final name
  only after ffmpeg finishes, so an unfinished file never looks complete.
- If a run is interrupted, the leftover `.part.mkv` is removed the next time that episode starts.
- File names are cleaned so they are valid on your operating system.

---

## Troubleshooting

| Message or problem | What to do |
|---|---|
| `FFmpeg wasn't found` | Install ffmpeg and add it to your `PATH`, or place `ffmpeg.exe` next to the program. In the desktop app, choose the file under Settings > ffmpeg. |
| `ffmpeg does not work` / exit code `0xc0000135` on Windows | A DLL is missing. Use a complete ffmpeg build, such as the "essentials" zip. |
| `no widevine device provided` | Add a `.wvd` file, or `client_id.bin` and `private_key.pem`, to the folder you run from or the program's folder. |
| `Login failed` | Your `etp_rt` cookie is wrong or has expired. Copy it again from your browser after logging in. |
| `not a Crunchyroll episode or series link` | Use a link containing `/watch/` or `/series/`. |
| The title is not available | Check that your account can watch it in your region and subscription. |
| Rate-limited or repeated retries | Add `--download-delay 30s` (or more), and lower the parallel segment count in the desktop app. |
| A file is incomplete or has `.part` in its name | The run was interrupted. Run again; the partial file is replaced. |

For more detail on any error, run with `--debug-manifest` and see `crunchyroll-downloader help errors`.

---

## How it works

For each episode, the tool:

1. Logs in with your `etp_rt` cookie and gets an access token.
2. Reads the episode's metadata, its available audio versions and its subtitles.
3. Reads the DASH manifest and picks the video and audio tracks that match your quality and language settings.
4. Requests a Widevine license for the content key, using your device.
5. Downloads the encrypted segments in parallel and writes them in order.
6. Decrypts the media in memory, in chunks (CENC, AES-CTR or AES-CBC).
7. Uses ffmpeg to merge the decrypted video, audio and subtitle tracks into one MKV.

Episodes are downloaded one after another, because a single progress view follows one episode, and because
Crunchyroll limits how many requests a client may make.

---

## Project layout

```
src/
├── main.rs          Command-line entry point: flags, URLs, batch files, season choice
├── lib.rs           Library root shared by both programs
├── api.rs           Login, access tokens, season and episode metadata, playback requests
├── episode.rs       Language resolution, the per-episode flow and the run summary
├── download.rs      Parallel segment downloads, written in order
├── mpd.rs           DASH manifest parsing
├── cenc.rs          CENC decryption of fragmented MP4
├── drm.rs           PSSH handling and content key selection
├── cdm.rs           Widevine device loading, license challenge and license parsing
├── ffmpeg.rs        ffmpeg lookup, verification and MKV merge
├── backoff.rs       Retry and rate-limit backoff
├── progress.rs      Progress bars and shared run state
├── vault.rs         Encrypted settings vault
├── util.rs          Language tables, URL parsing, file name cleaning
├── help.rs          The `help` topics
├── ui.rs / log.rs   Console and log output
└── gui/             Desktop app (iced): app state, views, theme, dropdown, engine
tests/fixtures/      Test data for decryption
```

---

## Testing

```bash
cargo test
```

The tests cover URL and language handling, retry and backoff, segment ordering, progress, DASH parsing,
PSSH and key selection, CENC decryption, Widevine license building and parsing, the vault format, ffmpeg lookup and
the GUI's queue and idle-lock logic.

Some things can only be checked with a real account and network access, so they are not covered by automated tests:

- A live login, playback and license exchange with Crunchyroll.
- Decryption of a real encrypted Crunchyroll file. Decryption is tested with a generated fixture.

---

## Differences from the Go version

- **Decryption scope.** Only the `cenc` (AES-CTR) and `cbcs` (AES-CBC) schemes are implemented. Other schemes fail
  with a clear error. Only fragmented MP4 is supported, as in the original.
- **Exit codes.** The Go version panics on some fatal errors (exit code 2 with a stack trace). This version prints the
  error and exits with code `1`.
- **Empty seasons.** An empty season is skipped with no output. The Go version panics.
- **Help text.** Generated by clap, so the layout differs from Go's `flag` output.

---

## License

MIT. See the license information in `Cargo.toml`.
