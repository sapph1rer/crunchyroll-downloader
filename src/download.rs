//! Track download: segmented (SegmentTemplate) and on-demand (SegmentBase).
//! Port of download.go (downloadParts, streamSegments, downloadSubs) and
//! ondemand.go (range requests, responseRange, streamRange).

use crate::api::USER_AGENT;
use crate::drm::{self, ContentKey};
use crate::mpd::{self, Mpd, OnDemandRep};
use crate::progress::ProgressBar;
use crate::util::track_title;
use anyhow::{anyhow, bail, Context, Result};
use futures::{stream, Future, StreamExt, TryStreamExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncWrite, AsyncWriteExt};

/// Segments fetched at once when the caller does not say otherwise.
pub const MAX_WORKERS: usize = 10;

fn cdn_request(http: &wreq::Client, url: &str) -> wreq::RequestBuilder {
    http.get(url)
        .header("Origin", "https://static.crunchyroll.com")
        .header("Referer", "https://static.crunchyroll.com/")
        .header("User-Agent", USER_AGENT)
}

/// GET with up to 5 attempts and a growing pause between them (2s, 4s, ...),
/// like the Go `downloadPart`.
pub async fn download_part(http: &wreq::Client, url: &str) -> Result<Vec<u8>> {
    const MAX_RETRIES: u32 = 5;
    let mut last = String::new();
    for attempt in 0..MAX_RETRIES {
        if attempt > 0 {
            crate::progress::pause(Duration::from_secs(2 * attempt as u64)).await;
        }
        if crate::progress::cancel_requested() {
            bail!("cancelled by you");
        }
        match cdn_request(http, url).send().await {
            Ok(resp) if resp.status().as_u16() == 200 => match resp.bytes().await {
                Ok(b) => return Ok(b.to_vec()),
                Err(e) => last = format!("reading body: {e}"),
            },
            Ok(resp) => last = format!("status {}", resp.status().as_u16()),
            Err(e) => last = e.to_string(),
        }
    }
    bail!("failed after {MAX_RETRIES} retries: {last}")
}

/// Ranged GET. Accepts 200 or 206, returning the body.
pub async fn range_request(http: &wreq::Client, url: &str, range: Option<&str>) -> Result<Vec<u8>> {
    let mut rb = cdn_request(http, url);
    if let Some(r) = range {
        rb = rb.header("Range", r);
    }
    let resp = rb.send().await?;
    let s = resp.status().as_u16();
    if s != 200 && s != 206 {
        bail!("unexpected status {s}");
    }
    Ok(resp.bytes().await?.to_vec())
}

/// Parses "bytes START-END/TOTAL" into (total, start).
pub fn parse_content_range(cr: &str) -> Option<(i64, i64)> {
    let rest = cr.strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, _end) = range.split_once('-')?;
    Some((total.parse().ok()?, start.parse().ok()?))
}

/// Works out (total size, byte offset of this body) for a response. A Content-Range
/// header wins. Otherwise a known, positive length means the body starts at zero.
/// Returns None when neither is usable.
pub fn response_range(content_range: Option<&str>, content_length: Option<u64>) -> Option<(i64, i64)> {
    if let Some(cr) = content_range {
        if let Some(parsed) = parse_content_range(cr) {
            return Some(parsed);
        }
    }
    match content_length {
        Some(n) if n > 0 => Some((n as i64, 0)),
        _ => None,
    }
}

/// Streams `url` from `start` to EOF into `out`, reporting absolute progress when
/// the total is known. Port of ondemand.go's streamRange.
async fn stream_range<W: AsyncWrite + Unpin>(
    http: &wreq::Client,
    url: &str,
    start: i64,
    out: &mut W,
    bar: &ProgressBar,
) -> Result<()> {
    let resp = cdn_request(http, url)
        .header("Range", format!("bytes={start}-"))
        .send()
        .await?;
    let s = resp.status().as_u16();
    if s != 200 && s != 206 {
        bail!("unexpected status {s}");
    }
    let cr = resp
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let range = response_range(cr.as_deref(), resp.content_length());
    let mut done = 0i64;
    let mut stream = Box::pin(resp.bytes_stream());
    while let Some(chunk) = stream.next().await {
        if crate::progress::cancel_requested() {
            bail!("cancelled by you");
        }
        let chunk = chunk?;
        out.write_all(&chunk).await?;
        done += chunk.len() as i64;
        if let Some((total, base)) = range {
            bar.set_total(total);
            bar.update(base + done, base + done);
        }
    }
    Ok(())
}

/// Fetches every item concurrently, with at most `workers` in flight (at least one),
/// and writes each payload to `w` strictly in index order. `buffered` keeps the output
/// order and caps memory, which replaces the Go slot-channel design. `on_fetched` is
/// called once per fetched segment, with the running count (1..=n).
pub async fn stream_segments<W, F, Fut, P>(
    w: &mut W,
    urls: Vec<String>,
    fetch: F,
    workers: usize,
    on_fetched: P,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<Vec<u8>>>,
    P: Fn(u64),
{
    let fetched = AtomicU64::new(0);
    let mut s = stream::iter(urls.into_iter())
        .map(|u| {
            let f = fetch(u);
            let fetched = &fetched;
            let on_fetched = &on_fetched;
            async move {
                let data = f.await?;
                let n = fetched.fetch_add(1, Ordering::SeqCst) + 1;
                on_fetched(n);
                Ok::<_, anyhow::Error>(data)
            }
        })
        .buffered(workers.max(1));

    let mut index = 0usize;
    while let Some(data) = s.try_next().await? {
        w.write_all(&data)
            .await
            .with_context(|| format!("writing segment {index}"))?;
        index += 1;
    }
    Ok(())
}

pub fn temp_media_path(is_video: bool) -> PathBuf {
    let name = format!(
        "crdl-{}-{}.{}",
        if is_video { "video" } else { "audio" },
        uuid::Uuid::new_v4(),
        if is_video { "mp4" } else { "mp3" }
    );
    std::env::temp_dir().join(name)
}

/// Downloads one track (video or audio) from a parsed manifest and decrypts it.
/// Returns the path to the decrypted temp file.
///
/// `locale` names the audio track (ignored for video). `workers` is how many segments
/// are fetched at once. Port of Go's downloadVideoTrack and downloadAudioTrack.
pub async fn download_track(
    http: &wreq::Client,
    manifest: &Mpd,
    is_video: bool,
    quality: &str,
    locale: &str,
    keys: &[ContentKey],
    workers: usize,
) -> Result<PathBuf> {
    let title = if is_video {
        "Downloading video".to_string()
    } else {
        format!("Downloading {} audio", track_title(locale))
    };
    let out_path = temp_media_path(is_video);
    let enc_path = out_path.with_extension("enc");

    let result = if manifest.is_on_demand() {
        let sets = mpd::parse_on_demand_sets(manifest)?;
        let set = sets
            .iter()
            .find(|s| s.is_video == is_video)
            .ok_or_else(|| anyhow!("no {} adaptation set in on-demand manifest", kind(is_video)))?;
        let rep = mpd::select_on_demand_rep(set, quality)?;
        on_demand_download(http, &rep, &title, &enc_path, &out_path, keys).await
    } else {
        segmented_download(http, manifest, is_video, quality, locale, &title, &enc_path, &out_path, keys, workers).await
    };

    let _ = tokio::fs::remove_file(&enc_path).await;
    match result.context("decrypting track") {
        Ok(()) => Ok(out_path),
        Err(e) => {
            let _ = tokio::fs::remove_file(&out_path).await;
            Err(e)
        }
    }
}

fn kind(is_video: bool) -> &'static str {
    if is_video {
        "video"
    } else {
        "audio"
    }
}

/// On-demand path: init range for key selection, then the rest of the file from
/// the index range to EOF, then decrypt.
async fn on_demand_download(
    http: &wreq::Client,
    rep: &OnDemandRep,
    title: &str,
    enc_path: &std::path::Path,
    out_path: &std::path::Path,
    keys: &[ContentKey],
) -> Result<()> {
    let (i_start, i_end) = mpd::parse_byte_range(&rep.init_range).context("parse init range")?;
    let init = range_request(http, &rep.base_url, Some(&format!("bytes={i_start}-{i_end}")))
        .await
        .context("download init segment")?;
    let (index_start, _) = mpd::parse_byte_range(&rep.index_range).context("parse index range")?;

    let mut enc = tokio::fs::File::create(enc_path).await?;
    enc.write_all(&init).await.context("writing init segment")?;
    let bar = ProgressBar::new(title, 0, "");
    let streamed = stream_range(http, &rep.base_url, index_start, &mut enc, &bar).await;
    bar.finish();
    streamed?;
    enc.flush().await?;
    drop(enc);

    drm::decrypt_track(&init, enc_path, out_path, keys).await
}

/// Segmented path: init segment, then every media segment in order.
#[allow(clippy::too_many_arguments)]
async fn segmented_download(
    http: &wreq::Client,
    manifest: &Mpd,
    is_video: bool,
    quality: &str,
    locale: &str,
    title: &str,
    enc_path: &std::path::Path,
    out_path: &std::path::Path,
    keys: &[ContentKey],
    workers: usize,
) -> Result<()> {
    let period = manifest.first_period()?;
    // Video is the first adaptation set, audio the second (as in Go).
    let idx = if is_video { 0 } else { 1 };
    let set = period.adaptation_sets.get(idx).filter(|s| !s.representations.is_empty());
    let Some(set) = set else {
        if is_video {
            bail!("failed to get the video base URL, maybe the video quality you entered is wrong?");
        }
        bail!("failed to get the audio base URL for {locale}, maybe the audio quality you entered is wrong?");
    };
    let rep = mpd::select_segmented(set, is_video, quality)?;
    let init_url = mpd::build_url(&rep.base, &rep.rep_id, &rep.init_template, None);
    let init = download_part(http, &init_url).await.context("download init segment")?;
    let urls: Vec<String> = rep
        .numbers
        .iter()
        .map(|n| mpd::build_url(&rep.base, &rep.rep_id, &rep.media_template, Some(*n)))
        .collect();

    let mut enc = tokio::fs::File::create(enc_path).await?;
    enc.write_all(&init).await.context("writing init segment")?;

    let total = urls.len() as i64;
    let bar = ProgressBar::new(title, total, "segments");
    let total_bytes = AtomicU64::new(0);
    let fetch = |url: String| {
        let total_bytes = &total_bytes;
        async move {
            let data = download_part(http, &url).await?;
            total_bytes.fetch_add(data.len() as u64, Ordering::SeqCst);
            Ok::<_, anyhow::Error>(data)
        }
    };
    let streamed = stream_segments(&mut enc, urls, fetch, workers, |n| {
        bar.update(n as i64, total_bytes.load(Ordering::SeqCst) as i64);
    })
    .await;
    bar.finish();
    streamed?;
    enc.flush().await?;
    drop(enc);

    drm::decrypt_track(&init, enc_path, out_path, keys).await
}

/// Downloads a subtitle or caption file with one GET (no retries, as in Go) into
/// a temp file named by its format.
pub async fn download_subs(http: &wreq::Client, url: &str, format: &str) -> Result<PathBuf> {
    let resp = cdn_request(http, url).send().await?;
    let body = resp.bytes().await?;
    let ext = if format.is_empty() { "ass" } else { format };
    let path = std::env::temp_dir().join(format!("crdl-subs-{}.{ext}", uuid::Uuid::new_v4()));
    tokio::fs::write(&path, body).await?;
    Ok(path)
}

#[cfg(test)]
pub(crate) mod test_writer {
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use tokio::io::AsyncWrite;

    /// In-memory AsyncWrite. Can fail from the Nth write, and counts writes.
    #[derive(Clone, Default)]
    pub struct Sink {
        pub buf: Arc<Mutex<Vec<u8>>>,
        pub writes: Arc<Mutex<usize>>,
        pub fail_from: usize,
        pub on_write: Option<Arc<dyn Fn() + Send + Sync>>,
    }

    impl AsyncWrite for Sink {
        fn poll_write(self: Pin<&mut Self>, _cx: &mut Context<'_>, data: &[u8]) -> Poll<std::io::Result<usize>> {
            if let Some(cb) = &self.on_write {
                cb();
            }
            let mut writes = self.writes.lock().unwrap();
            *writes += 1;
            if self.fail_from > 0 && *writes >= self.fail_from {
                return Poll::Ready(Err(std::io::Error::other("disk full")));
            }
            self.buf.lock().unwrap().extend_from_slice(data);
            Poll::Ready(Ok(data.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_writer::Sink;
    use super::*;

    /// Segments allowed to be resident at once (fetched but not yet written).
    /// Go's streamSegments allows twice the worker count; the buffered stream
    /// keeps at most MAX_WORKERS, which satisfies the same bound.
    const MAX_BUFFERED_SEGMENTS: usize = MAX_WORKERS * 2;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    fn urls(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("seg-{i}")).collect()
    }

    fn index_of(url: &str) -> usize {
        url.trim_start_matches("seg-").parse().unwrap()
    }

    /// Variable-length payloads so a misordered or truncated write can't match by chance.
    fn segment_body(i: usize) -> Vec<u8> {
        vec![b'a' + (i % 26) as u8; 1 + i % 7]
    }

    fn concatenated(n: usize) -> Vec<u8> {
        (0..n).flat_map(segment_body).collect()
    }

    async fn run(sink: &mut Sink, urls: Vec<String>, fetch: impl Fn(String) -> futures::future::BoxFuture<'static, Result<Vec<u8>>>) -> Result<()> {
        run_with(sink, urls, fetch, MAX_WORKERS).await
    }

    async fn run_with(
        sink: &mut Sink,
        urls: Vec<String>,
        fetch: impl Fn(String) -> futures::future::BoxFuture<'static, Result<Vec<u8>>>,
        workers: usize,
    ) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(30), stream_segments(sink, urls, fetch, workers, |_| {}))
            .await
            .expect("stream_segments deadlocked")
    }

    #[tokio::test]
    async fn one_worker_still_writes_in_order() {
        let total = 40;
        let mut sink = Sink::default();
        let fetch = |url: String| -> futures::future::BoxFuture<'static, Result<Vec<u8>>> {
            Box::pin(async move {
                let i = index_of(&url);
                tokio::time::sleep(Duration::from_millis(((total - i) % 5) as u64)).await;
                Ok(segment_body(i))
            })
        };
        run_with(&mut sink, urls(total), fetch, 1).await.unwrap();
        assert_eq!(*sink.buf.lock().unwrap(), concatenated(total));
    }

    fn ok_fetch(url: String) -> futures::future::BoxFuture<'static, Result<Vec<u8>>> {
        Box::pin(async move { Ok(segment_body(index_of(&url))) })
    }

    #[tokio::test]
    async fn writes_in_index_order_despite_out_of_order_completion() {
        let total = 200;
        let mut sink = Sink::default();
        let fetch = |url: String| -> futures::future::BoxFuture<'static, Result<Vec<u8>>> {
            Box::pin(async move {
                let i = index_of(&url);
                tokio::time::sleep(Duration::from_millis(((total - i) % 13) as u64)).await;
                Ok(segment_body(i))
            })
        };
        run(&mut sink, urls(total), fetch).await.unwrap();
        assert_eq!(*sink.buf.lock().unwrap(), concatenated(total));
    }

    #[tokio::test]
    async fn survives_slow_lead_segment() {
        let total = MAX_BUFFERED_SEGMENTS * 4;
        let mut sink = Sink::default();
        let fetch = |url: String| -> futures::future::BoxFuture<'static, Result<Vec<u8>>> {
            Box::pin(async move {
                let i = index_of(&url);
                if i == 0 {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                Ok(segment_body(i))
            })
        };
        run(&mut sink, urls(total), fetch).await.unwrap();
        assert_eq!(*sink.buf.lock().unwrap(), concatenated(total));
    }

    #[tokio::test]
    async fn bounds_resident_segments() {
        let total = MAX_BUFFERED_SEGMENTS * 20;
        let resident = Arc::new(Mutex::new((0usize, 0usize))); // (current, peak)
        let bump = {
            let resident = resident.clone();
            move || {
                let mut r = resident.lock().unwrap();
                r.0 += 1;
                r.1 = r.1.max(r.0);
            }
        };
        let release = {
            let resident = resident.clone();
            move || resident.lock().unwrap().0 -= 1
        };
        let mut sink = Sink { on_write: Some(Arc::new(release)), ..Default::default() };
        let fetch = {
            let bump = Arc::new(bump);
            move |url: String| -> futures::future::BoxFuture<'static, Result<Vec<u8>>> {
                let bump = bump.clone();
                Box::pin(async move {
                    let i = index_of(&url);
                    if i == 0 {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                    bump();
                    Ok(segment_body(i))
                })
            }
        };
        run(&mut sink, urls(total), fetch).await.unwrap();
        let peak = resident.lock().unwrap().1;
        assert!(peak <= MAX_BUFFERED_SEGMENTS, "held {peak} at once, want at most {MAX_BUFFERED_SEGMENTS}");
        assert_eq!(*sink.buf.lock().unwrap(), concatenated(total));
    }

    #[tokio::test]
    async fn propagates_fetch_error() {
        let mut sink = Sink::default();
        let fetch = |url: String| -> futures::future::BoxFuture<'static, Result<Vec<u8>>> {
            Box::pin(async move {
                if index_of(&url) == 7 {
                    return Err(anyhow!("segment 7 is gone"));
                }
                Ok(segment_body(index_of(&url)))
            })
        };
        let err = run(&mut sink, urls(MAX_BUFFERED_SEGMENTS * 4), fetch).await.unwrap_err();
        assert!(err.to_string().contains("segment 7 is gone"), "{err}");
    }

    #[tokio::test]
    async fn propagates_write_error_and_stops() {
        let mut sink = Sink { fail_from: 3, ..Default::default() };
        let err = run(&mut sink, urls(MAX_BUFFERED_SEGMENTS * 4), ok_fetch).await.unwrap_err();
        assert!(format!("{err:#}").contains("disk full"), "{err:#}");
    }

    #[tokio::test]
    async fn no_urls_is_ok_and_writes_nothing() {
        let mut sink = Sink::default();
        let fetch = |_: String| -> futures::future::BoxFuture<'static, Result<Vec<u8>>> {
            panic!("fetch called with no urls")
        };
        run(&mut sink, vec![], fetch).await.unwrap();
        assert!(sink.buf.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reports_progress_once_per_segment() {
        let total = 50;
        let mut sink = Sink::default();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        stream_segments(&mut sink, urls(total), ok_fetch, MAX_WORKERS, move |n| seen2.lock().unwrap().push(n))
            .await
            .unwrap();
        let mut seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), total);
        seen.sort_unstable();
        assert_eq!(seen, (1..=total as u64).collect::<Vec<_>>());
    }

    #[test]
    fn content_range_parses() {
        assert_eq!(parse_content_range("bytes 123456-999999/1000000"), Some((1000000, 123456)));
        assert_eq!(parse_content_range("bytes 0-999/1000"), Some((1000, 0)));
        assert_eq!(parse_content_range("bytes foo-bar/baz"), None);
        assert_eq!(parse_content_range("0-999/1000"), None);
    }

    #[test]
    fn response_range_cases() {
        assert_eq!(response_range(Some("bytes 123456-999999/1000000"), None), Some((1000000, 123456)));
        assert_eq!(response_range(Some("bytes 0-999/1000"), None), Some((1000, 0)));
        assert_eq!(response_range(None, Some(5000)), Some((5000, 0)));
        assert_eq!(response_range(None, None), None);
        assert_eq!(response_range(None, Some(0)), None);
        assert_eq!(response_range(Some("bytes foo-bar/baz"), None), None);
        assert_eq!(response_range(Some("0-999/1000"), None), None);
    }

    /// Keeps `Instant` in scope for future timing tests without a warning.
    #[allow(dead_code)]
    fn _now() -> Instant {
        Instant::now()
    }
}
