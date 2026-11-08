//! Per-episode orchestration: resolve dubs, fetch playback, download tracks in
//! parallel, merge. Port of download.go (downloadEpisode & friends) and
//! episode.go (getEpisode/getEpisodeInfo).

use crate::api::{Api, ApiError, Episode, EpisodeInfo, EpisodeMetadata, SeasonEpisode, Subtitle};
use crate::backoff::{fmt_duration, Backoff};
use crate::download::{download_subs, download_track};
use crate::drm::{self, ContentKey};
use crate::ffmpeg::{self, MediaTrack};
use crate::mpd::Mpd;
use crate::progress::EpisodeState;
use crate::ui;
use crate::util::{expand_langs, requests_all, sanitize_filename, track_title};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use futures::future::try_join_all;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct Options {
    pub audio_langs: Vec<String>,
    pub subs_langs: Vec<String>,
    pub cc_langs: Vec<String>,
    pub video_quality: String,
    pub audio_quality: String,
    pub debug: bool,
    pub output_dir: PathBuf,
    /// Segments fetched at once for each track.
    pub segment_workers: usize,
    /// Audio dubs of one episode downloaded at once.
    pub track_workers: usize,
}

/// Resolves a locale -> dub GUID map from the episode's versions. Falls back to
/// the episode's own content ID only when the API lists no versions.
pub fn guid_by_locale(info: &EpisodeInfo, base_id: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for v in &info.episode_metadata.versions {
        m.insert(v.audio_locale.clone(), v.guid.clone());
    }
    if m.is_empty() && !info.episode_metadata.audio_locale.is_empty() {
        m.insert(info.episode_metadata.audio_locale.clone(), base_id.to_string());
    }
    m
}

/// Unselected dubs' GUIDs, ordered by locale; empty locales and GUIDs skipped.
pub fn additional_version_guids(m: &BTreeMap<String, String>, selected: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (locale, guid) in m {
        if !locale.is_empty() && !guid.is_empty() && !selected.contains(guid) && !out.contains(guid) {
            out.push(guid.clone());
        }
    }
    out
}

/// Union of subtitle/caption tracks across playbacks, first usable URL per locale.
pub fn merge_tracks(
    episodes: &[Episode],
    pick: impl Fn(&Episode) -> &HashMap<String, Option<Subtitle>>,
) -> BTreeMap<String, Subtitle> {
    let mut out: BTreeMap<String, Subtitle> = BTreeMap::new();
    for ep in episodes {
        for (locale, sub) in pick(ep) {
            if let Some(sub) = sub {
                let need = out.get(locale).map(|s| s.url.is_empty()).unwrap_or(true);
                if need {
                    out.insert(locale.clone(), sub.clone());
                }
            }
        }
    }
    out
}

/// Locales with a downloadable file (empty locale and empty URL excluded).
pub fn available_locales(tracks: &BTreeMap<String, Subtitle>) -> Vec<String> {
    tracks
        .iter()
        .filter(|(l, s)| !l.is_empty() && !s.url.is_empty())
        .map(|(l, _)| l.clone())
        .collect()
}

/// Drops locales the episode does not offer, warning about each one.
pub fn filter_available(langs: &[String], tracks: &BTreeMap<String, Subtitle>, kind: &str, ep: i64) -> Vec<String> {
    langs
        .iter()
        .filter(|l| {
            let ok = tracks.contains_key(*l);
            if !ok {
                crate::say!("! {kind} locale {l} is not available for episode {ep}, skipping it.");
            }
            ok
        })
        .cloned()
        .collect()
}

/// Parses one playback's DASH manifest, normalises its PSSH and requests the
/// content keys for it.
async fn keys_for_playback(api: &Api, ep: &Episode, guid: &str, locale: &str) -> Result<(Mpd, Vec<ContentKey>)> {
    let body = api
        .http()
        .get(&ep.manifest_url)
        .header("Authorization", api.bearer().await)
        .header("User-Agent", crate::api::USER_AGENT)
        .send()
        .await
        .with_context(|| format!("parsing manifest for {locale}"))?
        .text()
        .await?;
    let manifest = Mpd::parse(&body).with_context(|| format!("parsing manifest for {locale}"))?;
    let pssh = manifest
        .pssh()
        .ok_or_else(|| anyhow!("PSSH not found for {locale}"))?;
    // Crunchyroll wraps Widevine data in a non-Widevine PSSH; normalise before use.
    let raw = base64::engine::general_purpose::STANDARD.decode(&pssh)?;
    let normalised = drm::to_widevine_pssh(&raw)?;
    let keys = drm::request_content_keys(api, &normalised, guid, &ep.token)
        .await
        .with_context(|| format!("getLicense for {locale}"))?;
    Ok((manifest, keys))
}

pub fn is_rate_limited(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<ApiError>(), Some(ApiError::RateLimited))
}

/// Downloads one episode. Returns Err on failure; rate-limit errors are
/// detectable with `is_rate_limited`.
pub async fn download_episode(
    api: &Api,
    backoff: &Backoff,
    base_id: &str,
    info: &EpisodeInfo,
    opts: &Options,
) -> Result<Outcome> {
    let series = sanitize_filename(&info.episode_metadata.series_title);
    let title = sanitize_filename(&info.title);
    let dir = opts.output_dir.join(&series);
    if tokio::fs::metadata(&dir).await.is_err() {
        let _ = tokio::fs::create_dir_all(&dir).await;
    }
    let output = dir.join(format!(
        "{series} S{:02}E{:02} - {title} [{}].mkv",
        info.episode_metadata.season_number,
        info.episode_metadata.episode_number,
        opts.video_quality
    ));
    if output.exists() {
        return Ok(Outcome::Skipped("already downloaded".into()));
    }

    // Resolve each requested audio locale to its version GUID. Each dub is a
    // separate playback stream with its own manifest, token and Widevine keys.
    let by_locale = guid_by_locale(info, base_id);
    let available_audio: Vec<String> = by_locale
        .iter()
        .filter(|(l, g)| !l.is_empty() && !g.is_empty())
        .map(|(l, _)| l.clone())
        .collect();
    let audio_langs = expand_langs(&opts.audio_langs, available_audio, "ja-JP");

    let mut versions: Vec<(String, String)> = Vec::new(); // (locale, guid)
    for locale in &audio_langs {
        match by_locale.get(locale) {
            Some(g) => versions.push((locale.clone(), g.clone())),
            None => crate::say!(
                "! Audio locale {locale} is not available for episode {}, skipping it.",
                info.episode_metadata.episode_number
            ),
        }
    }
    if versions.is_empty() {
        return Ok(Outcome::Skipped(format!(
            "none of the requested audio languages ({}) is available",
            audio_langs.join(", ")
        )));
    }

    ui::title(format!(
        "Downloading {} (S{:02}E{:02}) from {}",
        info.title, info.episode_metadata.season_number, info.episode_metadata.episode_number, info.episode_metadata.series_title
    ));

    // Remove a leftover partial from an interrupted run. ffmpeg runs without -y, so a
    // stale file would make it refuse to overwrite.
    let _ = tokio::fs::remove_file(crate::ffmpeg::part_path(&output)).await;

    // Space this download out from the previous one, if a delay is configured.
    backoff.wait().await;
    let result = download_inner(api, info, opts, &by_locale, &audio_langs, &versions, &output).await;
    // Start the backoff clock as soon as this download is over, success or not.
    backoff.done();
    result.map(|()| Outcome::Downloaded)
}

async fn download_inner(
    api: &Api,
    info: &EpisodeInfo,
    opts: &Options,
    by_locale: &BTreeMap<String, String>,
    audio_langs: &[String],
    versions: &[(String, String)],
    output: &Path,
) -> Result<()> {
    if crate::progress::cancel_requested() {
        bail!("cancelled by you");
    }
    // Every playback token we opened and have not yet released (content ID -> token).
    // Versions release their own stream as soon as their tracks are downloaded.
    let streams: Mutex<HashMap<String, String>> = Mutex::new(HashMap::new());
    let result = download_versions(api, info, opts, by_locale, audio_langs, versions, output, &streams).await;

    crate::say!("Cleaning up...");
    let remaining: Vec<(String, String)> = streams.lock().unwrap().drain().collect();
    for (id, tok) in remaining {
        if let Err(e) = api.delete_stream(&id, &tok).await {
            crate::say!("Failed to remove the player stream for {id}: {e}");
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn download_versions(
    api: &Api,
    info: &EpisodeInfo,
    opts: &Options,
    by_locale: &BTreeMap<String, String>,
    audio_langs: &[String],
    versions: &[(String, String)],
    output: &Path,
    streams: &Mutex<HashMap<String, String>>,
) -> Result<()> {
    // One video track (from the first version) plus one audio track per version.
    crate::progress::set_tracks_expected(versions.len() + 1);
    // Fetch selected dubs' playback up front. Captions are per-audio-locale,
    // and the first version alone may not contain the requested captions.
    let mut episodes: Vec<Episode> = Vec::with_capacity(versions.len());
    let mut selected: Vec<String> = Vec::with_capacity(versions.len());
    for (_, guid) in versions {
        let ep = api.playback(guid, opts.debug).await?;
        streams.lock().unwrap().insert(guid.clone(), ep.token.clone());
        episodes.push(ep);
        selected.push(guid.clone());
    }
    if requests_all(&opts.subs_langs) || requests_all(&opts.cc_langs) {
        // ALL covers captions attached to dubs whose audio was not selected. Keep
        // these playback tokens until the tracks have downloaded, but do not
        // download their audio streams.
        for guid in additional_version_guids(by_locale, &selected) {
            match api.playback(&guid, opts.debug).await {
                Ok(ep) => {
                    streams.lock().unwrap().insert(guid.clone(), ep.token.clone());
                    episodes.push(ep);
                }
                Err(ApiError::RateLimited) => return Err(anyhow::Error::new(ApiError::RateLimited)),
                Err(e) => crate::say!("! Playback {guid} is unavailable; skipping its subtitles and captions: {e}"),
            }
        }
    }

    // Merge subtitles and captions across versions. Subtitles (translation
    // scripts) are usually identical across versions, while captions are the
    // per-dub transcriptions that only exist on their own version.
    let subtitles = merge_tracks(&episodes, |e| &e.subtitles);
    let captions = merge_tracks(&episodes, |e| &e.captions);

    let subs_langs = expand_langs(&opts.subs_langs, available_locales(&subtitles), "en-US");
    let cc_langs = expand_langs(&opts.cc_langs, available_locales(&captions), "");

    let ep_no = info.episode_metadata.episode_number;
    crate::say!(
        "Audio locales: {} | Subtitle locales: {} | CC locales: {}",
        audio_langs.join(", "),
        subs_langs.join(", "),
        cc_langs.join(", ")
    );
    let subs_langs = filter_available(&subs_langs, &subtitles, "Subtitle", ep_no);
    let cc_langs = filter_available(&cc_langs, &captions, "Closed caption", ep_no);

    // Subtitle and caption download jobs.
    let sub_jobs: Vec<(String, Subtitle, bool)> = subs_langs
        .iter()
        .map(|l| (l.clone(), subtitles[l].clone(), false))
        .chain(cc_langs.iter().map(|l| (l.clone(), captions[l].clone(), true)))
        .collect();
    let sub_fut = try_join_all(sub_jobs.iter().map(|(locale, sub, is_cc)| {
        let http = api.http().clone();
        let (locale, format, url, is_cc) = (locale.clone(), sub_format(sub), sub.url.clone(), *is_cc);
        async move {
            let file = download_subs(&http, &url, &format)
                .await
                .with_context(|| format!("downloading subtitles for {}", track_title(&locale)))?;
            Ok::<_, anyhow::Error>(MediaTrack { file, locale, format, is_cc })
        }
    }));

    // Audio versions run side by side, up to `track_workers` at a time. The first
    // version also carries the video track, which is identical across dubs and is
    // downloaded only once.
    let track_gate = tokio::sync::Semaphore::new(opts.track_workers.max(1));
    let audio_fut = try_join_all(versions.iter().enumerate().map(|(i, (locale, guid))| {
        let ep = &episodes[i];
        let track_gate = &track_gate;
        async move {
            let _slot = track_gate.acquire().await.map_err(|e| anyhow!("{e}"))?;
            let (manifest, keys) = keys_for_playback(api, ep, guid, locale).await?;
            let workers = opts.segment_workers;
            let (video, audio) = if i == 0 {
                let (v, a) = tokio::try_join!(
                    download_track(api.http(), &manifest, true, &opts.video_quality, locale, &keys, workers),
                    download_track(api.http(), &manifest, false, &opts.audio_quality, locale, &keys, workers),
                )?;
                (Some(v), a)
            } else {
                (None, download_track(api.http(), &manifest, false, &opts.audio_quality, locale, &keys, workers).await?)
            };
            if let Err(e) = api.delete_stream(guid, &ep.token).await {
                crate::say!("Failed to remove the player stream for {locale}: {e}");
            }
            streams.lock().unwrap().remove(guid);
            Ok::<_, anyhow::Error>((MediaTrack { file: audio, locale: locale.clone(), format: String::new(), is_cc: false }, video))
        }
    }));

    let (subs, version_out) = tokio::try_join!(sub_fut, audio_fut)?;
    if !subs.is_empty() {
        crate::say!("Downloaded subtitles!");
    }
    let mut audio_tracks = Vec::with_capacity(version_out.len());
    let mut video: Option<PathBuf> = None;
    for (track, v) in version_out {
        if v.is_some() {
            video = v;
        }
        audio_tracks.push(track);
    }
    let video = video.ok_or_else(|| anyhow!("no video track"))?;
    ffmpeg::merge(&video, &audio_tracks, &subs, output, info).await?;
    Ok(())
}

/// What happened to one episode.
#[derive(Debug)]
pub enum Outcome {
    Downloaded,
    /// Not downloaded on purpose (already present, or no requested language exists).
    Skipped(String),
    Failed(Failure),
}

/// A failed episode, with the reason and a suggested fix when one is known.
#[derive(Debug)]
pub struct Failure {
    pub label: String,
    pub message: String,
    pub hint: Option<&'static str>,
}

/// Totals for a whole run, printed at the end.
#[derive(Debug, Default)]
pub struct Summary {
    pub downloaded: usize,
    pub skipped: Vec<(String, String)>,
    pub failed: Vec<Failure>,
}

impl Summary {
    pub fn record(&mut self, label: String, outcome: Outcome) {
        match outcome {
            Outcome::Downloaded => self.downloaded += 1,
            Outcome::Skipped(reason) => self.skipped.push((label, reason)),
            Outcome::Failed(f) => self.failed.push(f),
        }
    }

    /// Process exit code: 0 when nothing failed, 1 otherwise.
    pub fn exit_code(&self) -> i32 {
        if self.failed.is_empty() {
            0
        } else {
            1
        }
    }

    /// Prints the totals, then every skip and failure with its reason.
    pub fn print(&self) {
        ui::title("Summary");
        ui::ok(format!("{} downloaded", self.downloaded));
        if !self.skipped.is_empty() {
            ui::info(format!("{} skipped", self.skipped.len()));
            for (label, reason) in &self.skipped {
                ui::detail(format!("{label}: {reason}"));
            }
        }
        if self.failed.is_empty() {
            return;
        }
        ui::fail(format!("{} failed", self.failed.len()));
        for f in &self.failed {
            ui::detail(format!("{}: {}", f.label, first_line(&f.message)));
            if let Some(h) = f.hint {
                ui::detail(format!("  hint: {h}"));
            }
        }
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// Label used in messages, for example "S02E01 Title".
pub fn episode_label(info: &EpisodeInfo) -> String {
    format!(
        "S{:02}E{:02} {}",
        info.episode_metadata.season_number, info.episode_metadata.episode_number, info.title
    )
}

/// Suggests a fix for a known failure. Matched on the error chain, so the wording of
/// Go-compatible messages is what decides. Unknown errors get no hint.
pub fn hint_for(e: &anyhow::Error) -> Option<&'static str> {
    if let Some(api) = e.downcast_ref::<ApiError>() {
        return match api {
            ApiError::CloudflareBlocked(_) => {
                Some("Cloudflare blocked the request. Wait a while, switch network, then try again.")
            }
            ApiError::AuthRejected(_) => {
                Some("Your etp_rt cookie is wrong or expired. Log in again in the browser and copy the new value (see `help etp-rt`).")
            }
            ApiError::RateLimited => Some("Crunchyroll is rate-limiting you. Use a longer --download-delay."),
            ApiError::Other(_) => None,
        };
    }
    let m = format!("{e:#}").to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| m.contains(w));
    if has(&["no widevine device"]) {
        Some("Put a .wvd file, or client_id.bin and private_key.pem, in the folder you run from or next to the program (see `help cdm`).")
    } else if has(&["getlicense"]) {
        Some("The license server refused the request. Try again later. If it keeps happening, the device file may be revoked (see `help cdm`).")
    } else if has(&["pssh not found"]) {
        Some("Crunchyroll did not offer DRM for that audio language. It may not be released yet. Try another --audio-lang.")
    } else if has(&["video base url", "video quality", "no video representation"]) {
        Some("That --video-quality is not offered for this episode. Try 1080p, 720p or 480p (see `help video-quality`).")
    } else if has(&["audio base url", "audio quality", "no audio representation"]) {
        Some("That --audio-quality is not offered. Try 192k, 128k or 96k (see `help audio-quality`).")
    } else if has(&["ffmpeg failed"]) {
        Some("ffmpeg could not merge the streams. Read the ffmpeg message above. `ffmpeg -version` shows whether ffmpeg itself works.")
    } else if has(&["no license key", "decrypt"]) {
        Some("Decryption failed. The license key does not match this episode, or the encryption scheme is not supported.")
    } else if has(&["no space left", "permission denied", "read-only"]) {
        Some("Check that the disk has free space and the output folder is writable (see `help output-dir`).")
    } else if has(&["timed out", "connection", "dns error", "tcp connect"]) {
        Some("Network problem. Check your connection and try again.")
    } else {
        None
    }
}

/// Retries the same episode with growing waits whenever Crunchyroll rate-limits.
/// Other failures are printed at once and returned as `Outcome::Failed`.
pub async fn download_episode_with_retry(
    api: &Api,
    backoff: &Backoff,
    base_id: &str,
    info: &EpisodeInfo,
    opts: &Options,
) -> Outcome {
    let label = episode_label(info);
    loop {
        match download_episode(api, backoff, base_id, info, opts).await {
            Ok(outcome) => {
                backoff.reset_rate_limit();
                return outcome;
            }
            Err(e) if is_rate_limited(&e) => {
                let wait = backoff.rate_limit_wait();
                let retry_at = chrono::Local::now() + chrono::Duration::from_std(wait).unwrap_or_default();
                ui::warn(format!(
                    "Rate limited by Crunchyroll. Retrying {label} in {} (around {}).",
                    fmt_duration(wait),
                    retry_at.format("%-I:%M%p")
                ));
                crate::progress::pause(wait).await;
                if crate::progress::cancel_requested() {
                    return Outcome::Failed(Failure { label, message: "cancelled by you".into(), hint: None });
                }
            }
            Err(e) => {
                let message = if opts.debug { format!("{e:?}") } else { format!("{e:#}") };
                let failure = Failure { label: label.clone(), message, hint: hint_for(&e) };
                ui::error_block(format!("{label} failed"), &failure.message, failure.hint);
                return Outcome::Failed(failure);
            }
        }
    }
}

/// Downloads every episode of a season in order, recording each result in `summary`.
pub async fn download_season(
    api: &Api,
    backoff: &Backoff,
    opts: &Options,
    episodes: &[SeasonEpisode],
    summary: &mut Summary,
) {
    if let Some(first) = episodes.first() {
        ui::title(format!(
            "Season {} of {} ({} episodes)",
            first.season_number,
            first.series_title,
            episodes.len()
        ));
    }
    for ep in episodes {
        let info = info_from_season_episode(ep);
        let label = episode_label(&info);
        let outcome = download_episode_with_retry(api, backoff, &ep.id, &info, opts).await;
        summary.record(label, outcome);
    }
}

/// Downloads the given episodes in order, recording each result in `summary` and in the
/// shared run state the GUI reads. Episodes the user cancelled are skipped, and a cancel
/// during an episode stops it without counting as a failure.
/// Used by the GUI, where the user picks episodes from several seasons.
pub async fn download_episodes(
    api: &Api,
    backoff: &Backoff,
    opts: &Options,
    episodes: &[SeasonEpisode],
    summary: &mut Summary,
) {
    ui::title(format!("Downloading {} episode(s)", episodes.len()));
    let total = episodes.len();
    let ids: Vec<String> = episodes.iter().map(|e| e.id.clone()).collect();
    crate::progress::begin_run(&ids);
    for (index, ep) in episodes.iter().enumerate() {
        let info = info_from_season_episode(ep);
        let label = episode_label(&info);
        crate::progress::set_episode(index, total, &label);
        if crate::progress::is_cancelled(&ep.id) {
            ui::warn(format!("Cancelled {label} before it started."));
            crate::progress::set_state(index, EpisodeState::Cancelled);
            summary.record(label, Outcome::Skipped("cancelled by you".into()));
            continue;
        }
        crate::progress::start_episode(&ep.id);
        crate::progress::set_state(index, EpisodeState::Downloading);
        let outcome = download_episode_with_retry(api, backoff, &ep.id, &info, opts).await;
        let cancelled = crate::progress::cancel_requested();
        crate::progress::end_episode();
        let (state, outcome) = match outcome {
            Outcome::Downloaded => (EpisodeState::Done, Outcome::Downloaded),
            Outcome::Skipped(reason) => (EpisodeState::Skipped(reason.clone()), Outcome::Skipped(reason)),
            Outcome::Failed(_) if cancelled => {
                ui::warn(format!("Cancelled {label}."));
                (EpisodeState::Cancelled, Outcome::Skipped("cancelled by you".into()))
            }
            Outcome::Failed(f) => (EpisodeState::Failed(first_line(&f.message).to_string()), Outcome::Failed(f)),
        };
        crate::progress::set_state(index, state);
        summary.record(label, outcome);
    }
    crate::progress::set_episode(total, total, "");
}

pub fn info_from_season_episode(ep: &SeasonEpisode) -> EpisodeInfo {
    EpisodeInfo {
        episode_metadata: EpisodeMetadata {
            audio_locale: ep.audio_locale.clone(),
            episode_number: ep.episode_number,
            season_number: ep.season_number,
            series_title: ep.series_title.clone(),
            availability_starts: ep.availability_starts.clone(),
            versions: ep.versions.clone(),
        },
        title: ep.title.clone(),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{DubVersion, EpisodeMetadata};

    fn sub(language: &str, format: &str, url: &str) -> Option<Subtitle> {
        Some(Subtitle { language: language.into(), format: format.into(), url: url.into() })
    }

    fn episode(subtitles: Vec<(&str, Option<Subtitle>)>, captions: Vec<(&str, Option<Subtitle>)>) -> Episode {
        Episode {
            manifest_url: String::new(),
            subtitles: subtitles.into_iter().map(|(l, s)| (l.to_string(), s)).collect(),
            captions: captions.into_iter().map(|(l, s)| (l.to_string(), s)).collect(),
            token: String::new(),
            error: None,
            reason: None,
        }
    }

    fn info(audio_locale: &str, versions: &[(&str, &str)]) -> EpisodeInfo {
        EpisodeInfo {
            episode_metadata: EpisodeMetadata {
                audio_locale: audio_locale.into(),
                episode_number: 1,
                season_number: 1,
                series_title: "Series".into(),
                availability_starts: String::new(),
                versions: versions
                    .iter()
                    .map(|(l, g)| DubVersion { audio_locale: l.to_string(), guid: g.to_string() })
                    .collect(),
            },
            title: "Episode".into(),
        }
    }

    /// Port of Go TestBuildGuidByLocale.
    #[test]
    fn build_guid_by_locale_cases() {
        let m = guid_by_locale(&info("it-IT", &[("ja-JP", "ja-guid"), ("it-IT", "it-guid")]), "base-id");
        assert_eq!(m.get("ja-JP").map(String::as_str), Some("ja-guid"));
        assert_eq!(m.get("it-IT").map(String::as_str), Some("it-guid"));
        assert_eq!(m.len(), 2);

        // Preferred dub absent from versions is not mapped.
        let m = guid_by_locale(&info("it-IT", &[("ja-JP", "ja-guid")]), "base-id");
        assert_eq!(m.keys().collect::<Vec<_>>(), vec!["ja-JP"]);

        // No versions: falls back to the content ID.
        let m = guid_by_locale(&info("ja-JP", &[]), "base-id");
        assert_eq!(m.get("ja-JP").map(String::as_str), Some("base-id"));
    }

    /// Port of Go TestMergeSubtitleAndCaptions (issue #48: en-US captions exist
    /// only on the second version, so they must be pulled from there).
    #[test]
    fn merge_subtitle_and_captions_issue_48() {
        let episodes = vec![
            episode(
                vec![("en-US", sub("en-US", "ass", "sub-en")), ("fr-FR", sub("fr-FR", "ass", ""))],
                vec![("zh-CN", sub("zh-CN", "vtt", "cc-zh")), ("en-US", sub("en-US", "vtt", ""))],
            ),
            episode(
                vec![("en-US", sub("en-US", "ass", "sub-en")), ("fr-FR", sub("fr-FR", "ass", "sub-fr"))],
                vec![("en-US", sub("en-US", "vtt", "cc-en"))],
            ),
        ];
        let subtitles = merge_tracks(&episodes, |e| &e.subtitles);
        let captions = merge_tracks(&episodes, |e| &e.captions);
        assert!(subtitles.contains_key("en-US"), "en-US subtitle missing");
        assert!(captions.contains_key("en-US"), "en-US caption missing; should come from the second version");
        assert!(captions.contains_key("zh-CN"), "zh-CN caption missing");
        assert_eq!(subtitles["fr-FR"].url, "sub-fr");
        assert_eq!(captions["en-US"].url, "cc-en");
    }

    /// Port of Go TestFilterAvailableLangs.
    #[test]
    fn filter_available_langs_cases() {
        let mut available = BTreeMap::new();
        available.insert("en-US".to_string(), Subtitle::default());
        available.insert("es-419".to_string(), Subtitle::default());
        assert_eq!(
            filter_available(&["en-US".into(), "hi-IN".into()], &available, "Subtitle", 0),
            vec!["en-US".to_string()]
        );
        assert!(filter_available(&[], &available, "Subtitle", 0).is_empty());
    }

    /// Port of Go TestAvailableSubtitleLangs. Go's nil entries are represented by
    /// absence, since `merge_tracks` never keeps them.
    #[test]
    fn available_subtitle_langs_cases() {
        let mut tracks = BTreeMap::new();
        tracks.insert("en-US".to_string(), Subtitle { url: "subtitle-en".into(), ..Default::default() });
        tracks.insert("fr-FR".to_string(), Subtitle { url: "subtitle-fr".into(), ..Default::default() });
        tracks.insert("de-DE".to_string(), Subtitle::default());
        tracks.insert(String::new(), Subtitle { url: "no-locale".into(), ..Default::default() });
        let got = expand_langs(&["ALL".into()], available_locales(&tracks), "en-US");
        assert_eq!(got, vec!["en-US".to_string(), "fr-FR".to_string()]);
    }

    /// Port of Go TestAdditionalVersionGUIDs.
    #[test]
    fn additional_version_guids_cases() {
        let mut available = BTreeMap::new();
        for (l, g) in [("ja-JP", "ja-guid"), ("en-US", "en-guid"), ("fr-FR", "fr-guid"), ("fr-CA", "fr-guid"), ("de-DE", "")] {
            available.insert(l.to_string(), g.to_string());
        }
        assert_eq!(
            additional_version_guids(&available, &["ja-guid".to_string()]),
            vec!["en-guid".to_string(), "fr-guid".to_string()]
        );
    }
}

/// File extension for a subtitle: the API's `format` when set, otherwise the URL's
/// extension (for example `.ass` or `.vtt`), so the file never ends up unnamed.
fn sub_format(sub: &Subtitle) -> String {
    if !sub.format.is_empty() {
        return sub.format.clone();
    }
    let path = sub.url.split(['?', '#']).next().unwrap_or("");
    match path.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() && ext.len() <= 4 && ext.chars().all(|c| c.is_ascii_alphanumeric()) => {
            ext.to_ascii_lowercase()
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod sub_format_tests {
    use super::sub_format;
    use crate::api::Subtitle;

    #[test]
    fn format_falls_back_to_url_extension() {
        let sub = |format: &str, url: &str| Subtitle { format: format.into(), url: url.into(), ..Default::default() };
        assert_eq!(sub_format(&sub("ass", "https://x/a.vtt")), "ass");
        assert_eq!(sub_format(&sub("", "https://x/a.ass?sig=1")), "ass");
        assert_eq!(sub_format(&sub("", "https://x/a.VTT#frag")), "vtt");
        assert_eq!(sub_format(&sub("", "https://x/noext")), "");
        assert_eq!(sub_format(&sub("", "")), "");
    }

    #[test]
    fn missing_fields_deserialize_as_empty() {
        let s: Subtitle = serde_json::from_str(r#"{"language":"en-US"}"#).unwrap();
        assert_eq!(s.format, "");
        assert_eq!(s.url, "");
    }
}

#[cfg(test)]
mod report_tests {
    use super::*;

    #[test]
    fn hints_match_known_failures() {
        let hint = |m: &str| hint_for(&anyhow::anyhow!("{m}"));
        assert!(hint("getLicense for ja-JP: no widevine device provided").unwrap().contains(".wvd"));
        assert!(hint("PSSH not found for ja-JP").unwrap().contains("--audio-lang"));
        assert!(hint("failed to get the video base URL, maybe the video quality you entered is wrong?").unwrap().contains("--video-quality"));
        assert!(hint("failed to get the audio base URL for ja-JP, maybe the audio quality you entered is wrong?").unwrap().contains("--audio-quality"));
        assert!(hint("ffmpeg failed: exit code: 1").unwrap().contains("ffmpeg"));
        assert_eq!(hint("something unexpected"), None);
    }

    #[test]
    fn api_errors_get_their_own_hint() {
        let cf = anyhow::Error::new(ApiError::CloudflareBlocked(403));
        assert!(hint_for(&cf).unwrap().contains("Cloudflare"));
        let auth = anyhow::Error::new(ApiError::AuthRejected("HTTP 401".into()));
        assert!(hint_for(&auth).unwrap().contains("etp_rt"));
    }

    #[test]
    fn exit_code_reflects_failures() {
        let mut s = Summary::default();
        s.record("a".into(), Outcome::Downloaded);
        s.record("b".into(), Outcome::Skipped("already downloaded".into()));
        assert_eq!(s.exit_code(), 0);
        s.record("c".into(), Outcome::Failed(Failure { label: "c".into(), message: "x".into(), hint: None }));
        assert_eq!(s.exit_code(), 1);
        assert_eq!((s.downloaded, s.skipped.len(), s.failed.len()), (1, 1, 1));
    }
}
