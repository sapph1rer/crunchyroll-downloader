//! Widevine handling.
//!
//! What's here:
//! * PSSH normalisation (`to_widevine_pssh`)
//! * the license exchange, through the Widevine CDM in `cdm.rs`
//! * CENC decryption of the downloaded track, in `cenc.rs`

use anyhow::{anyhow, bail, Result};
use std::path::Path;

/// A decrypted content key: 16-byte KID and 16-byte AES key.
#[derive(Debug, Clone)]
pub struct ContentKey {
    pub kid: [u8; 16],
    pub key: [u8; 16],
}

/// Widevine system ID, used to tag PSSH boxes.
pub const WIDEVINE_SYSTEM_ID: [u8; 16] = [
    0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce, 0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d, 0x21, 0xed,
];

/// Rewrites a PSSH box's system ID to Widevine's (Crunchyroll tags the Widevine
/// payload with the PlayReady/common system ID). The payload is untouched.
pub fn to_widevine_pssh(pssh: &[u8]) -> Result<Vec<u8>> {
    // Box layout: size(4) "pssh"(4) version/flags(4) systemID(16) ...
    if pssh.len() < 28 || &pssh[4..8] != b"pssh" {
        bail!("not a PSSH box");
    }
    let mut out = pssh.to_vec();
    if out[12..28] != WIDEVINE_SYSTEM_ID {
        out[12..28].copy_from_slice(&WIDEVINE_SYSTEM_ID);
    }
    Ok(out)
}

/// Full Widevine license exchange: build the challenge with the device key,
/// POST it, and parse the response into content keys. `normalized_pssh` is the
/// full PSSH box with the Widevine system ID already set.
pub async fn request_content_keys(
    api: &crate::api::Api,
    normalized_pssh: &[u8],
    content_id: &str,
    video_token: &str,
) -> Result<Vec<ContentKey>> {
    use rand::RngCore;
    use std::time::{SystemTime, UNIX_EPOCH};

    let dev = crate::cdm::device()?;
    let pssh_data = crate::cdm::pssh_payload(normalized_pssh)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let nonce = rand::thread_rng().next_u32().max(1);
    let (challenge, session) = crate::cdm::build_challenge(&dev, &pssh_data, now, nonce)?;
    let response = api
        .widevine_license(content_id, video_token, challenge)
        .await?;
    crate::cdm::parse_license(&dev, &session, &response)
}

/// Decrypts a downloaded track (init segment + media) to `output`. The key is chosen
/// by KID, as in Go's decryptMP4. Runs on a blocking thread. The media is streamed in
/// chunks, so memory use does not grow with the file size.
pub async fn decrypt_track(
    init: &[u8],
    encrypted: &Path,
    output: &Path,
    keys: &[ContentKey],
) -> Result<()> {
    let (init, encrypted, output, keys) = (
        init.to_vec(),
        encrypted.to_path_buf(),
        output.to_path_buf(),
        keys.to_vec(),
    );
    tokio::task::spawn_blocking(move || crate::cenc::decrypt_file(&init, &encrypted, &output, &keys))
        .await
        .map_err(|e| anyhow!("decryption task failed: {e}"))?
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pssh_system_id_rewritten() {
        let mut b = vec![0, 0, 0, 0x1c];
        b.extend_from_slice(b"pssh");
        b.extend_from_slice(&[0, 0, 0, 0]);
        b.extend_from_slice(&[0x9a; 16]);
        b.extend_from_slice(&[0, 0, 0, 0]);
        let out = to_widevine_pssh(&b).unwrap();
        assert_eq!(&out[12..28], &WIDEVINE_SYSTEM_ID);
    }
}


#[cfg(test)]
mod pssh_tests {
    use super::*;
    use crate::mpd::Mpd;

    const PLAYREADY_SYSTEM_ID: [u8; 16] = [
        0x9a, 0x04, 0xf0, 0x79, 0x98, 0x40, 0x42, 0x86, 0xab, 0x92, 0xe6, 0x5b, 0xe0, 0x88, 0x5f, 0x95,
    ];

    /// Encodes a version-0 PSSH box with the given system ID and payload.
    fn build_pssh(system_id: [u8; 16], data: &[u8]) -> Vec<u8> {
        let size = (32 + data.len()) as u32;
        let mut b = Vec::new();
        b.extend(size.to_be_bytes());
        b.extend(b"pssh");
        b.extend([0u8; 4]);
        b.extend(system_id);
        b.extend((data.len() as u32).to_be_bytes());
        b.extend(data);
        b
    }

    /// Port of Go TestToWidevinePssh.
    #[test]
    fn widevine_system_id_is_unchanged() {
        let payload = [0x08, 0x01, 0x12, 0x10, 0xaa, 0xbb, 0xcc, 0xdd];
        let original = build_pssh(WIDEVINE_SYSTEM_ID, &payload);
        assert_eq!(to_widevine_pssh(&original).unwrap(), original);
    }

    #[test]
    fn playready_system_id_is_rewritten_with_payload_kept() {
        let payload = [0x08, 0x01, 0x12, 0x10, 0xaa, 0xbb, 0xcc, 0xdd];
        let got = to_widevine_pssh(&build_pssh(PLAYREADY_SYSTEM_ID, &payload)).unwrap();
        assert_eq!(&got[12..28], &WIDEVINE_SYSTEM_ID[..], "system id");
        assert_eq!(crate::cdm::pssh_payload(&got).unwrap(), payload.to_vec(), "payload");
    }

    const WIDEVINE_URI: &str = "urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed";
    const PLAYREADY_URI: &str = "urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95";

    fn mpd(body: &str) -> Mpd {
        Mpd::parse(&format!(
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:cenc="urn:mpeg:cenc:2013"><Period>{body}</Period></MPD>"#
        ))
        .unwrap()
    }

    fn cp(uri: &str, pssh: Option<&str>) -> String {
        match pssh {
            Some(p) => format!(r#"<ContentProtection schemeIdUri="{uri}"><cenc:pssh>{p}</cenc:pssh></ContentProtection>"#),
            None => format!(r#"<ContentProtection schemeIdUri="{uri}"/>"#),
        }
    }

    /// Port of Go TestGetPsshPrefersWidevine.
    #[test]
    fn widevine_preferred_over_playready() {
        let m = mpd(&format!(
            "<AdaptationSet>{}{}</AdaptationSet>",
            cp(PLAYREADY_URI, Some("playready-pssh")),
            cp(WIDEVINE_URI, Some("widevine-pssh"))
        ));
        assert_eq!(m.pssh().as_deref(), Some("widevine-pssh"));
    }

    #[test]
    fn falls_back_when_no_widevine_scheme() {
        let m = mpd(&format!("<AdaptationSet>{}</AdaptationSet>", cp(PLAYREADY_URI, Some("playready-pssh"))));
        assert_eq!(m.pssh().as_deref(), Some("playready-pssh"));
    }

    #[test]
    fn none_when_no_pssh() {
        let m = mpd(&format!("<AdaptationSet>{}</AdaptationSet>", cp(WIDEVINE_URI, None)));
        assert_eq!(m.pssh(), None);
    }

    #[test]
    fn representation_level_widevine_pssh() {
        let m = mpd(&format!(
            "<AdaptationSet><Representation id=\"r\">{}{}</Representation></AdaptationSet>",
            cp(PLAYREADY_URI, Some("playready-pssh")),
            cp(WIDEVINE_URI, Some("widevine-pssh"))
        ));
        assert_eq!(m.pssh().as_deref(), Some("widevine-pssh"));
    }
}
