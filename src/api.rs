//! Crunchyroll HTTP API: auth, metadata, seasons, playback.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;

pub const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:147.0) Gecko/20100101 Firefox/147.0";
/// Production token endpoint. Tests point `Api` at a local server instead.
pub const TOKEN_URL: &str = "https://www.crunchyroll.com/auth/v1/token";
const BASIC_AUTH: &str = "Basic bm9haWhkZXZtXzZpeWcwYThsMHE6";

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// Token request refused: the etp_rt cookie is almost certainly wrong/expired.
    #[error("crunchyroll rejected the etp_rt cookie ({0})")]
    AuthRejected(String),
    /// Cloudflare's bot check answered instead of Crunchyroll. The cookie was never
    /// evaluated, so this must not be reported as a bad cookie.
    #[error(
        "blocked by Cloudflare's bot check (HTTP {0}), not by Crunchyroll. The cookie was not evaluated. \
         Try again later, from a different network, or with a current browser build of this tool."
    )]
    CloudflareBlocked(u16),
    /// Playback endpoint asked us to back off.
    #[error("rate limited by Crunchyroll")]
    RateLimited,
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

/// Builds a wreq::Client with optional Firefox emulation and optional proxy.
/// `with_emulation` should be true for API calls (needs Cloudflare bypass),
/// false for CDN segment downloads (no emulation needed → faster).
pub fn build_client(proxy_url: Option<&str>, with_emulation: bool) -> Result<wreq::Client, wreq::Error> {
    let mut b = wreq::Client::builder().user_agent(USER_AGENT);
    if with_emulation {
        b = b.emulation(wreq_util::Emulation::Firefox147);
    }
    if let Some(url) = proxy_url.filter(|s| !s.is_empty()) {
        if let Ok(proxy) = wreq::Proxy::all(url) {
            b = b.proxy(proxy);
        }
    }
    b.build()
}

/// Shared client: holds the bearer token and can refresh it on 401.
/// Also exposes a separate CDN client that bypasses the proxy for speed.
#[derive(Clone)]
pub struct Api {
    http: wreq::Client,
    /// Direct client for CDN segment downloads — no proxy, no emulation.
    cdn_http: wreq::Client,
    etp_rt: Arc<String>,
    device_id: Arc<String>,
    token: Arc<RwLock<String>>,
    token_url: Arc<String>,
}

impl Api {
    pub async fn login(etp_rt: String) -> Result<Self, ApiError> {
        Self::login_at(TOKEN_URL, etp_rt, None).await
    }

    pub async fn login_with_proxy(etp_rt: String, proxy_url: &str) -> Result<Self, ApiError> {
        Self::login_at(TOKEN_URL, etp_rt, Some(proxy_url)).await
    }

    /// Like `login`, but fetches the token from `token_url`.
    pub async fn login_at(token_url: &str, etp_rt: String, proxy_url: Option<&str>) -> Result<Self, ApiError> {
        let device_id = uuid::Uuid::new_v4().to_string();
        // Firefox emulation gives the same TLS and HTTP/2 fingerprint as a real Firefox
        // (the same one the User-Agent claims). Cloudflare checks this, and rustls and
        // native-tls both fail its bot check.
        // The API client uses the proxy; the CDN client goes direct for maximum throughput.
        let http = build_client(proxy_url, true)
            .map_err(|e| ApiError::Other(e.into()))?;
        let cdn_http = build_client(None, false)
            .map_err(|e| ApiError::Other(e.into()))?;
        let token = fetch_token(&http, token_url, &device_id, &etp_rt).await?;
        Ok(Self {
            http,
            cdn_http,
            etp_rt: Arc::new(etp_rt),
            device_id: Arc::new(device_id),
            token: Arc::new(RwLock::new(token)),
            token_url: Arc::new(token_url.to_string()),
        })
    }

    /// The API client (goes through proxy when configured).
    pub fn http(&self) -> &wreq::Client {
        &self.http
    }

    /// The CDN client (always direct, bypasses proxy for fast segment downloads).
    pub fn cdn_http(&self) -> &wreq::Client {
        &self.cdn_http
    }

    pub async fn bearer(&self) -> String {
        format!("Bearer {}", self.token.read().await)
    }

    /// Sends a GET/DELETE with the bearer token. On 401, refreshes the token once
    /// and retries, mirroring the Go `DoRequest` behaviour.
    pub async fn send_authed(
        &self,
        method: wreq::Method,
        url: &str,
        extra: impl Fn(wreq::RequestBuilder) -> wreq::RequestBuilder,
    ) -> Result<wreq::Response, ApiError> {
        let build = |tok: &str| {
            let rb = self
                .http
                .request(method.clone(), url)
                .header("Authorization", format!("Bearer {tok}"));
            extra(rb)
        };
        let tok = self.token.read().await.clone();
        let resp = build(&tok).send().await.map_err(|e| ApiError::Other(e.into()))?;
        if resp.status() != wreq::StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        crate::say!("Access token expired. Refetching one...");
        let new = fetch_token(&self.http, &self.token_url, &self.device_id, &self.etp_rt).await?;
        *self.token.write().await = new.clone();
        let resp = build(&new).send().await.map_err(|e| ApiError::Other(e.into()))?;
        if resp.status() == wreq::StatusCode::UNAUTHORIZED {
            return Err(ApiError::Other(anyhow!(
                "{method} {url}: still unauthorized after refreshing the access token"
            )));
        }
        Ok(resp)
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        let resp = self
            .send_authed(wreq::Method::GET, url, |rb| rb)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        let text = resp.text().await?;
        serde_json::from_str(&text).with_context(|| format!("decoding {url}"))
    }

    /// One page of the catalog, ordered by `sort`. Returns the cards and the total available.
    pub async fn browse(
        &self,
        sort: crate::browse::Sort,
        show: crate::browse::Show,
        genre: Option<&str>,
        start: usize,
        n: usize,
        locale: &str,
    ) -> Result<(Vec<crate::browse::CatalogItem>, usize)> {
        let url = crate::browse::browse_url(sort, show, genre, start, n, locale);
        let v: serde_json::Value = self.get_json(&url).await?;
        Ok(crate::browse::parse_browse(&v))
    }

    /// The genres the catalog can be filtered by.
    pub async fn genres(&self, locale: &str) -> Result<Vec<crate::browse::Genre>> {
        let v: serde_json::Value = self.get_json(&crate::browse::categories_url(locale)).await?;
        Ok(crate::browse::parse_categories(&v))
    }

    /// Series matching a search, best matches first.
    pub async fn search(&self, query: &str, n: usize, locale: &str) -> Result<Vec<crate::browse::CatalogItem>> {
        let v: serde_json::Value = self.get_json(&crate::browse::search_url(query, n, locale)).await?;
        Ok(crate::browse::parse_search(&v))
    }

    pub async fn seasons(&self, series_id: &str, audio: &str, subs: &str) -> Result<Vec<Season>> {
        let url = format!(
            "https://www.crunchyroll.com/content/v2/cms/series/{series_id}/seasons?force_locale=&preferred_audio_language={audio}&locale={subs}"
        );
        let v: DataWrap<Season> = self.get_json(&url).await?;
        Ok(v.data)
    }

    pub async fn season_episodes(
        &self,
        season_id: &str,
        audio: &str,
        subs: &str,
    ) -> Result<Vec<SeasonEpisode>> {
        let url = format!(
            "https://www.crunchyroll.com/content/v2/cms/seasons/{season_id}/episodes?preferred_audio_language={audio}&locale={subs}"
        );
        let v: DataWrap<SeasonEpisode> = self.get_json(&url).await?;
        Ok(v.data)
    }

    /// Metadata of one playable item from a /watch/ link: a series episode, or a film (whose
    /// object carries `movie_metadata` instead of `episode_metadata`).
    pub async fn episode_info(&self, id: &str) -> Result<EpisodeInfo> {
        let url = format!(
            "https://www.crunchyroll.com/content/v2/cms/objects/{id}?ratings=true&preferred_audio_language=ja-JP&locale=en-US"
        );
        let v: serde_json::Value = self.get_json(&url).await?;
        let obj = v
            .get("data")
            .and_then(|d| d.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| anyhow!("no metadata returned for {id}"))?;
        if obj.get("episode_metadata").is_some() {
            return serde_json::from_value(obj.clone()).with_context(|| format!("decoding episode {id}"));
        }
        let ep = movie_episode(obj, 1, "").ok_or_else(|| anyhow!("{id} is neither an episode nor a movie"))?;
        Ok(crate::episode::info_from_season_episode(&ep))
    }

    /// The films of a movie listing, as "episodes" of it (so they download exactly like
    /// episodes), with the listing's title.
    pub async fn movie_listing(&self, listing_id: &str, audio: &str, subs: &str) -> Result<(String, Vec<SeasonEpisode>)> {
        let base = format!("https://www.crunchyroll.com/content/v2/cms/movie_listings/{listing_id}");
        let query = format!("preferred_audio_language={audio}&locale={subs}");
        let head: serde_json::Value = self.get_json(&format!("{base}?{query}")).await?;
        let listing_title = head
            .get("data")
            .and_then(|d| d.as_array())
            .and_then(|a| a.first())
            .and_then(|o| o.get("title"))
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        let movies: serde_json::Value = self.get_json(&format!("{base}/movies?{query}")).await?;
        let films: Vec<SeasonEpisode> = movies
            .get("data")
            .and_then(|d| d.as_array())
            .map(|a| {
                a.iter()
                    .enumerate()
                    .filter_map(|(i, m)| movie_episode(m, i as i64 + 1, &listing_title))
                    .collect()
            })
            .unwrap_or_default();
        let title = if listing_title.is_empty() {
            films.first().map(|f| f.series_title.clone()).unwrap_or_default()
        } else {
            listing_title
        };
        Ok((title, films))
    }

    /// Fetches playback info for one dub GUID.
    pub async fn playback(&self, guid: &str, debug: bool) -> Result<Episode, ApiError> {
        let url = format!("https://www.crunchyroll.com/playback/v3/{guid}/web/firefox/play");
        let resp = self
            .send_authed(wreq::Method::GET, &url, |rb| rb)
            .await?;
        let body = resp.text().await.map_err(|e| ApiError::Other(e.into()))?;
        let episode: Episode = serde_json::from_str(&body).map_err(|e| ApiError::Other(e.into()))?;
        if let Some(err) = &episode.error {
            crate::say!("Error: {err}");
            if let Some(reason) = &episode.reason {
                crate::say!(" ({reason})");
            }
            if err.starts_with("429") {
                crate::say!("Crunchyroll is rate-limiting this account. Wait a while before retrying, or use a different account.");
                return Err(ApiError::RateLimited);
            }
            return Err(ApiError::Other(anyhow!("playback error: {err}")));
        }
        if debug {
            crate::say!("\n{body}");
        }
        Ok(episode)
    }

    /// Releases a playback stream so Crunchyroll sees the player "leave".
    pub async fn delete_stream(&self, content_id: &str, s_token: &str) -> Result<()> {
        let url = format!("https://www.crunchyroll.com/playback/v1/token/{content_id}/{s_token}");
        let resp = self
            .send_authed(wreq::Method::DELETE, &url, |rb| rb)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        if resp.status() != wreq::StatusCode::NO_CONTENT {
            bail!("unexpected status {}", resp.status());
        }
        Ok(())
    }

    /// Posts a Widevine challenge and returns the decoded license bytes.
    pub async fn widevine_license(
        &self,
        content_id: &str,
        video_token: &str,
        challenge: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let resp = self
            .send_authed(
                wreq::Method::POST,
                "https://www.crunchyroll.com/license/v1/license/widevine",
                |rb| {
                    rb.header("Content-Type", "application/octet-stream")
                        .header("X-Cr-Content-Id", content_id)
                        .header("X-Cr-Video-Token", video_token)
                        .header("Origin", "https://static.crunchyroll.com")
                        .header("Referer", "https://static.crunchyroll.com/")
                        .body(challenge.clone())
                },
            )
            .await
            .map_err(|e| anyhow!("{e}"))?;
        #[derive(Deserialize)]
        struct LicenseResp {
            license: String,
        }
        let text = resp.text().await?;
        let parsed: LicenseResp = serde_json::from_str(&text).context("license response")?;
        use base64::Engine;
        Ok(base64::engine::general_purpose::STANDARD.decode(parsed.license)?)
    }
}

/// True when the response is Cloudflare's managed challenge page rather than an
/// answer from Crunchyroll: `cf-mitigated: challenge` header, or the usual
/// "Just a moment..." interstitial markup on a 403/503.
fn is_cloudflare_challenge(status: u16, cf_mitigated: bool, body: &str) -> bool {
    if !matches!(status, 403 | 503) {
        return false;
    }
    cf_mitigated || body.contains("Just a moment") || body.contains("cf-chl-")
}

async fn fetch_token(http: &wreq::Client, token_url: &str, device_id: &str, etp_rt: &str) -> Result<String, ApiError> {
    let resp = http
        .post(token_url)
        .header("Authorization", BASIC_AUTH)
        .form(&[
            ("device_id", device_id),
            ("device_type", "Firefox on Linux"),
            ("grant_type", "etp_rt_cookie"),
        ])
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Cookie", format!("device_id={device_id}; etp_rt={etp_rt}"))
        .send()
        .await
        .map_err(|e| ApiError::Other(anyhow!("failed to get access token: {e}")))?;
    let status = resp.status();
    let cf_mitigated = resp.headers().contains_key("cf-mitigated");
    let text = resp.text().await.unwrap_or_default();
    if is_cloudflare_challenge(status.as_u16(), cf_mitigated, &text) {
        return Err(ApiError::CloudflareBlocked(status.as_u16()));
    }
    match status.as_u16() {
        200 => {}
        400 | 401 | 403 => {
            return Err(ApiError::AuthRejected(format!(
                "HTTP {}: {}",
                status.as_u16(),
                text.trim()
            )))
        }
        s => {
            return Err(ApiError::Other(anyhow!(
                "failed to get access token: HTTP {s}: {}",
                text.trim()
            )))
        }
    }
    #[derive(Deserialize)]
    struct TokenResp {
        access_token: String,
    }
    let parsed: TokenResp = serde_json::from_str(&text)
        .map_err(|e| ApiError::Other(anyhow!("failed to get access token: {e}")))?;
    if parsed.access_token.is_empty() {
        return Err(ApiError::Other(anyhow!(
            "failed to get access token: the response had no access_token"
        )));
    }
    Ok(parsed.access_token)
}

// ---------- models ----------

#[derive(Debug, Deserialize)]
pub struct DataWrap<T> {
    pub data: Vec<T>,
}

#[derive(Debug, Deserialize)]
pub struct Season {
    pub id: String,
    pub season_number: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SeasonEpisode {
    pub id: String,
    #[serde(default)]
    pub versions: Vec<DubVersion>,
    pub season_number: i64,
    pub episode_number: i64,
    pub series_title: String,
    #[serde(default)]
    pub audio_locale: String,
    pub title: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub availability_starts: String,
    /// A film from a movie listing rather than a series episode (set by this program).
    #[serde(default)]
    pub is_movie: bool,
}

/// Reads a film object (from `/objects/{id}` or a listing's `/movies`) as episode `index`
/// of its listing. Metadata may sit in `movie_metadata` or on the object itself. When the
/// object names no dub versions and no audio locale, Japanese is assumed, so the film's own
/// id is used for playback.
pub fn movie_episode(v: &serde_json::Value, index: i64, listing_title: &str) -> Option<SeasonEpisode> {
    use serde_json::Value;
    let text = |o: &Value, k: &str| o.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let kind = text(v, "type");
    if !kind.is_empty() && kind != "movie" {
        return None;
    }
    let id = text(v, "id");
    let title = text(v, "title");
    if id.is_empty() || title.is_empty() {
        return None;
    }
    let meta = v.get("movie_metadata").unwrap_or(v);
    let versions: Vec<DubVersion> = meta
        .get("versions")
        .or_else(|| v.get("versions"))
        .and_then(|x| serde_json::from_value(x.clone()).ok())
        .unwrap_or_default();
    let mut audio_locale = text(meta, "audio_locale");
    if audio_locale.is_empty() {
        audio_locale = text(v, "audio_locale");
    }
    if audio_locale.is_empty() && versions.is_empty() {
        audio_locale = "ja-JP".into();
    }
    let mut series_title = text(meta, "movie_listing_title");
    if series_title.is_empty() {
        series_title = text(v, "movie_listing_title");
    }
    if series_title.is_empty() {
        series_title = if listing_title.is_empty() { title.clone() } else { listing_title.to_string() };
    }
    Some(SeasonEpisode {
        id,
        versions,
        season_number: 0,
        episode_number: index,
        series_title,
        audio_locale,
        title,
        availability_starts: String::new(),
        is_movie: true,
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct EpisodeInfo {
    pub episode_metadata: EpisodeMetadata,
    pub title: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EpisodeMetadata {
    #[serde(default)]
    pub audio_locale: String,
    pub episode_number: i64,
    pub season_number: i64,
    pub series_title: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub availability_starts: String,
    #[serde(default)]
    pub versions: Vec<DubVersion>,
    /// A film rather than a series episode (set by this program).
    #[serde(default)]
    pub is_movie: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DubVersion {
    pub audio_locale: String,
    pub guid: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Subtitle {
    /// Kept to mirror the Go model; not used by the download path.
    #[serde(default)]
    #[allow(dead_code)]
    pub language: String,
    /// Go tolerates a missing `format`/`url` (it leaves them empty), so do the same.
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub url: String,
}

/// Playback response. `error` is polymorphic in the API: a string on failure,
/// but `false`, `null`, `0` or absent on success.
#[derive(Debug, Deserialize)]
pub struct Episode {
    #[serde(rename = "url", default)]
    pub manifest_url: String,
    #[serde(default)]
    pub subtitles: std::collections::HashMap<String, Option<Subtitle>>,
    #[serde(default)]
    pub captions: std::collections::HashMap<String, Option<Subtitle>>,
    #[serde(default)]
    pub token: String,
    #[serde(default, deserialize_with = "de_episode_error")]
    pub error: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

fn de_episode_error<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(false) => None,
        serde_json::Value::Number(n) if n.as_f64() == Some(0.0) => None,
        serde_json::Value::String(s) if s.is_empty() => None,
        serde_json::Value::String(s) => Some(s),
        other => Some(other.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_field_shapes() {
        for (json, expect) in [
            (r#"{"error": false}"#, None),
            (r#"{"error": null}"#, None),
            (r#"{"error": 0}"#, None),
            (r#"{"error": ""}"#, None),
            (r#"{"error": "429 Too many"}"#, Some("429 Too many")),
        ] {
            let ep: Episode = serde_json::from_str(json).unwrap();
            assert_eq!(ep.error.as_deref(), expect, "input {json}");
        }
    }
}


#[cfg(test)]
mod cloudflare_tests {
    use super::is_cloudflare_challenge;

    #[test]
    fn detects_challenge_pages_only() {
        let page = "<!DOCTYPE html><title>Just a moment...</title>";
        assert!(is_cloudflare_challenge(403, false, page));
        assert!(is_cloudflare_challenge(403, true, "{}"));
        assert!(is_cloudflare_challenge(503, false, page));
        // A real Crunchyroll rejection is not a challenge.
        assert!(!is_cloudflare_challenge(403, false, r#"{"error":"invalid_grant"}"#));
        assert!(!is_cloudflare_challenge(401, false, page));
        assert!(!is_cloudflare_challenge(200, true, page));
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A request as seen by the test server.
    struct Req {
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    fn read_request(stream: &mut TcpStream) -> Option<Req> {
        let mut reader = BufReader::new(stream.try_clone().ok()?);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?; // request line
        let mut headers = HashMap::new();
        let mut content_length = 0usize;
        loop {
            let mut h = String::new();
            reader.read_line(&mut h).ok()?;
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                let k = k.trim().to_ascii_lowercase();
                let v = v.trim().to_string();
                if k == "content-length" {
                    content_length = v.parse().unwrap_or(0);
                }
                headers.insert(k, v);
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).ok()?;
        Some(Req { headers, body })
    }

    /// Starts a one-thread HTTP/1.1 server on a random local port and returns its
    /// base URL. `handler` returns (status, body) for each request. Each connection
    /// is answered once with `Connection: close`, which is all these tests need.
    fn serve<F>(handler: F) -> String
    where
        F: Fn(&Req) -> (u16, String) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some(req) = read_request(&mut stream) else { continue };
                let (status, body) = handler(&req);
                let resp = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}/")
    }

    fn client_with_token(token_url: &str, token: &str) -> Api {
        Api {
            http: wreq::Client::new(),
            cdn_http: wreq::Client::new(),
            etp_rt: Arc::new("cookie".into()),
            device_id: Arc::new("device".into()),
            token: Arc::new(RwLock::new(token.into())),
            token_url: Arc::new(token_url.into()),
        }
    }

    /// Port of Go TestGetAccessToken.
    #[tokio::test]
    async fn get_access_token_cases() {
        // (name, status, body, Ok(token) or Err(is_auth_rejected))
        let cases: Vec<(&str, u16, &str, Result<&str, bool>)> = vec![
            ("valid cookie", 200, r#"{"access_token":"abc"}"#, Ok("abc")),
            (
                "invalid cookie",
                400,
                r#"{"code":"auth.obtain_access_token.oauth2_error","error":"invalid_grant"}"#,
                Err(true),
            ),
            ("unauthorized", 401, "{}", Err(true)),
            ("server error", 500, "oops", Err(false)),
            ("empty token", 200, r#"{"access_token":""}"#, Err(false)),
            ("malformed JSON", 200, "not json", Err(false)),
        ];
        let http = wreq::Client::new();
        for (name, status, body, want) in cases {
            let url = serve(move |_| (status, body.to_string()));
            let got = fetch_token(&http, &url, "device", "cookie").await;
            match (got, want) {
                (Ok(tok), Ok(w)) => assert_eq!(tok, w, "{name}"),
                (Err(e), Err(rejected)) => {
                    let is_rejected = matches!(e, ApiError::AuthRejected(_));
                    assert_eq!(is_rejected, rejected, "{name}: {e}");
                }
                (got, want) => panic!("{name}: got {got:?}, want {want:?}"),
            }
        }
    }

    /// Port of Go TestDoRequestRefreshesTokenOnce: the retry resends the body.
    #[tokio::test]
    async fn do_request_refreshes_token_once() {
        let token_url = serve(|_| (200, r#"{"access_token":"fresh"}"#.into()));
        let calls = Arc::new(AtomicUsize::new(0));
        let api_calls = calls.clone();
        let api_url = serve(move |r| {
            api_calls.fetch_add(1, Ordering::SeqCst);
            if r.headers.get("authorization").map(String::as_str) != Some("Bearer fresh") {
                return (401, String::new());
            }
            (200, String::from_utf8_lossy(&r.body).into_owned())
        });

        let api = client_with_token(&token_url, "stale");
        let resp = api
            .send_authed(wreq::Method::POST, &api_url, |rb| rb.body("payload"))
            .await
            .expect("request should succeed after refresh");
        assert_eq!(resp.text().await.unwrap(), "payload", "retried body");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "API call count");
        assert_eq!(*api.token.read().await, "fresh");
    }

    /// Port of Go TestDoRequestStopsWhenCookieRejected.
    #[tokio::test]
    async fn do_request_stops_when_cookie_rejected() {
        let token_calls = Arc::new(AtomicUsize::new(0));
        let tc = token_calls.clone();
        let token_url = serve(move |_| {
            tc.fetch_add(1, Ordering::SeqCst);
            (400, r#"{"error":"invalid_grant"}"#.into())
        });
        let api_url = serve(|_| (401, String::new()));
        let api = client_with_token(&token_url, "stale");

        let err = api
            .send_authed(wreq::Method::GET, &api_url, |rb| rb)
            .await
            .err()
            .expect("expected an error");
        assert!(matches!(err, ApiError::AuthRejected(_)), "{err}");
        assert_eq!(token_calls.load(Ordering::SeqCst), 1, "token endpoint calls");
    }

    /// Port of Go TestDoRequestStopsWhenStillUnauthorized.
    #[tokio::test]
    async fn do_request_stops_when_still_unauthorized() {
        let token_calls = Arc::new(AtomicUsize::new(0));
        let tc = token_calls.clone();
        let token_url = serve(move |_| {
            tc.fetch_add(1, Ordering::SeqCst);
            (200, r#"{"access_token":"fresh"}"#.into())
        });
        let api_calls = Arc::new(AtomicUsize::new(0));
        let ac = api_calls.clone();
        let api_url = serve(move |_| {
            ac.fetch_add(1, Ordering::SeqCst);
            (401, String::new())
        });
        let api = client_with_token(&token_url, "stale");

        assert!(api.send_authed(wreq::Method::GET, &api_url, |rb| rb).await.is_err(), "expected an error after a second 401");
        assert_eq!(token_calls.load(Ordering::SeqCst), 1, "token endpoint calls");
        assert_eq!(api_calls.load(Ordering::SeqCst), 2, "API call count");
    }

    /// Port of Go TestEpisodeUnmarshalErrorField.
    #[test]
    fn episode_error_field_table() {
        let cases = [
            (r#"{"error":"region locked"}"#, "region locked", ""),
            (r#"{"error":false}"#, "", ""),
            (r#"{"error":null}"#, "", ""),
            (r#"{"error":0}"#, "", ""),
            (r#"{"error":403}"#, "403", ""),
            (r#"{"error":true}"#, "true", ""),
            (r#"{}"#, "", ""),
            (r#"{"error":4294,"reason":"Too many requests"}"#, "4294", "Too many requests"),
        ];
        for (json, want_err, want_reason) in cases {
            let ep: Episode = serde_json::from_str(json).unwrap();
            assert_eq!(ep.error.as_deref().unwrap_or(""), want_err, "error for {json}");
            assert_eq!(ep.reason.as_deref().unwrap_or(""), want_reason, "reason for {json}");
        }
    }
}

#[cfg(test)]
mod movie_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_a_film_with_versions() {
        let v = json!({
            "id": "G25FVD45Q", "type": "movie", "title": "The Movie",
            "movie_metadata": {
                "movie_listing_title": "Some Listing",
                "versions": [
                    { "audio_locale": "ja-JP", "guid": "G25FVD45Q", "original": true },
                    { "audio_locale": "en-US", "guid": "GEN123" }
                ]
            }
        });
        let ep = movie_episode(&v, 1, "").unwrap();
        assert!(ep.is_movie);
        assert_eq!(ep.series_title, "Some Listing");
        assert_eq!(ep.versions.len(), 2);
        assert_eq!((ep.season_number, ep.episode_number), (0, 1));
    }

    #[test]
    fn a_bare_film_plays_its_own_id_in_japanese() {
        let v = json!({ "id": "M1", "title": "Bare Film" });
        let ep = movie_episode(&v, 2, "Listing").unwrap();
        assert_eq!(ep.audio_locale, "ja-JP");
        assert_eq!(ep.series_title, "Listing");
        let info = crate::episode::info_from_season_episode(&ep);
        let guids = crate::episode::guid_by_locale(&info, "M1");
        assert_eq!(guids.get("ja-JP").map(String::as_str), Some("M1"));
    }

    #[test]
    fn episodes_are_not_films() {
        assert!(movie_episode(&json!({ "id": "E", "type": "episode", "title": "Ep" }), 1, "").is_none());
    }
}
