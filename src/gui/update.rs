//! App updates from GitHub Releases (same scheme as waku-app).
//!
//! The newest release of [`REPO`] carries [`ASSET`] (the three .exe files) and `ASSET.sha256`.
//! The zip is downloaded, checked against the checksum, unpacked, and copied over the
//! program's folder. Windows lets a running .exe be renamed but not overwritten, so each old
//! file becomes `<name>.old-<n>` first (removed at the next start); then the new GUI starts
//! and this one closes. `scripts/make-release.ps1` builds the zip + checksum and publishes.

use std::io::Read;
use std::path::{Path, PathBuf};

pub const REPO: &str = "sapph1rer/crunchyroll-downloader";
pub const ASSET: &str = "crunchyroll-downloader-windows-x64.zip";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub version: (u32, u32, u32),
    pub notes: String,
    pub page: String,
    pub zip_url: Option<String>,
    pub sha_url: Option<String>,
}

impl Release {
    pub fn is_newer(&self) -> bool {
        parse_version(VERSION).is_some_and(|me| self.version > me)
    }
}

/// Progress of an update install.
#[derive(Clone, Debug)]
pub enum UpdateEvent {
    /// Download fraction 0.0..=1.0 (negative when the size is unknown).
    Progress(f32),
    /// Installed: the new GUI program to start.
    Done(Result<PathBuf, String>),
}

pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let core = s.trim().trim_start_matches(['v', 'V']).split(['-', '+']).next()?;
    let mut it = core.split('.').map(|x| x.parse::<u32>().ok());
    let a = it.next()??;
    Some((a, it.next().flatten().unwrap_or(0), it.next().flatten().unwrap_or(0)))
}

/// A build run from cargo's target folder: replaced with git/cargo, not by the updater.
pub fn dev_build() -> bool {
    std::env::current_exe().map(|p| p.components().any(|c| c.as_os_str() == "target")).unwrap_or(false)
}

pub fn parse_release(v: &serde_json::Value) -> Result<Release, String> {
    let tag = v["tag_name"].as_str().ok_or("no releases yet")?.to_string();
    let version = parse_version(&tag).ok_or_else(|| format!("release tag {tag} is not a version"))?;
    let url_of = |name: &str| {
        v["assets"].as_array().into_iter().flatten().find(|a| a["name"].as_str() == Some(name)).and_then(|a| a["browser_download_url"].as_str().map(String::from))
    };
    Ok(Release {
        tag,
        version,
        notes: v["body"].as_str().unwrap_or("").trim().to_string(),
        page: v["html_url"].as_str().unwrap_or("").to_string(),
        zip_url: url_of(ASSET),
        sha_url: url_of(&format!("{ASSET}.sha256")),
    })
}

fn client() -> wreq::Client {
    wreq::Client::new()
}

fn get(c: &wreq::Client, url: &str) -> wreq::RequestBuilder {
    // GitHub refuses requests without a User-Agent; release files redirect to a CDN.
    c.get(url)
        .header("User-Agent", concat!("crunchyroll-downloader/", env!("CARGO_PKG_VERSION")))
        .redirect(wreq::redirect::Policy::limited(10))
}

/// The newest release.
// shortcut: GitHub API only (60 requests/hour per IP); add waku's github.com page fallback if that bites.
pub async fn latest() -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = get(&client(), &url).header("Accept", "application/vnd.github+json").send().await.map_err(|e| format!("GitHub: {e}"))?;
    let body = resp.text().await.map_err(|e| format!("GitHub: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| format!("GitHub: {e}"))?;
    if let (Some(m), None) = (v["message"].as_str(), v.get("tag_name")) {
        return Err(format!("GitHub: {m}"));
    }
    parse_release(&v)
}

fn sha256_hex(path: &Path) -> std::io::Result<String> {
    use sha2::Digest;
    let mut f = std::fs::File::open(path)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Unpacks `zip` into `dir` (a single top folder inside the zip is skipped). Returns the files.
pub fn unpack(zip: &Path, dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut a = zip::ZipArchive::new(std::fs::File::open(zip).map_err(|e| e.to_string())?).map_err(|_| "the download is not a zip")?;
    let names: Vec<PathBuf> = (0..a.len()).filter_map(|i| a.by_index(i).ok().and_then(|f| f.enclosed_name())).collect();
    let firsts: std::collections::HashSet<_> = names.iter().filter_map(|p| p.components().next().map(|c| c.as_os_str().to_owned())).collect();
    let top = (firsts.len() == 1 && names.iter().all(|p| p.components().count() > 1)).then(|| PathBuf::from(firsts.into_iter().next().unwrap()));
    let _ = std::fs::remove_dir_all(dir);
    let mut out = Vec::new();
    for i in 0..a.len() {
        let mut f = a.by_index(i).map_err(|e| e.to_string())?;
        let Some(p) = f.enclosed_name() else { continue };
        let rel = match &top {
            Some(t) => p.strip_prefix(t).map(Path::to_path_buf).unwrap_or(p),
            None => p,
        };
        if rel.as_os_str().is_empty() || f.is_dir() {
            continue;
        }
        let dest = dir.join(&rel);
        std::fs::create_dir_all(dest.parent().unwrap_or(dir)).map_err(|e| e.to_string())?;
        let mut w = std::fs::File::create(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        std::io::copy(&mut f, &mut w).map_err(|e| format!("{}: {e}", dest.display()))?;
        out.push(rel);
    }
    Ok(out)
}

/// Copies the unpacked files over `target`, moving existing (maybe running) files aside first.
pub fn swap_in(src: &Path, files: &[PathBuf], target: &Path) -> Result<(), String> {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    for rel in files {
        let to = target.join(rel);
        if to.exists() {
            let aside = to.with_file_name(format!("{}.old-{stamp}", to.file_name().unwrap_or_default().to_string_lossy()));
            std::fs::rename(&to, &aside).map_err(|e| format!("cannot move {} aside: {e}", to.display()))?;
        }
        std::fs::copy(src.join(rel), &to).map_err(|e| format!("cannot write {}: {e}", to.display()))?;
    }
    Ok(())
}

/// Removes `*.old-N` files an earlier update left next to the program (no longer in use).
pub fn clean_old() {
    let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) else { return };
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().contains(".old-") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Downloads the zip (with progress), checks it against its .sha256, then puts it in place.
pub fn install_stream(r: Release) -> impl futures::Stream<Item = UpdateEvent> {
    async_stream::stream! {
        let (Some(zip_url), Some(sha_url)) = (r.zip_url.clone(), r.sha_url.clone()) else {
            yield UpdateEvent::Done(Err(format!("release {} has no {ASSET} with a .sha256 — not installed", r.tag)));
            return;
        };
        let c = client();
        let want = match get(&c, &sha_url).send().await.and_then(|r| r.error_for_status()) {
            Ok(resp) => resp.text().await.unwrap_or_default().split_whitespace().next().unwrap_or("").to_lowercase(),
            Err(e) => { yield UpdateEvent::Done(Err(format!("checksum download failed: {e}"))); return; }
        };
        let resp = match get(&c, &zip_url).send().await.and_then(|r| r.error_for_status()) {
            Ok(r) => r,
            Err(e) => { yield UpdateEvent::Done(Err(format!("download failed: {e}"))); return; }
        };
        let total = resp.content_length().unwrap_or(0);
        let zip = std::env::temp_dir().join(format!("crdl-update-{}.zip", std::process::id()));
        let mut file = match tokio::fs::File::create(&zip).await {
            Ok(f) => f,
            Err(e) => { yield UpdateEvent::Done(Err(format!("cannot write a temp file: {e}"))); return; }
        };
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;
        let mut body = resp.bytes_stream();
        let (mut done, mut last_pct) = (0u64, -1i64);
        while let Some(chunk) = body.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => { yield UpdateEvent::Done(Err(format!("download interrupted: {e}"))); return; }
            };
            if let Err(e) = file.write_all(&chunk).await {
                yield UpdateEvent::Done(Err(format!("write error: {e}"))); return;
            }
            done += chunk.len() as u64;
            let pct = if total > 0 { (done * 100 / total) as i64 } else { -1 };
            if pct != last_pct {
                last_pct = pct;
                yield UpdateEvent::Progress(if total > 0 { done as f32 / total as f32 } else { -1.0 });
            }
        }
        if let Err(e) = file.flush().await {
            yield UpdateEvent::Done(Err(format!("finishing the download: {e}"))); return;
        }
        drop(file);
        let tag = r.tag.clone();
        let res = tokio::task::spawn_blocking(move || {
            let got = sha256_hex(&zip).map_err(|e| e.to_string())?;
            if want.len() != 64 || want != got {
                let _ = std::fs::remove_file(&zip);
                return Err("the download does not match its checksum — not installed".to_string());
            }
            put_in_place(&zip, &tag)
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        yield UpdateEvent::Done(res);
    }
}

fn put_in_place(zip: &Path, tag: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = exe.parent().ok_or("no program folder")?.to_path_buf();
    let dir = std::env::temp_dir().join(format!("crdl-update-{tag}"));
    let files = unpack(zip, &dir)?;
    let _ = std::fs::remove_file(zip);
    let exe_name = exe.file_name().unwrap_or_default();
    if !files.iter().any(|p| p.file_name() == Some(exe_name)) {
        return Err(format!("the update has no {}", exe_name.to_string_lossy()));
    }
    swap_in(&dir, &files, &target)?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(exe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.4"), Some((0, 4, 0)));
        assert_eq!(parse_version("v2.0.0-beta.1"), Some((2, 0, 0)));
        assert_eq!(parse_version("First-Release"), None);
    }

    #[test]
    fn release_json() {
        let v = serde_json::json!({"tag_name": "v9.0.0", "body": "notes", "html_url": "https://x/tag",
            "assets": [
                {"name": "crunchyroll-downloader-gui.exe", "browser_download_url": "https://x/gui.exe"},
                {"name": ASSET, "browser_download_url": "https://x/win.zip"},
                {"name": format!("{ASSET}.sha256"), "browser_download_url": "https://x/win.zip.sha256"}]});
        let r = parse_release(&v).unwrap();
        assert_eq!(r.version, (9, 0, 0));
        assert!(r.is_newer());
        assert_eq!(r.zip_url.as_deref(), Some("https://x/win.zip"));
        assert_eq!(r.sha_url.as_deref(), Some("https://x/win.zip.sha256"));
        // An old release with bare .exe files only: found, but not installable.
        let old = parse_release(&serde_json::json!({"tag_name": "v0.2.0", "assets": []})).unwrap();
        assert!(old.zip_url.is_none() && !old.is_newer());
        assert!(parse_release(&serde_json::json!({"message": "Not Found"})).is_err());
    }

    /// zip with a top folder → unpacked flat → swapped over a folder whose file is "in use".
    #[test]
    fn unpack_and_swap() {
        let base = std::env::temp_dir().join(format!("crdl-upd-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("app")).unwrap();
        std::fs::write(base.join("app/crunchyroll-downloader-gui.exe"), b"old").unwrap();
        std::fs::write(base.join("app/vault.bin"), b"keep").unwrap();
        let zp = base.join("u.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&zp).unwrap());
            let o = zip::write::SimpleFileOptions::default();
            z.start_file("pkg/crunchyroll-downloader-gui.exe", o).unwrap();
            z.write_all(b"new").unwrap();
            z.start_file("pkg/cr-login-helper.exe", o).unwrap();
            z.write_all(b"helper").unwrap();
            z.finish().unwrap();
        }
        let files = unpack(&zp, &base.join("x")).unwrap();
        assert!(files.contains(&PathBuf::from("crunchyroll-downloader-gui.exe")));
        swap_in(&base.join("x"), &files, &base.join("app")).unwrap();
        assert_eq!(std::fs::read(base.join("app/crunchyroll-downloader-gui.exe")).unwrap(), b"new");
        assert_eq!(std::fs::read(base.join("app/cr-login-helper.exe")).unwrap(), b"helper");
        assert_eq!(std::fs::read(base.join("app/vault.bin")).unwrap(), b"keep");
        let old: Vec<_> = std::fs::read_dir(base.join("app")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains(".old-")).collect();
        assert_eq!(old.len(), 1);
        assert_eq!(std::fs::read(old[0].path()).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&base);
    }
}
