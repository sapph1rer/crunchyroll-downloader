//! Encrypted storage for everything the GUI keeps: settings, the etp_rt cookie, and
//! the Widevine device (private key and client ID, or a .wvd file).
//!
//! File layout: magic "CRDV", version byte, 16-byte salt (the header), then a 12-byte
//! nonce and the AES-256-GCM ciphertext of the JSON contents. The header is bound to
//! the ciphertext as associated data, so changing it makes decryption fail. The key is
//! derived from the password with Argon2id. A wrong password fails the GCM check.
//!
//! Lost password means lost data. There is no recovery.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, bail, Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const MAGIC: &[u8; 4] = b"CRDV";
const VERSION: u8 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
/// Argon2id cost: 32 MiB of memory, 3 passes. The memory is released after unlocking.
const KDF_MEMORY_KIB: u32 = 32 * 1024;
const KDF_PASSES: u32 = 3;
pub const MIN_PASSWORD_LEN: usize = 8;

/// Non-secret preferences. Most mirror the CLI options. Numbers are kept as typed text,
/// so a bad value can be reported where it was entered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Zeroize)]
#[serde(default)]
pub struct Settings {
    pub output_dir: String,
    pub audio_langs: String,
    pub subs_langs: String,
    pub cc_langs: String,
    pub video_quality: String,
    pub audio_quality: String,
    pub download_delay: String,
    pub debug: bool,
    /// Path to ffmpeg, or to the folder that holds it. Empty means search as usual.
    pub ffmpeg_path: String,
    /// Segments fetched at once for each track (1 to 16).
    pub segment_workers: String,
    /// Audio dubs of one episode downloaded at once (1 to 4).
    pub track_workers: String,
    /// Minutes without any input before the vault locks. 0 turns this off.
    pub auto_lock_minutes: String,
    /// The last link fetched, restored on the next unlock.
    pub last_url: String,
    /// Episode IDs of the last download, selected again when the same link is fetched.
    pub last_picks: Vec<String>,
    /// Optional HTTP/HTTPS/SOCKS5 proxy URL used for Crunchyroll API calls only.
    /// CDN segment downloads bypass this proxy for maximum speed.
    /// Example: "http://user:pass@127.0.0.1:1080" or "socks5://127.0.0.1:1080"
    pub proxy_url: String,
    /// Show a desktop notification when a download queue finishes.
    pub notify_done: bool,
}

/// The default download folder: `Downloads\Crunchyroll`, falling back to the home folder,
/// and only as a last resort the current folder.
pub fn default_output_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .map(|d| d.join("Crunchyroll"))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Whether an output folder setting means "use the default" (empty, or the old "." default
/// that put files wherever the program happened to run from).
pub fn output_is_unset(value: &str) -> bool {
    matches!(value.trim(), "" | ".")
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            output_dir: default_output_dir().display().to_string(),
            audio_langs: "ja-JP".into(),
            subs_langs: "en-US".into(),
            cc_langs: String::new(),
            video_quality: "1080p".into(),
            audio_quality: "192k".into(),
            download_delay: "0s".into(),
            debug: false,
            ffmpeg_path: String::new(),
            segment_workers: "20".into(),
            track_workers: "2".into(),
            auto_lock_minutes: "10".into(),
            last_url: String::new(),
            last_picks: Vec::new(),
            proxy_url: String::new(),
            notify_done: true,
        }
    }
}

/// Secrets. Zeroed when dropped.
#[derive(Default, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct Secrets {
    #[serde(default)]
    pub etp_rt: String,
    #[serde(default)]
    pub client_id: Option<Vec<u8>>,
    #[serde(default)]
    pub private_key_pem: Option<String>,
    #[serde(default)]
    pub wvd: Option<Vec<u8>>,
}

impl Secrets {
    /// True when either a .wvd file or the client ID and private key pair is present.
    pub fn has_device(&self) -> bool {
        self.wvd.is_some() || (self.client_id.is_some() && self.private_key_pem.is_some())
    }
}

/// Everything inside the encrypted file. Zeroed when dropped, so the decrypted values do not
/// stay in memory after the vault is locked.
#[derive(Default, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct Contents {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub secrets: Secrets,
}

/// An unlocked vault. Holds the derived key while open, so saving does not repeat the
/// slow key derivation.
pub struct Vault {
    path: PathBuf,
    salt: [u8; SALT_LEN],
    key: Zeroizing<[u8; KEY_LEN]>,
    pub contents: Contents,
}

/// A file with this name next to the program turns on portable mode: the vault (and a
/// downloaded ffmpeg) then live next to the program instead of in the user's AppData, so
/// the whole folder can be carried on a USB drive.
pub const PORTABLE_MARKER: &str = "portable.txt";

/// The folder the running program is in.
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// Whether portable mode is on (the marker file sits next to the program).
pub fn is_portable() -> bool {
    exe_dir().is_some_and(|d| d.join(PORTABLE_MARKER).is_file())
}

/// Where the vault lives in normal (installed) mode: the per-user config folder.
pub fn installed_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().ok_or_else(|| anyhow!("no per-user config folder is available"))?;
    Ok(dir.join("crunchyroll-downloader").join("vault.bin"))
}

/// Where the vault lives: next to the program in portable mode, otherwise AppData.
pub fn default_path() -> Result<PathBuf> {
    if is_portable() {
        if let Some(dir) = exe_dir() {
            return Ok(dir.join("vault.bin"));
        }
    }
    installed_path()
}

/// Turns portable mode on or off for an open vault, moving the vault file to match.
/// Refuses to overwrite a different vault already sitting at the destination, and leaves
/// everything as it was if the move fails. Returns the vault's new location.
pub fn set_portable(vault: &mut Vault, on: bool) -> Result<PathBuf> {
    let dir = exe_dir().ok_or_else(|| anyhow!("cannot find the program's folder"))?;
    let marker = dir.join(PORTABLE_MARKER);
    let target = if on { dir.join("vault.bin") } else { installed_path()? };
    if target != vault.path && target.exists() {
        bail!(
            "a vault already exists at {}. Move or delete it first, so it is not overwritten.",
            target.display()
        );
    }
    if on {
        // Writing the marker first also proves the folder is writable (not Program Files).
        std::fs::write(
            &marker,
            "This file keeps Crunchyroll Downloader's encrypted vault next to the program (portable mode).\n\
             Delete it, or turn portable mode off in Settings, to go back to AppData.\n",
        )
        .with_context(|| format!("cannot write to {} (is the folder read-only?)", dir.display()))?;
        if let Err(e) = vault.move_to(&target) {
            let _ = std::fs::remove_file(&marker);
            return Err(e);
        }
    } else {
        vault.move_to(&target)?;
        let _ = std::fs::remove_file(&marker);
    }
    Ok(target)
}

pub fn exists(path: &Path) -> bool {
    path.is_file()
}

pub fn check_password(password: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        bail!("the password must be at least {MIN_PASSWORD_LEN} characters");
    }
    Ok(())
}

fn derive_key(password: &str, salt: &[u8]) -> Result<Zeroizing<[u8; KEY_LEN]>> {
    let params = Params::new(KDF_MEMORY_KIB, KDF_PASSES, 1, Some(KEY_LEN)).map_err(|e| anyhow!("{e}"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    argon
        .hash_password_into(password.as_bytes(), salt, key.as_mut())
        .map_err(|e| anyhow!("key derivation failed: {e}"))?;
    Ok(key)
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    rand::thread_rng().fill_bytes(&mut out);
    out
}

fn header(salt: &[u8; SALT_LEN]) -> Vec<u8> {
    let mut h = Vec::with_capacity(MAGIC.len() + 1 + SALT_LEN);
    h.extend_from_slice(MAGIC);
    h.push(VERSION);
    h.extend_from_slice(salt);
    h
}

/// Encrypts `plaintext` under `key`. Output: nonce followed by ciphertext.
fn seal(key: &[u8; KEY_LEN], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new(key.into());
    let nonce_bytes: [u8; NONCE_LEN] = random_bytes();
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), Payload { msg: plaintext, aad })
        .map_err(|_| anyhow!("encryption failed"))?;
    let mut out = nonce_bytes.to_vec();
    out.extend(ct);
    Ok(out)
}

fn open_sealed(key: &[u8; KEY_LEN], aad: &[u8], data: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    if data.len() < NONCE_LEN {
        bail!("vault file is truncated");
    }
    let (nonce, ct) = data.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(key.into());
    let pt = cipher
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad })
        .map_err(|_| anyhow!("wrong password, or the vault file was changed"))?;
    Ok(Zeroizing::new(pt))
}

/// Writes the new file beside the old one, flushes it to disk, and only then swaps it in.
/// A crash or power cut leaves either the old vault or the new one, never a partial file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::new(e)).with_context(|| format!("writing {}", tmp.display()));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::new(e)).with_context(|| format!("replacing {}", path.display()));
    }
    Ok(())
}

impl Vault {
    /// Creates an empty vault protected by `password`, and saves it.
    pub fn create(path: &Path, password: &str) -> Result<Self> {
        check_password(password)?;
        let salt: [u8; SALT_LEN] = random_bytes();
        let vault = Self {
            path: path.to_path_buf(),
            salt,
            key: derive_key(password, &salt)?,
            contents: Contents::default(),
        };
        vault.save()?;
        Ok(vault)
    }

    /// Opens an existing vault. Fails with a clear message when the password is wrong.
    pub fn open(path: &Path, password: &str) -> Result<Self> {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let hdr_len = MAGIC.len() + 1 + SALT_LEN;
        if data.len() < hdr_len || &data[..4] != MAGIC {
            bail!("this is not a vault file");
        }
        if data[4] != VERSION {
            bail!("unsupported vault version {}", data[4]);
        }
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&data[5..hdr_len]);
        let key = derive_key(password, &salt)?;
        let plain = open_sealed(&key, &data[..hdr_len], &data[hdr_len..])?;
        let contents: Contents = serde_json::from_slice(&plain).context("the vault contents are damaged")?;
        Ok(Self { path: path.to_path_buf(), salt, key, contents })
    }

    /// Encrypts the current contents with a fresh nonce and replaces the file.
    pub fn save(&self) -> Result<()> {
        let mut json = Zeroizing::new(serde_json::to_vec(&self.contents).context("serializing the vault")?);
        let hdr = header(&self.salt);
        let body = seal(&self.key, &hdr, &json)?;
        json.zeroize();
        let mut file = hdr;
        file.extend(body);
        write_atomic(&self.path, &file)
    }

    /// Saves the vault at a new location and removes the old file. On failure the vault
    /// keeps its old location and the old file is untouched.
    pub fn move_to(&mut self, new_path: &Path) -> Result<()> {
        if new_path == self.path {
            return self.save();
        }
        if let Some(parent) = new_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let old = std::mem::replace(&mut self.path, new_path.to_path_buf());
        if let Err(e) = self.save() {
            self.path = old;
            return Err(e);
        }
        let _ = std::fs::remove_file(&old);
        Ok(())
    }

    /// Re-encrypts everything under a new password, with a new salt.
    pub fn change_password(&mut self, new_password: &str) -> Result<()> {
        check_password(new_password)?;
        self.salt = random_bytes();
        self.key = derive_key(new_password, &self.salt)?;
        self.save()
    }

    /// Path of the file on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("crdv-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("vault.bin")
    }

    #[test]
    fn move_to_relocates_the_vault_and_keeps_it_readable() {
        let from = temp("move-from");
        let to = temp("move-to").with_file_name("moved").join("vault.bin");
        let mut v = Vault::create(&from, "correct horse").unwrap();
        v.contents = sample();
        v.save().unwrap();
        v.move_to(&to).unwrap();
        assert!(!from.exists(), "old file removed");
        assert_eq!(v.path(), to.as_path());
        let back = Vault::open(&to, "correct horse").unwrap();
        assert_eq!(&*back.contents.secrets.etp_rt, "SECRET-ETP-VALUE-123");
    }

    #[test]
    fn old_dot_output_counts_as_unset() {
        assert!(output_is_unset("."));
        assert!(output_is_unset("  "));
        assert!(!output_is_unset("D:\\Anime"));
        assert!(default_output_dir().ends_with("Crunchyroll"));
    }

    fn sample() -> Contents {
        Contents {
            settings: Settings { audio_langs: "ja-JP,en-US".into(), ..Settings::default() },
            secrets: Secrets {
                etp_rt: "SECRET-ETP-VALUE-123".into(),
                client_id: Some(vec![1, 2, 3, 4]),
                private_key_pem: Some("-----BEGIN PRIVATE KEY-----".into()),
                wvd: None,
            },
        }
    }

    #[test]
    fn round_trip_restores_contents() {
        let path = temp("roundtrip");
        let mut v = Vault::create(&path, "correct horse").unwrap();
        v.contents = sample();
        v.save().unwrap();
        let back = Vault::open(&path, "correct horse").unwrap();
        assert_eq!(back.contents.secrets.etp_rt, "SECRET-ETP-VALUE-123");
        assert_eq!(back.contents.secrets.client_id, Some(vec![1, 2, 3, 4]));
        assert_eq!(back.contents.settings.audio_langs, "ja-JP,en-US");
    }

    #[test]
    fn wrong_password_is_rejected() {
        let path = temp("wrong");
        Vault::create(&path, "correct horse").unwrap();
        let err = Vault::open(&path, "battery staple").err().expect("must fail");
        assert!(format!("{err:#}").contains("wrong password"), "{err:#}");
    }

    #[test]
    fn file_never_contains_plaintext_secrets() {
        let path = temp("plain");
        let mut v = Vault::create(&path, "correct horse").unwrap();
        v.contents = sample();
        v.save().unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert!(!raw.windows(20).any(|w| w == b"SECRET-ETP-VALUE-123"));
        assert!(!raw.windows(11).any(|w| w == b"BEGIN PRIVA"));
    }

    #[test]
    fn tampering_is_detected() {
        let path = temp("tamper");
        let mut v = Vault::create(&path, "correct horse").unwrap();
        v.contents = sample();
        v.save().unwrap();
        let mut raw = std::fs::read(&path).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        std::fs::write(&path, &raw).unwrap();
        assert!(Vault::open(&path, "correct horse").is_err());
    }

    #[test]
    fn header_is_bound_to_ciphertext() {
        let path = temp("header");
        Vault::create(&path, "correct horse").unwrap();
        let mut raw = std::fs::read(&path).unwrap();
        raw[5] ^= 0x01; // flip a salt byte
        std::fs::write(&path, &raw).unwrap();
        assert!(Vault::open(&path, "correct horse").is_err());
    }

    #[test]
    fn change_password_replaces_the_old_one() {
        let path = temp("change");
        let mut v = Vault::create(&path, "first password").unwrap();
        v.contents = sample();
        v.change_password("second password").unwrap();
        assert!(Vault::open(&path, "first password").is_err());
        let back = Vault::open(&path, "second password").unwrap();
        assert_eq!(back.contents.secrets.etp_rt, "SECRET-ETP-VALUE-123");
    }

    #[test]
    fn short_passwords_are_refused() {
        assert!(check_password("short").is_err());
        assert!(check_password("long enough").is_ok());
    }

    #[test]
    fn not_a_vault_is_reported() {
        let path = temp("garbage");
        std::fs::write(&path, b"hello world, not a vault").unwrap();
        let err = Vault::open(&path, "correct horse").err().expect("must fail");
        assert!(format!("{err:#}").contains("not a vault"));
    }

    #[test]
    fn device_presence_is_detected() {
        let mut s = Secrets::default();
        assert!(!s.has_device());
        s.client_id = Some(vec![1]);
        assert!(!s.has_device());
        s.private_key_pem = Some("k".into());
        assert!(s.has_device());
    }
}
