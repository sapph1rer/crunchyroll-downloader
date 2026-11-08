use crate::api::EpisodeInfo;
use crate::util::{track_title, LANGUAGE_CODES};
use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use tokio::process::Command;

/// The ffmpeg binary the last `locate` or `locate_with` call chose. It can change, so the
/// GUI can pick a different binary without a restart.
static FFMPEG: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Returned when no ffmpeg binary can be found.
#[derive(Debug, thiserror::Error)]
#[error("ffmpeg not found")]
pub struct FfmpegNotFound;

/// Windows exit code when a process cannot start because a DLL it needs is missing.
const STATUS_DLL_NOT_FOUND: i32 = 0xC0000135u32 as i32;

/// Windows flag: start a console program without opening a console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Keeps ffmpeg from opening a console window when the GUI starts it.
fn hide_console(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    #[cfg(not(windows))]
    let _ = cmd;
}

fn hide_console_std(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// Runs `ffmpeg -version` so a broken binary fails before anything is downloaded.
/// Windows reports a missing DLL as exit code 0xc0000135, which gets its own message.
pub fn verify(path: &Path) -> Result<()> {
    let mut cmd = std::process::Command::new(path);
    cmd.arg("-version");
    hide_console_std(&mut cmd);
    let out = cmd
        .output()
        .with_context(|| format!("starting {}", path.display()))?;
    if out.status.success() {
        return Ok(());
    }
    bail!("{}", describe_failure(path, out.status.code()))
}

/// Explains a failed `ffmpeg -version` run.
fn describe_failure(path: &Path, code: Option<i32>) -> String {
    if code == Some(STATUS_DLL_NOT_FOUND) {
        format!(
            "ffmpeg was found at {} but could not start: a DLL it needs is missing (exit code 0xc0000135).\n\
             Replace it with a complete build, for example the \"essentials\" zip from gyan.dev, and put ffmpeg.exe next to crunchyroll-downloader.exe.",
            path.display()
        )
    } else {
        format!("ffmpeg at {} failed its -version check (exit code {code:?})", path.display())
    }
}

/// Temporary name for a download in progress. It keeps the .mkv extension so ffmpeg can
/// pick the container, and it never equals a finished name, so a partial file cannot
/// be mistaken for a complete episode.
pub fn part_path(output: &Path) -> PathBuf {
    let stem = output.file_stem().unwrap_or_default().to_string_lossy();
    output.with_file_name(format!("{stem}.part.mkv"))
}

/// Path to the resolved ffmpeg binary (set by `locate` or `locate_with`, defaults to "ffmpeg").
pub fn path() -> PathBuf {
    FFMPEG
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

/// Finds ffmpeg on PATH, in the current directory, or next to this executable,
/// and remembers it for `merge`. Port of Go's findFFmpeg.
pub fn locate() -> Result<PathBuf> {
    locate_with("")
}

/// Like `locate`, but a non-empty `custom` path is used instead of searching. It may name
/// the ffmpeg binary or the folder that contains it. Either way the result is remembered.
pub fn locate_with(custom: &str) -> Result<PathBuf> {
    let custom = custom.trim();
    let found = if custom.is_empty() {
        let exe = std::env::current_exe().ok();
        let cwd = std::env::current_dir().context("reading current directory")?;
        let path_env = std::env::var_os("PATH").unwrap_or_default();
        find_ffmpeg_from(exe.as_deref(), &path_env, &cwd)?
    } else {
        let given = PathBuf::from(custom);
        let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
        let binary = if given.is_dir() { given.join(name) } else { given.clone() };
        if !binary.is_file() {
            bail!("no ffmpeg binary at {}", given.display());
        }
        absolute(&binary)?
    };
    *FFMPEG.write().unwrap_or_else(|e| e.into_inner()) = Some(found.clone());
    Ok(found)
}

/// Port of Go's findFFmpegFrom. Searches PATH, then the directory of `exe`. A
/// relative PATH entry (such as ".") resolves against `cwd`, reproducing the Go
/// exec.ErrDot case, and every result is returned as an absolute path.
pub fn find_ffmpeg_from(exe: Option<&Path>, path_env: &OsStr, cwd: &Path) -> Result<PathBuf> {
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    for dir in std::env::split_paths(path_env) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let dir = if dir.is_absolute() { dir } else { cwd.join(dir) };
        if let Some(p) = executable_in(&dir, name) {
            return absolute(&p);
        }
    }
    if let Some(dir) = exe.and_then(Path::parent) {
        if let Some(p) = executable_in(dir, name) {
            return absolute(&p);
        }
    }
    Err(anyhow::Error::new(FfmpegNotFound))
}

fn executable_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let p = dir.join(name);
    let meta = std::fs::metadata(&p).ok()?;
    if !meta.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    Some(p)
}

fn absolute(p: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(p).with_context(|| format!("resolving {}", p.display()))
}

pub struct MediaTrack {
    pub file: PathBuf,
    pub locale: String,
    pub format: String,
    pub is_cc: bool,
}

/// Muxes video, audio and subtitle tracks into one MKV, with the same stream
/// mapping, dispositions and metadata as the Go `mergeEverything`.
pub async fn merge(
    video: &Path,
    audio: &[MediaTrack],
    subs: &[MediaTrack],
    output: &Path,
    info: &EpisodeInfo,
) -> Result<()> {
    let mut args: Vec<String> = Vec::new();
    args.extend(["-i".into(), video.display().to_string()]);
    for t in audio.iter().chain(subs.iter()) {
        args.extend(["-i".into(), t.file.display().to_string()]);
    }

    args.extend(["-map".into(), "0:v:0".into()]);
    for i in 0..audio.len() {
        args.extend(["-map".into(), format!("{}:a:0", 1 + i)]);
    }
    for j in 0..subs.len() {
        args.extend(["-map".into(), format!("{}", 1 + audio.len() + j)]);
    }

    args.extend(["-c:v".into(), "copy".into(), "-c:a".into(), "copy".into()]);
    // VTT is transcoded to SRT (no cue-positioning syntax); ASS is copied.
    for (j, s) in subs.iter().enumerate() {
        let codec = if s.format == "vtt" { "srt" } else { "copy" };
        args.extend([format!("-c:s:{j}"), codec.into()]);
    }

    for (i, a) in audio.iter().enumerate() {
        args.extend([
            format!("-metadata:s:a:{i}"),
            format!("language={}", LANGUAGE_CODES.get(a.locale.as_str()).copied().unwrap_or("")),
            format!("-metadata:s:a:{i}"),
            format!("title={}", track_title(&a.locale)),
        ]);
    }
    for (j, s) in subs.iter().enumerate() {
        let mut title = track_title(&s.locale);
        if s.is_cc {
            title.push_str(" [CC]");
        }
        args.extend([
            format!("-metadata:s:s:{j}"),
            format!("language={}", LANGUAGE_CODES.get(s.locale.as_str()).copied().unwrap_or("")),
            format!("-metadata:s:s:{j}"),
            format!("title={title}"),
        ]);
    }

    // Only the first audio track and first non-CC subtitle are default.
    for i in 0..audio.len() {
        let d = if i == 0 { "default" } else { "0" };
        args.extend([format!("-disposition:a:{i}"), d.into()]);
    }
    let mut default_sub_set = false;
    for (j, s) in subs.iter().enumerate() {
        let d = if !s.is_cc && !default_sub_set {
            default_sub_set = true;
            "default"
        } else {
            "0"
        };
        args.extend([format!("-disposition:s:{j}"), d.into()]);
    }

    let m = &info.episode_metadata;
    args.extend([
        "-metadata:g".into(),
        format!("title=S{:02}E{:02} - {}", m.season_number, m.episode_number, info.title),
        "-metadata:g".into(),
        format!("show={}", m.series_title),
        "-metadata:g".into(),
        format!("track={}", m.episode_number),
        "-metadata:g".into(),
        format!("season_number={}", m.season_number),
    ]);
    let part = part_path(output);
    args.push(part.display().to_string());

    let mut cmd = Command::new(path());
    cmd.args(&args);
    hide_console(&mut cmd);
    let out = cmd
        .output()
        .await
        .context("starting ffmpeg")?;
    if !out.status.success() {
        let _ = tokio::fs::remove_file(&part).await;
        bail!(
            "ffmpeg failed: {}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // Only a finished file gets the real name.
    tokio::fs::rename(&part, output)
        .await
        .with_context(|| format!("moving {} into place", part.display()))?;

    for p in std::iter::once(video.to_path_buf())
        .chain(audio.iter().map(|t| t.file.clone()))
        .chain(subs.iter().map(|t| t.file.clone()))
    {
        let _ = tokio::fs::remove_file(p).await;
    }
    crate::say!("\nDownload finished! Output file: {}\n", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{EpisodeInfo, EpisodeMetadata};

    /// Builds real tracks with ffmpeg and checks that `merge` produces an MKV
    /// containing the expected streams. Skipped if ffmpeg is unavailable.
    #[tokio::test]
    async fn merge_produces_expected_streams() {
        let Ok(_) = locate() else {
            crate::say!("ffmpeg not found, skipping");
            return;
        };
        let dir = std::env::temp_dir().join(format!("crdl-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("v.mp4");
        let audio = dir.join("a.m4a");
        let sub = dir.join("s.srt");
        let run = |args: &[&str]| {
            let st = std::process::Command::new(path()).args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).status().unwrap();
            assert!(st.success(), "ffmpeg setup failed: {args:?}");
        };
        run(&["-f", "lavfi", "-i", "testsrc=duration=1:size=320x240:rate=10", "-c:v", "libx264", video.to_str().unwrap()]);
        run(&["-f", "lavfi", "-i", "sine=duration=1", "-c:a", "aac", audio.to_str().unwrap()]);
        std::fs::write(&sub, "1\n00:00:00,000 --> 00:00:01,000\nhi\n").unwrap();

        let info = EpisodeInfo {
            episode_metadata: EpisodeMetadata {
                audio_locale: "ja-JP".into(),
                episode_number: 1,
                season_number: 1,
                series_title: "Test".into(),
                availability_starts: String::new(),
                versions: vec![],
            },
            title: "Pilot".into(),
        };
        let out = dir.join("out.mkv");
        let audio_t = vec![MediaTrack { file: audio.clone(), locale: "ja-JP".into(), format: String::new(), is_cc: false }];
        let sub_t = vec![MediaTrack { file: sub.clone(), locale: "en-US".into(), format: "srt".into(), is_cc: false }];
        // merge deletes its inputs, so pass copies of the video and sub-tracks.
        let video_copy = dir.join("v2.mp4");
        std::fs::copy(&video, &video_copy).unwrap();
        merge(&video_copy, &audio_t, &sub_t, &out, &info).await.unwrap();

        let probe = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=codec_type", "-of", "csv=p=0"])
            .arg(&out)
            .output()
            .unwrap();
        let kinds = String::from_utf8_lossy(&probe.stdout);
        let kinds: Vec<&str> = kinds.lines().collect();
        assert!(kinds.contains(&"video") && kinds.contains(&"audio") && kinds.contains(&"subtitle"), "streams: {kinds:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fresh empty directory under the system temp dir.
    fn scratch_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("crdl-ffmpeg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Writes an executable named like ffmpeg in `dir` and returns its path.
    fn write_fake_ffmpeg(dir: &Path) -> PathBuf {
        let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    fn assert_same_file(got: &Path, want: &Path) {
        assert!(got.is_absolute(), "got relative path {got:?}, want an absolute one");
        let same = std::fs::canonicalize(got).unwrap() == std::fs::canonicalize(want).unwrap();
        assert!(same, "got {got:?}, want {want:?}");
    }

    /// Port of Go TestFindFFmpegOnPath.
    #[test]
    fn find_ffmpeg_on_path() {
        let dir = scratch_dir();
        let want = write_fake_ffmpeg(&dir);
        let got = find_ffmpeg_from(None, dir.as_os_str(), &dir).unwrap();
        assert_same_file(&got, &want);
    }

    /// Port of Go TestFindFFmpegInCurrentDirectory. A relative PATH entry
    /// reproduces the Go exec.ErrDot case (the working directory is searched).
    #[test]
    fn find_ffmpeg_in_current_directory() {
        let dir = scratch_dir();
        let want = write_fake_ffmpeg(&dir);
        let got = find_ffmpeg_from(None, OsStr::new("."), &dir).unwrap();
        assert_same_file(&got, &want);
    }

    /// Port of Go TestFindFFmpegNextToExecutable.
    #[test]
    fn find_ffmpeg_next_to_executable() {
        let empty_path = scratch_dir();
        let exe_dir = scratch_dir();
        let want = write_fake_ffmpeg(&exe_dir);
        let exe = exe_dir.join("crunchyroll-downloader");
        let got = find_ffmpeg_from(Some(&exe), empty_path.as_os_str(), &empty_path).unwrap();
        assert_same_file(&got, &want);
    }

    /// A custom path that does not exist is refused, and the remembered binary is left alone.
    /// (Only the failure is tested: a success would change the global used by other tests.)
    #[test]
    fn custom_path_must_exist() {
        let missing = scratch_dir().join("no-such-ffmpeg");
        let err = locate_with(&missing.display().to_string()).unwrap_err();
        assert!(err.to_string().contains("no ffmpeg binary at"), "{err}");
    }

    /// Port of Go TestFindFFmpegMissing.
    #[test]
    fn find_ffmpeg_missing() {
        let empty_path = scratch_dir();
        let exe = scratch_dir().join("crunchyroll-downloader");
        let err = find_ffmpeg_from(Some(&exe), empty_path.as_os_str(), &empty_path).unwrap_err();
        assert!(err.downcast_ref::<FfmpegNotFound>().is_some(), "{err}");
    }
}

#[cfg(test)]
mod preflight_tests {
    use super::*;

    #[test]
    fn dll_failure_gets_its_own_message() {
        let msg = describe_failure(Path::new("C:/x/ffmpeg.exe"), Some(STATUS_DLL_NOT_FOUND));
        assert!(msg.contains("DLL it needs is missing"), "{msg}");
        assert!(msg.contains("C:/x/ffmpeg.exe"), "{msg}");
        let other = describe_failure(Path::new("ffmpeg"), Some(1));
        assert!(other.contains("exit code Some(1)"), "{other}");
    }

    #[test]
    fn part_path_keeps_mkv_and_is_distinct() {
        let out = Path::new("dir/Show S01E01 - Title [th-TH].mkv");
        let part = part_path(out);
        assert_eq!(part, Path::new("dir/Show S01E01 - Title [th-TH].part.mkv"));
        assert_ne!(part, out);
    }
}
