//! Widevine CDM: device loading, license challenge, license parsing.
//!
//! Port of the parts of `iyear/gowidevine` that the downloader uses.
//!
//! Protocol summary (matches pywidevine's `cdm.py` and `license_protocol.proto`):
//! 1. Build `LicenseRequest` (protobuf): client id blob, PSSH payload, random
//!    request id, timestamp and key-control nonce.
//! 2. Sign the serialized request with RSASSA-PSS (SHA-1, 20-byte salt) using the
//!    device key. Send `SignedMessage { type=LICENSE_REQUEST, msg, signature }`.
//!    The request carries NO session key.
//! 3. The server returns `SignedMessage { type=LICENSE, msg, signature,
//!    session_key, oemcrypto_core_message }`. `session_key` is a 16-byte AES key
//!    wrapped with RSA-OAEP(SHA-1) to the device public key.
//! 4. Unwrap the session key, derive the enc key and server MAC key with AES-CMAC
//!    (SP800-108 counter mode, context = label || 0x00 || request || bits),
//!    verify HMAC-SHA256 over `oemcrypto_core_message || msg`, then decrypt each
//!    CONTENT key with AES-128-CBC and PKCS#7 unpadding.
//!
//! The protobuf field numbers and derivation are checked against pywidevine's
//! source. Live Crunchyroll behaviour is not verified; the tests use a local
//! mock license server.

use crate::drm::ContentKey;
use anyhow::{anyhow, bail, Context, Result};
use cmac::{Cmac, Mac as CmacMac};
use hmac::{Hmac, Mac};
use rand::RngCore;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::{RandomizedSigner, SignatureEncoding};
use rsa::{Oaep, RsaPrivateKey};
use sha1::Sha1;
use sha2::Sha256;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Widevine device: RSA identity key plus the ClientIdentification blob.
#[derive(Clone)]
pub struct Device {
    pub key: RsaPrivateKey,
    pub client_id: Vec<u8>,
}

/// The device in use. Set once per run by `device()` or by `install`, and replaceable
/// so the GUI can switch devices without restarting.
static DEVICE: RwLock<Option<Arc<Device>>> = RwLock::new(None);

/// Makes `dev` the device for all later decryption.
pub fn install(dev: Arc<Device>) {
    let mut guard = DEVICE.write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(dev);
}

/// Builds a device from the client ID bytes and the PEM text of its RSA key.
pub fn device_from_parts(client_id: Vec<u8>, pem_text: &str) -> Result<Device> {
    let key = RsaPrivateKey::from_pkcs8_pem(pem_text)
        .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem_text))
        .context("parsing private_key.pem")?;
    Ok(Device { key, client_id })
}

/// Loads the device once. Looks in the current directory first, then in the folder
/// that holds the program. Within a folder it searches for a `.wvd` file first, then
/// `client_id.bin` plus `private_key.pem`, as the Go version does.
pub fn device() -> Result<Arc<Device>> {
    let current = DEVICE.read().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(d) = current {
        return Ok(d);
    }
    let dev = Arc::new(find_device(&search_dirs())?);
    install(dev.clone());
    Ok(dev)
}

/// Folders checked for a device, in order, without duplicates.
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from(".")];
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        let same = match (std::fs::canonicalize("."), std::fs::canonicalize(&exe_dir)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if !same {
            dirs.push(exe_dir);
        }
    }
    dirs
}

/// True when `dir` holds a `.wvd` file, or the `client_id.bin` and `private_key.pem` pair.
fn has_device_files(dir: &Path) -> bool {
    let has_wvd = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.path().extension().is_some_and(|x| x == "wvd"));
    has_wvd || (dir.join("client_id.bin").is_file() && dir.join("private_key.pem").is_file())
}

/// Loads the device from the first folder in `dirs` that has device files.
pub fn find_device(dirs: &[PathBuf]) -> Result<Device> {
    for dir in dirs {
        if has_device_files(dir) {
            return load_device_from_dir(dir);
        }
    }
    let looked: Vec<String> = dirs
        .iter()
        .map(|d| {
            if d == Path::new(".") {
                std::env::current_dir().unwrap_or_else(|_| d.clone())
            } else {
                d.clone()
            }
            .display()
            .to_string()
        })
        .collect();
    bail!("{} Looked in: {}", NO_DEVICE_HELP, looked.join(", "))
}

/// Explains what the user must provide when no device is found.
pub const NO_DEVICE_HELP: &str = "Put a \".wvd\" file, or \"client_id.bin\" and \"private_key.pem\", in one of the folders below.";

pub fn load_device_from_dir(dir: &Path) -> Result<Device> {
    // Go reads the directory in filename order and takes the first ".wvd".
    let mut names: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    names.sort();
    let wvd = names
        .into_iter()
        .find(|p| p.extension().is_some_and(|e| e == "wvd"));
    if let Some(p) = wvd {
        return parse_wvd(&std::fs::read(p)?);
    }
    let cid = dir.join("client_id.bin");
    let pem = dir.join("private_key.pem");
    if cid.is_file() && pem.is_file() {
        let client_id = std::fs::read(cid)?;
        let pem_text = std::fs::read_to_string(pem)?;
        return device_from_parts(client_id, &pem_text);
    }
    bail!(
        "no widevine device provided. You either need:\n- a \".wvd\" file,\n- or \"client_id.bin\" and \"private_key.pem\" files.\nI'm not sharing links for obvious reasons, but search \"ready to use cdms\" on Google :)\n"
    )
}

/// Parses a WVD v2 file: "WVD", version, device type, security level, flags,
/// then u16 BE length + PKCS#1 DER private key, then u16 BE length + client id.
pub fn parse_wvd(b: &[u8]) -> Result<Device> {
    if b.len() < 7 || &b[..3] != b"WVD" {
        bail!("not a WVD file");
    }
    if b[3] != 2 {
        bail!("unsupported WVD version {}", b[3]);
    }
    let mut p = 7;
    let key_len = read_u16(b, &mut p)?;
    let key_der = take(b, &mut p, key_len)?;
    let cid_len = read_u16(b, &mut p)?;
    let client_id = take(b, &mut p, cid_len)?.to_vec();
    let key = RsaPrivateKey::from_pkcs1_der(key_der).context("parsing WVD private key")?;
    Ok(Device { key, client_id })
}

fn read_u16(b: &[u8], p: &mut usize) -> Result<usize> {
    let s = take(b, p, 2)?;
    Ok(u16::from_be_bytes([s[0], s[1]]) as usize)
}

fn take<'a>(b: &'a [u8], p: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = p.checked_add(n).ok_or_else(|| anyhow!("length overflow"))?;
    let s = b.get(*p..end).ok_or_else(|| anyhow!("truncated input"))?;
    *p = end;
    Ok(s)
}

/// Extracts the Widevine `pssh_data` payload from a full PSSH box.
pub fn pssh_payload(b: &[u8]) -> Result<Vec<u8>> {
    if b.len() < 32 || &b[4..8] != b"pssh" {
        bail!("not a PSSH box");
    }
    let version = b[8];
    let mut p = 28; // size(4) type(4) version+flags(4) systemID(16)
    if version > 0 {
        let n = u32::from_be_bytes(take(b, &mut p, 4)?.try_into()?) as usize;
        take(b, &mut p, 16 * n)?;
    }
    let len = u32::from_be_bytes(take(b, &mut p, 4)?.try_into()?) as usize;
    Ok(take(b, &mut p, len)?.to_vec())
}

// ---------- minimal protobuf codec ----------

#[derive(Default)]
struct Pb(Vec<u8>);

impl Pb {
    fn varint(&mut self, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.0.push(b);
                return;
            }
            self.0.push(b | 0x80);
        }
    }
    fn uint(&mut self, field: u32, v: u64) {
        self.varint((field as u64) << 3);
        self.varint(v);
    }
    fn bytes(&mut self, field: u32, data: &[u8]) {
        self.varint(((field as u64) << 3) | 2);
        self.varint(data.len() as u64);
        self.0.extend_from_slice(data);
    }
}

#[derive(Debug, Clone, Copy)]
enum Val<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}

fn read_varint(b: &[u8]) -> Result<(u64, &[u8])> {
    let mut v = 0u64;
    for (i, &x) in b.iter().enumerate() {
        if i >= 10 {
            break;
        }
        v |= ((x & 0x7f) as u64) << (7 * i);
        if x & 0x80 == 0 {
            return Ok((v, &b[i + 1..]));
        }
    }
    bail!("truncated varint")
}

fn parse_fields(mut b: &[u8]) -> Result<Vec<(u32, Val<'_>)>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let (key, rest) = read_varint(b)?;
        let field = (key >> 3) as u32;
        match key & 7 {
            0 => {
                let (v, r) = read_varint(rest)?;
                out.push((field, Val::Int(v)));
                b = r;
            }
            1 => {
                if rest.len() < 8 {
                    bail!("truncated fixed64");
                }
                b = &rest[8..];
            }
            2 => {
                let (len, r) = read_varint(rest)?;
                let len = len as usize;
                if r.len() < len {
                    bail!("truncated length-delimited field {field}");
                }
                out.push((field, Val::Bytes(&r[..len])));
                b = &r[len..];
            }
            5 => {
                if rest.len() < 4 {
                    bail!("truncated fixed32");
                }
                b = &rest[4..];
            }
            w => bail!("unsupported protobuf wire type {w}"),
        }
    }
    Ok(out)
}

fn get_bytes<'a>(fields: &[(u32, Val<'a>)], n: u32) -> Option<&'a [u8]> {
    fields.iter().find_map(|(f, v)| match v {
        Val::Bytes(b) if *f == n => Some(*b),
        _ => None,
    })
}

fn get_int(fields: &[(u32, Val<'_>)], n: u32) -> Option<u64> {
    fields.iter().find_map(|(f, v)| match v {
        Val::Int(i) if *f == n => Some(*i),
        _ => None,
    })
}

// ---------- challenge ----------

const MSG_LICENSE_REQUEST: u64 = 1;
const MSG_LICENSE: u64 = 2;
const REQUEST_TYPE_NEW: u64 = 1;
const PROTOCOL_VERSION_21: u64 = 21;
const LICENSE_TYPE_AUTOMATIC: u64 = 3;
const KEY_TYPE_CONTENT: u64 = 2;

/// State that must survive from the challenge to the license response.
pub struct Session {
    pub request_msg: Vec<u8>,
}

/// Builds the serialized `SignedMessage` challenge plus the session state.
pub fn build_challenge(dev: &Device, pssh_data: &[u8], request_time: u64, nonce: u32) -> Result<(Vec<u8>, Session)> {
    let mut rng = rand::thread_rng();
    let mut request_id = [0u8; 16];
    rng.fill_bytes(&mut request_id);

    // WidevinePsshData { pssh_data = 1, license_type = 2, request_id = 3 }
    let mut wpd = Pb::default();
    wpd.bytes(1, pssh_data);
    wpd.uint(2, LICENSE_TYPE_AUTOMATIC);
    wpd.bytes(3, &request_id);
    // ContentIdentification { widevine_pssh_data = 1 }
    let mut cid = Pb::default();
    cid.bytes(1, &wpd.0);
    // LicenseRequest
    let mut lr = Pb::default();
    lr.bytes(1, &dev.client_id);
    lr.bytes(2, &cid.0);
    lr.uint(3, REQUEST_TYPE_NEW);
    lr.uint(4, request_time);
    lr.uint(6, PROTOCOL_VERSION_21);
    lr.uint(7, nonce as u64);
    let request_msg = lr.0;

    let signer = rsa::pss::SigningKey::<Sha1>::new(dev.key.clone());
    let signature = signer.sign_with_rng(&mut rng, &request_msg).to_bytes();

    let mut sm = Pb::default();
    sm.uint(1, MSG_LICENSE_REQUEST);
    sm.bytes(2, &request_msg);
    sm.bytes(3, &signature);

    Ok((sm.0, Session { request_msg }))
}

// ---------- key derivation ----------

fn cmac_block(key: &[u8], data: &[u8]) -> Result<[u8; 16]> {
    let mut m = <Cmac<aes::Aes128> as CmacMac>::new_from_slice(key)
        .map_err(|e| anyhow!("cmac key: {e}"))?;
    CmacMac::update(&mut m, data);
    let out = CmacMac::finalize(m).into_bytes();
    Ok(out.into())
}

/// NIST SP800-108 counter-mode KDF with AES-CMAC, as used by Widevine.
fn kdf(session_key: &[u8], label: &[u8], context: &[u8], blocks: u8, bits: u32) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(16 * blocks as usize);
    for i in 1..=blocks {
        let mut input = vec![i];
        input.extend_from_slice(label);
        input.push(0);
        input.extend_from_slice(context);
        input.extend_from_slice(&bits.to_be_bytes());
        out.extend_from_slice(&cmac_block(session_key, &input)?);
    }
    Ok(out)
}

/// Returns (encryption key, server MAC key) for a session.
pub fn derive_keys(session_key: &[u8], request_msg: &[u8]) -> Result<([u8; 16], [u8; 32])> {
    let enc = kdf(session_key, b"ENCRYPTION", request_msg, 1, 128)?;
    let mac = kdf(session_key, b"AUTHENTICATION", request_msg, 4, 512)?;
    let mut enc_key = [0u8; 16];
    enc_key.copy_from_slice(&enc);
    // Counters 1-2 form the server MAC key, counters 3-4 the client MAC key.
    let mut server_mac = [0u8; 32];
    server_mac.copy_from_slice(&mac[..32]);
    Ok((enc_key, server_mac))
}

// ---------- license ----------

/// Verifies and decrypts a license response into content keys.
pub fn parse_license(dev: &Device, session: &Session, response: &[u8]) -> Result<Vec<ContentKey>> {
    let sm = parse_fields(response)?;
    if get_int(&sm, 1) != Some(MSG_LICENSE) {
        bail!("license response is not a LICENSE message");
    }
    let license = get_bytes(&sm, 2).ok_or_else(|| anyhow!("license response has no msg"))?;
    let signature = get_bytes(&sm, 3).ok_or_else(|| anyhow!("license response has no signature"))?;
    let wrapped_session_key = get_bytes(&sm, 4).ok_or_else(|| anyhow!("license response has no session_key"))?;
    let core_message = get_bytes(&sm, 9).unwrap_or(&[]);

    let session_key = dev
        .key
        .decrypt(Oaep::new::<Sha1>(), wrapped_session_key)
        .context("unwrapping session key")?;
    let (enc_key, server_mac) = derive_keys(&session_key, &session.request_msg)?;

    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&server_mac)
        .map_err(|e| anyhow!("hmac key: {e}"))?;
    Mac::update(&mut mac, core_message);
    Mac::update(&mut mac, license);
    Mac::verify_slice(mac, signature).map_err(|_| anyhow!("license signature mismatch"))?;

    let mut keys = Vec::new();
    for (f, v) in parse_fields(license)? {
        if f != 3 {
            continue;
        }
        let Val::Bytes(kc_bytes) = v else { continue };
        let kc = parse_fields(kc_bytes)?;
        if get_int(&kc, 4) != Some(KEY_TYPE_CONTENT) {
            continue;
        }
        let kid = get_bytes(&kc, 1).ok_or_else(|| anyhow!("key container without id"))?;
        let iv = get_bytes(&kc, 2).ok_or_else(|| anyhow!("key container without iv"))?;
        let wrapped = get_bytes(&kc, 3).ok_or_else(|| anyhow!("key container without key"))?;
        let key = aes_cbc_decrypt(&enc_key, iv, wrapped)?;
        keys.push(ContentKey {
            kid: kid.try_into().map_err(|_| anyhow!("KID is not 16 bytes"))?,
            key: key.try_into().map_err(|_| anyhow!("content key is not 16 bytes"))?,
        });
    }
    if keys.is_empty() {
        bail!("license contained no content keys");
    }
    Ok(keys)
}

fn aes_cbc_decrypt(key: &[u8; 16], iv: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    let dec = cbc::Decryptor::<aes::Aes128>::new_from_slices(key, iv)
        .map_err(|e| anyhow!("aes-cbc init: {e}"))?;
    let mut buf = data.to_vec();
    let out = dec
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| anyhow!("aes-cbc decrypt: {e}"))?;
    Ok(out.to_vec())
}

#[cfg(test)]
pub(crate) fn aes_cbc_encrypt(key: &[u8; 16], iv: &[u8], data: &[u8]) -> Vec<u8> {
    use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
    let enc = cbc::Encryptor::<aes::Aes128>::new_from_slices(key, iv).unwrap();
    let mut buf = vec![0u8; data.len() + 16];
    buf[..data.len()].copy_from_slice(data);
    enc.encrypt_padded_mut::<Pkcs7>(&mut buf, data.len()).unwrap().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::signature::Verifier;

    fn test_device() -> Device {
        let mut rng = rand::thread_rng();
        Device {
            key: RsaPrivateKey::new(&mut rng, 1024).unwrap(),
            client_id: b"fake-client-id".to_vec(),
        }
    }

    #[test]
    fn wvd_roundtrip() {
        let dev = test_device();
        let der = rsa::pkcs1::EncodeRsaPrivateKey::to_pkcs1_der(&dev.key).unwrap();
        let mut wvd = b"WVD".to_vec();
        wvd.extend_from_slice(&[2, 2, 3, 0]);
        wvd.extend_from_slice(&(der.as_bytes().len() as u16).to_be_bytes());
        wvd.extend_from_slice(der.as_bytes());
        wvd.extend_from_slice(&(dev.client_id.len() as u16).to_be_bytes());
        wvd.extend_from_slice(&dev.client_id);
        let parsed = parse_wvd(&wvd).unwrap();
        assert_eq!(parsed.client_id, dev.client_id);
        assert_eq!(parsed.key, dev.key);
    }

    #[test]
    fn pssh_payload_extracted() {
        let mut b = vec![0, 0, 0, 0];
        b.extend_from_slice(b"pssh");
        b.extend_from_slice(&[0, 0, 0, 0]);
        b.extend_from_slice(&[0xed; 16]);
        b.extend_from_slice(&3u32.to_be_bytes());
        b.extend_from_slice(b"abc");
        let total = b.len() as u32;
        b[..4].copy_from_slice(&total.to_be_bytes());
        assert_eq!(pssh_payload(&b).unwrap(), b"abc");
    }

    /// Plays the license server from pywidevine's protocol: verifies the request
    /// signature, wraps a session key to the device key, signs a License with the
    /// derived server MAC key, and checks that `parse_license` recovers the key.
    #[test]
    fn challenge_and_license_roundtrip() {
        let dev = test_device();
        let (challenge, session) = build_challenge(&dev, b"pssh-payload", 1_700_000_000, 42).unwrap();

        // Server: request must be a LICENSE_REQUEST with a valid PSS signature.
        let sm = parse_fields(&challenge).unwrap();
        assert_eq!(get_int(&sm, 1), Some(MSG_LICENSE_REQUEST));
        assert!(get_bytes(&sm, 4).is_none(), "request must not carry a session key");
        let msg = get_bytes(&sm, 2).unwrap();
        let sig = get_bytes(&sm, 3).unwrap();
        let verifier = rsa::pss::VerifyingKey::<Sha1>::new(dev.key.to_public_key());
        verifier
            .verify(msg, &rsa::pss::Signature::try_from(sig).unwrap())
            .expect("request signature verifies");
        assert_eq!(msg, session.request_msg.as_slice());

        // Server: wrap a fresh session key to the device public key (OAEP-SHA1).
        let mut rng = rand::thread_rng();
        let server_session_key = random_session_key_for_test();
        let wrapped = dev
            .key
            .to_public_key()
            .encrypt(&mut rng, Oaep::new::<Sha1>(), &server_session_key)
            .unwrap();

        let (enc_key, server_mac) = derive_keys(&server_session_key, msg).unwrap();
        let kid = [1u8; 16];
        let content_key = [2u8; 16];
        let iv = [3u8; 16];
        let wrapped_key = aes_cbc_encrypt(&enc_key, &iv, &content_key);

        let mut kc = Pb::default();
        kc.bytes(1, &kid);
        kc.bytes(2, &iv);
        kc.bytes(3, &wrapped_key);
        kc.uint(4, KEY_TYPE_CONTENT);
        let mut lic = Pb::default();
        lic.bytes(3, &kc.0);

        let core = b"core-msg";
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&server_mac).unwrap();
        Mac::update(&mut mac, core);
        Mac::update(&mut mac, &lic.0);
        let lsig = mac.finalize().into_bytes();

        let mut resp = Pb::default();
        resp.uint(1, MSG_LICENSE);
        resp.bytes(2, &lic.0);
        resp.bytes(3, &lsig);
        resp.bytes(4, &wrapped);
        resp.bytes(9, core);

        let keys = parse_license(&dev, &session, &resp.0).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].kid, kid);
        assert_eq!(keys[0].key, content_key);

        // A tampered license must be rejected.
        let mut bad = resp.0.clone();
        let idx = bad.len() - 1;
        bad[idx] ^= 1;
        assert!(parse_license(&dev, &session, &bad).is_err());
    }

    /// Known-answer test: vectors computed with pywidevine's `Cdm.derive_context`
    /// and `Cdm.derive_keys` for session key 0..16 and message b"request-message-bytes".
    #[test]
    fn key_derivation_matches_pywidevine() {
        let sk: [u8; 16] = std::array::from_fn(|i| i as u8);
        let (enc, server) = derive_keys(&sk, b"request-message-bytes").unwrap();
        assert_eq!(hex(&enc), "77a7c2edf25b841c014056315e14e572");
        assert_eq!(
            hex(&server),
            "ff945216a4bedf6df9a058cf2bfc74bda28200a3a805b400eb0abf936a86c609"
        );
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn random_session_key_for_test() -> [u8; 16] {
        let mut k = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut k);
        k
    }
}

