# crunchyroll-downloader

A Rust tool that downloads episodes and films from Crunchyroll for **personal, offline use
with the owner's own account**. It ships as a desktop GUI (iced), a CLI, and a small WebView2
login helper, and handles the whole pipeline: sign-in, catalog browsing, manifest parsing,
Widevine decryption, and muxing into a finished `.mkv`.

> **Scope / ethics.** A personal tool: it uses the owner's own Crunchyroll login to save
> content their account can already watch, for offline viewing. It has no sharing or
> redistribution features. Keep it that way.

Primary platform: **Windows** (the GUI and CLI also build on Linux; the login helper is
Windows-focused because it uses WebView2).

---

## Handoff status — read this first

**Current version: v6.** All of the code below compiles and the test suite passes, but the
latest features have **not yet been run on a real Windows machine against the live
Crunchyroll API**. The owner will test next. Expect fixes in these areas:

| Area | Verified | Not yet verified |
| --- | --- | --- |
| Library, CLI, GUI | `cargo check` clean (no warnings), `cargo test --lib` = **115 passed** | Real runs on Windows |
| `login-helper` (wry/WebView2) | Code reviewed only — **never compiled** (dev sandbox was Linux) | Build on Windows, cookie capture |
| Browse / search / genres | Parsing unit-tested with sample JSON | Real `/discover/*` responses |
| Movies (`movie_listing`) | Parsing + naming unit-tested | Real `/cms/movie_listings/*` and playback of films |
| ffmpeg auto-download | Compiles | Real download + unzip from gyan.dev |
| Desktop notification | Compiles (`notify-rust`) | Toast appearance on Windows |

If something breaks during testing, the **Logs tab** (or `--debug-manifest` in the CLI) has
the details; fix against the real JSON rather than guessing.

---

## Goals

- **Easy:** pick a show from a poster grid (Browse tab) or paste a link; one click per
  episode, season, film, or everything. No digging cookies out of DevTools (in-app login),
  no hunting for ffmpeg (one-click install).
- **Fast:** segments download concurrently; with a proxy configured, only API calls use it
  while the heavy CDN downloads go direct (geo-unlock without throttling).
- **Safe credentials:** login cookie and Widevine device live in a password-encrypted,
  auto-locking vault; secrets are zeroized in memory and never logged.
- **Calm GUI:** Crunchyroll-orange theme, clear next-step buttons, plain-language errors.

---

## Features

### GUI tabs
- **Download** — paste an episode/series/movie link → Fetch → tick episodes or whole
  seasons → Download. A "Before your first download" checklist offers the one action each
  missing item needs: **Log in**, **Add device**, **Download ffmpeg** (with live %).
  Live progress, a queue, "Open folder", taskbar flash + desktop notification when done.
- **Browse** — poster grid of the catalog: **Series / Movies**, **Popular / Newest / A–Z**,
  **genre** filter, **Load more** (36 per page), and **search as you type** (series and
  movies). Click a card for a detail page (poster, description, facts) with every download
  option: one episode (⬇ on its row), a whole season (**Season**), ticked episodes, or
  everything (**Select all**). Progress and queue show on the detail page too.
- **Settings** — languages (searchable dropdowns), quality, output folder (Browse/Open),
  delay, notify toggle, speed (segment/track workers, API proxy), ffmpeg (status, download,
  choose), account (log in / switch / sign out / paste cookie), Widevine device, auto-lock
  (5/10/20/30 min/Never), **portable mode**, debug output, and a "where things are stored"
  summary.
- **Logs** — every step with reasons and hints; export to a file.

### Behaviour worth knowing
- Episodes whose finished file already exists show a green **Downloaded** badge, are not
  ticked by "Select all", and are skipped by the downloader. Unfinished `*.part.mkv`
  files are cleaned up on the next run.
- A new cookie (from the login window or pasted) is **checked with Crunchyroll before it is
  saved**. A refused cookie is never stored; a network failure stores it with a warning.
- If a fetch is refused because the login expired, the app says so and offers **Log in**.
- Temp leftovers from a crash (`crdl-*`, `ffmpeg-dl-*`, `cr-login-*` older than 6 h) are
  removed at startup.

### CLI
Everything except Browse and the login window. See "CLI reference" below.

---

## Build & run

```sh
# Workspace: one command builds all three binaries into target/release/
cargo build --release
#   crunchyroll-downloader(.exe)      CLI
#   crunchyroll-downloader-gui(.exe)  GUI
#   cr-login-helper(.exe)             login window — keep it next to the GUI exe

# Just one
cargo build --release -p crunchyroll-downloader          # CLI + GUI
cargo build --release -p cr-login-helper                 # helper only (Windows)
cargo build --release -p crunchyroll-downloader --no-default-features   # CLI without GUI deps
```

On Linux (or any dev box without WebView2), check and test **only the main package** — a
bare `cargo build` would also try to build the helper:

```sh
cargo check -p crunchyroll-downloader --bins
cargo test  -p crunchyroll-downloader --lib
```

### Requirements
- Rust stable.
- A Crunchyroll account (email + password — Crunchyroll's web login has no Google/Apple
  sign-in; accounts made that way can set a password via "Forgot password").
- A Widevine CDM device: `client_id.bin` + `private_key.pem`, or one `.wvd` file.
- ffmpeg — or let the GUI install it.
- For the login helper: WebView2 Runtime (built into Windows 11, usually present on 10).

---

## Architecture

```
crunchyroll-downloader-gui (iced)  ──spawns──►  cr-login-helper (wry/WebView2, own process)
        │  uses                                   prints ETP_RT=<cookie> on stdout, exits
        ▼
library crate (src/lib.rs)  ◄── crunchyroll-downloader (CLI, clap)
```

### Library (`src/`)

| Module | Responsibility |
| --- | --- |
| `api` | Crunchyroll client. `wreq` with **Firefox TLS/HTTP2 emulation** (Cloudflare). Token from `etp_rt` (`/auth/v1/token`), refresh on 401. Two clients: `http` (API, proxied) and `cdn_http` (segments, direct). Seasons, episodes, playback, `episode_info` (episodes **and** films), `movie_listing`, `browse`, `search`, `genres`, and `movie_episode()` (film → episode). |
| `browse` | Discover endpoints → `CatalogItem` cards (`Kind::Series`/`Movie`), `Sort`, `Show`, `Genre`. Parses JSON **loosely** (`serde_json::Value`) so a renamed field degrades one card, not the page. |
| `episode` | One episode end to end: pick versions/tracks, download, decrypt, mux. `output_path()` is the single naming rule (shared with the GUI). `guid_by_locale`, `info_from_season_episode`, `episode_label`. |
| `download` | Concurrent segment downloader (`MAX_WORKERS` = 10 CLI default; GUI setting default 20). |
| `mpd` / `drm` / `cdm` / `cenc` | DASH manifest, PSSH/license, Widevine L3 CDM, CENC decryption. |
| `ffmpeg` | Locate (setting → next to program → PATH), verify, mux. Hides console windows. |
| `vault` | Encrypted storage, settings, portable mode, default output folder. |
| `runner` | CLI orchestration (URL kinds: `watch`, `series`, `movie_listing`). |
| `util` | `parse_url`, `sanitize_filename`, language helpers. |
| `backoff`, `progress`, `log`, `ui`, `help` | Retry, progress state, logging, terminal UI, `help` topics. |

### GUI (`src/gui/`)

| Module | Responsibility |
| --- | --- |
| `app` | State (`App`, `BrowseState`, `LoginPhase`, `FfmpegDlEvent`, `Poster`), the `Message` enum, `update`, async helpers (login helper runner, ffmpeg download stream, temp cleanup, toast). |
| `views` | All screens. Reusable pieces: `setup_row`, `catalog_card` (episode picker used by Download and Browse detail), `select`/`quality_pick` (dropdowns), `series_card`, `poster_view`. |
| `engine` | Bridges GUI → library: `fetch`, `run`, `verify_login`, `browse`, `poster`. `LOGIN_EXPIRED` marker. |
| `theme` | **All** colors, icons (Material Symbols codepoints), widget styles. Accent `#F47521`. |
| `dropdown` | Overlay list widget used by `select`. |
| `logs` | Log buffer for the Logs tab. |

### `login-helper/`
Separate process so WebView2's event loop never collides with iced's. Opens
`https://www.crunchyroll.com/login` with an **ephemeral user-data folder** (temp dir, wiped
on exit), polls `cookies_for_url` every second for the HttpOnly `etp_rt`, prints
`ETP_RT=<value>`, exits. Exit codes: `0` captured · `2` window closed · `3` timeout (300 s) ·
`1` error. Built as a GUI-subsystem exe (no console flash); the GUI also spawns it with
`CREATE_NO_WINDOW`. Pinned `wry = 0.52`, `tao = 0.31` — if a version bump breaks it, check
`WebViewBuilder::new_with_web_context`, `with_url`, `cookies_for_url`, `run_return`.

---

## Key flows

**Download:** `etp_rt` → token → fetch catalog (`/series/<id>` seasons → episodes, or
`/movie_listing/<id>` films, or one `/watch/<id>`) → per episode: playback JSON → MPD →
Widevine license → keys → concurrent segment download (CDN, direct) → decrypt → subtitles
→ ffmpeg mux to `*.part.mkv` → rename to final name.

**Films:** a movie listing's films become `SeasonEpisode`s with `is_movie = true`
(season 0, episode index). They use the exact episode pipeline. A film with no `versions`
and no `audio_locale` is assumed `ja-JP` and played by its own id.

**Login:** Log in → spawn helper (`LoginPhase::Waiting`) → cookie on stdout (zeroized) →
`engine::verify_login` (`LoginPhase::Verifying`) → `LoginVerified` → store only if it works
(`AuthRejected` ⇒ not saved).

**Browse:** `BrowseQueryInput` bumps `seq` and fires after 400 ms; results carry `seq` and
stale ones are dropped. The signed-in `Api` is cached in `BrowseState` and **dropped on
lock**. Genres are requested once with the first page. Posters load from the image CDN
(direct, `format=auto` rewritten to `format=jpeg`) and are cached per session.

**ffmpeg install:** gyan.dev `ffmpeg-release-essentials.zip` streamed with progress
(`Task::run` + `async_stream`), `bin/ffmpeg.exe` extracted, path saved to settings.

---

## Security model

- Vault: **Argon2id** key from the user's password + random salt; **AES-256-GCM**, file
  header bound as AAD. Holds `Settings` and `Secrets` (`etp_rt`, `client_id`,
  `private_key_pem`, `wvd`). `Secrets` zeroize on drop.
- Secrets are never printed; the UI shows only "saved / not set". `Message` derives `Clone`
  only (no `Debug`), and cookies travel inside `Zeroizing<String>`.
- The helper keeps no browser session (ephemeral profile, wiped).
- Auto-lock after inactivity (5/10/20/30 min/Never). Locking also drops the cached Browse
  session (live access token).

---

## Where files are stored (Windows)

| What | Normal mode | Portable mode (`portable.txt` next to the exe) |
| --- | --- | --- |
| Vault (login, device, settings — encrypted) | `%APPDATA%\crunchyroll-downloader\vault.bin` | `<exe folder>\vault.bin` |
| ffmpeg installed by the app | `%LOCALAPPDATA%\crunchyroll-downloader\ffmpeg\ffmpeg.exe` | `<exe folder>\ffmpeg.exe` |
| Episodes | `Downloads\Crunchyroll\<Series>\<Series> S01E01 - <Title> [quality].mkv` | same |
| Films | `Downloads\Crunchyroll\<Listing>\<Title> [quality].mkv` | same |
| Temp pieces | `%TEMP%\crdl-*`, `ffmpeg-dl-*`, `cr-login-*` — removed after use | same |

The output folder is a setting. Old vaults that stored `.` are migrated to
`Downloads\Crunchyroll` on unlock. Portable mode is toggled in Settings; the toggle moves
the vault and refuses to overwrite a different vault at the destination. The CLI still
defaults to `--output-dir .`.

---

## Network model

- `--proxy` (CLI, env `CR_PROXY`) / Settings → Speed → Proxy: HTTP/HTTPS/SOCKS5 for **API
  calls only** (login, token, metadata, license, browse). CDN segments, subtitles and
  posters always go direct.
- The catalog (Browse) reflects the API's IP region — a US proxy shows the US catalog.
- Browse titles use the first subtitle language as locale (default `en-US`).

---

## Settings reference (`vault::Settings`, all `#[serde(default)]`)

`output_dir`, `audio_langs`, `subs_langs`, `cc_langs`, `video_quality`, `audio_quality`,
`download_delay`, `debug`, `ffmpeg_path`, `segment_workers` (default 20), `track_workers`
(2), `auto_lock_minutes` (10; `0` = never), `last_url`, `last_picks`, `proxy_url`,
`notify_done` (true).

When adding a field: add it to the struct **and** to `impl Default for Settings`; older
vaults then load with the default automatically.

---

## CLI reference

```
--url <link>            episode, series or movie link (/watch/, /series/, /movie_listing/)
--file <list.txt>       one link per line
--season <n>            0 = every season (series links only)
--audio-lang ja-JP      comma-separated or ALL
--subs-lang  en-US      comma-separated or ALL
--cc-lang    <langs>    comma-separated or ALL
--video-quality 1080p   --audio-quality 192k
--etp-rt <cookie>       or env CR_ETP_RT
--download-delay 0s     e.g. 30s, 2m
--proxy <url>           API only; or env CR_PROXY
--output-dir .          --debug-manifest
```
The CLI finds the Widevine device as files (`client_id.bin` + `private_key.pem`, or a
`.wvd`) in the current folder and the other folders listed by `help cdm`.

---

## Conventions (for Claude Code and contributors)

- **Verify with** `cargo check -p crunchyroll-downloader --bins` and
  `cargo test -p crunchyroll-downloader --lib`. Keep both clean (no warnings).
- **Never log or print secrets.** Carry cookies in `Zeroizing<String>`; don't add `Debug`
  to `Message` or to structs holding secrets.
- **Undocumented endpoints → loose parsing** (`serde_json::Value` + defaults), with unit
  tests built from sample JSON (see `browse.rs`, `api::movie_tests`).
- **One naming rule:** change file names only in `episode::output_path` (the GUI's
  "Downloaded" badge depends on it).
- **Colors and icons only in `theme.rs`.** Dropdowns go through `DropdownId` +
  `dropdown_options` + `Message::Choose`.
- Long work in `update` goes into `Task::perform` / `spawn_blocking`, never inline.
- Windows-only code behind `#[cfg(windows)]` with a non-Windows fallback.
- Text in the UI: short, plain, tells the user the next action.

---

## Known gaps / ideas

- Fetching an episode list is blocked while a download runs (the Browse detail page says so).
- Desktop toasts may show "Windows PowerShell" as the sender (no registered AppUserModelID).
- Portable mode fails (with a clear error) if the exe sits in a read-only folder like
  Program Files.
- Movie listing link `/movie_listing/<id>` is this app's own form; it may not open on the
  website.
- Possible next steps: resume of partially downloaded episodes, AnimeShelf/rmp hand-off
  after download, registering an AppUserModelID for toasts.

---

## Manual test checklist (Windows)

1. `cargo build --release`; confirm all three exes; put `cr-login-helper.exe` next to the GUI.
2. Create a vault → **Log in** on the Download page → sign in → "Signed in" (cookie verified).
3. **Download ffmpeg** on the Download page → progress → ready.
4. Add the Widevine device.
5. Download tab: paste a series link → Fetch → download one episode (⬇).
6. Browse: Series/Movies, sorts, genre, Load more, search; open a series → **Season**; open a
   film → download.
7. Re-fetch → **Downloaded** badges; Select all skips them.
8. Settings: Open folder, notify toggle (toast), auto-lock, portable on/off (vault moves).
9. Optional: US proxy → Browse shows the US catalog; downloads stay fast.

---

## Version history

- **v1** Proxy split (API via proxy, CDN direct), 20 segment workers.
- **v2** WebView2 login helper; auto-lock presets.
- **v3** Crunchyroll theme + logo; searchable language dropdowns; flat Logs tab; ffmpeg
  one-click install; Download-page checklist with Log in / Download; login verification,
  expiry handling, sign out / switch account.
- **v4** Default output `Downloads\Crunchyroll` (+ migration); temp cleanup; desktop
  notification; "Downloaded" badges; portable mode; storage summary.
- **v5** Browse tab: poster grid, search, sorts, detail page, download one/season/all.
- **v6** Movies (movie listings, `/watch/` films, film naming); Series/Movies filter; genre
  filter; search covers movies.

---

## Repository layout

```
Cargo.toml              workspace root + main package (CLI, GUI, library)
src/                    library + CLI (main.rs) — see module table
src/gui/                GUI (main.rs, app.rs, views.rs, engine.rs, theme.rs, dropdown.rs, logs.rs)
src/gui/assets/         fonts (Roboto, Material Symbols) and logo.png (window icon)
tests/fixtures/         sample data for tests
login-helper/           WebView2 login helper (workspace member)
```
