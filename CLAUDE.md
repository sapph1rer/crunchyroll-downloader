# CLAUDE.md

Read `HANDOFF.md` first — it is the full project handoff (status, architecture, flows,
conventions, test checklist). Key rules in short:

- Owner works on **Windows**; replies to the owner in **Thai**.
- Verify every change with:
  - `cargo check -p crunchyroll-downloader --bins` (must be warning-free)
  - `cargo test -p crunchyroll-downloader --lib`
  - `login-helper` only builds on Windows (WebView2); don't build the whole workspace on Linux.
- The latest features (login helper, Browse, movies, ffmpeg auto-install, toasts) are
  **untested against the live API**. When the owner reports a failure, ask for the Logs-tab
  text and fix against the real JSON.
- Never log or print secrets; carry cookies in `Zeroizing<String>`; no `Debug` on `Message`.
- Undocumented Crunchyroll endpoints are parsed loosely with unit tests from sample JSON.
- File naming lives only in `episode::output_path`. Colors/icons only in `gui/theme.rs`.
- Personal-use tool for the owner's own account: no sharing/redistribution features.
