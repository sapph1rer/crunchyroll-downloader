//! Native CENC decryption for fragmented MP4 (ISO/IEC 23001-7, scheme `cenc`,
//! AES-128-CTR). This replaces the ffmpeg `-decryption_key` call and mirrors Go's
//! `decryptMP4`, which uses gowidevine/mp4ff.
//!
//! The input is the init segment followed by the media (moof/mdat fragments). The
//! output is the same file with:
//! * `sinf` removed from every sample entry and the entry renamed to its original
//!   format (`encv` -> `avc1`, `enca` -> `mp4a`), so players see a clear track;
//! * `senc`/`saiz`/`saio` removed from every `traf`, and the `trun` data offsets
//!   adjusted to match the shorter `moof`;
//! * `sidx`/`mfra` dropped, since their byte offsets would be stale;
//! * every encrypted sample decrypted in place.
//!
//! Both `cenc` (AES-CTR, full or subsample) and `cbcs` (AES-CBC with the crypt/skip
//! block pattern from `tenc`) are supported. Other schemes are rejected explicitly.

use crate::drm::{hex, ContentKey};
use aes::Aes128;
use anyhow::{anyhow, bail, Context, Result};
use ctr::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

type AesCtr = ctr::Ctr128BE<Aes128>;
type Fourcc = [u8; 4];

/// Media is streamed through this buffer, so memory does not grow with file size.
const CHUNK: usize = 1 << 20;

// ---------- box helpers ----------

fn be32(b: &[u8], at: usize) -> Result<u32> {
    let s = b.get(at..at + 4).ok_or_else(|| anyhow!("truncated box"))?;
    Ok(u32::from_be_bytes(s.try_into().unwrap()))
}

fn be16(b: &[u8], at: usize) -> Result<u16> {
    let s = b.get(at..at + 2).ok_or_else(|| anyhow!("truncated box"))?;
    Ok(u16::from_be_bytes(s.try_into().unwrap()))
}

fn kind(raw: &[u8]) -> Fourcc {
    [raw[4], raw[5], raw[6], raw[7]]
}

fn header_len(raw: &[u8]) -> usize {
    if raw.len() >= 16 && raw[..4] == [0, 0, 0, 1] {
        16
    } else {
        8
    }
}

/// Payload of a complete box (after its header).
fn body(raw: &[u8]) -> &[u8] {
    &raw[header_len(raw)..]
}

/// Splits a byte range into complete boxes, header included.
fn parse_boxes(data: &[u8]) -> Result<Vec<&[u8]>> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off < data.len() {
        if data.len() - off < 8 {
            bail!("truncated box header");
        }
        let mut size = be32(data, off)? as u64;
        let mut hl = 8u64;
        if size == 1 {
            if data.len() - off < 16 {
                bail!("truncated largesize box header");
            }
            size = u64::from_be_bytes(data[off + 8..off + 16].try_into().unwrap());
            hl = 16;
        } else if size == 0 {
            size = (data.len() - off) as u64;
        }
        if size < hl || off as u64 + size > data.len() as u64 {
            bail!("box {} overruns its container", String::from_utf8_lossy(&data[off + 4..off + 8]));
        }
        out.push(&data[off..off + size as usize]);
        off += size as usize;
    }
    Ok(out)
}

fn make_box(t: &Fourcc, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + payload.len());
    v.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    v.extend_from_slice(t);
    v.extend_from_slice(payload);
    v
}

fn find<'a>(boxes: &[&'a [u8]], t: &[u8; 4]) -> Option<&'a [u8]> {
    boxes.iter().copied().find(|b| kind(b) == *t)
}

/// Rebuilds a container box, passing each child through `f`.
fn map_children(raw: &[u8], mut f: impl FnMut(&[u8]) -> Result<Vec<u8>>) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    for child in parse_boxes(body(raw))? {
        payload.extend(f(child)?);
    }
    Ok(make_box(&kind(raw), &payload))
}

// ---------- encryption metadata from the init segment ----------

/// Per-track CENC parameters from `tenc`, plus the default sample size from `trex`.
#[derive(Debug, Clone)]
struct TrackCrypt {
    kid: [u8; 16],
    /// `cbcs` (AES-CBC with a block pattern) instead of `cenc`.
    cbcs: bool,
    /// (crypt, skip) block counts for `cbcs`. (0, 0) means the whole range is encrypted.
    pattern: (u8, u8),
    /// Per-sample IV length (8 or 16). 0 means every sample uses `constant_iv`.
    iv_size: usize,
    constant_iv: Option<Vec<u8>>,
    default_size: Option<u32>,
}

/// Parses a `tenc` payload. Returns None for an unprotected track. The result is
/// (iv_size, constant IV, KID, pattern), where pattern is (crypt, skip) from byte 5.
type TencInfo = (usize, Option<Vec<u8>>, [u8; 16], (u8, u8));

fn parse_tenc(b: &[u8]) -> Result<Option<TencInfo>> {
    // ver/flags(4) reserved(1) pattern(1) isProtected(1) ivSize(1) KID(16)
    let pattern_byte = *b.get(5).ok_or_else(|| anyhow!("truncated tenc"))?;
    let is_protected = *b.get(6).ok_or_else(|| anyhow!("truncated tenc"))? == 1;
    let iv_size = *b.get(7).ok_or_else(|| anyhow!("truncated tenc"))? as usize;
    let kid: [u8; 16] = b.get(8..24).ok_or_else(|| anyhow!("truncated tenc"))?.try_into().unwrap();
    let constant = if is_protected && iv_size == 0 {
        let n = *b.get(24).ok_or_else(|| anyhow!("truncated tenc constant IV"))? as usize;
        Some(b.get(25..25 + n).ok_or_else(|| anyhow!("truncated tenc constant IV"))?.to_vec())
    } else {
        None
    };
    // Version 0 has no pattern; those bytes are reserved and read as zero.
    let pattern = (pattern_byte >> 4, pattern_byte & 0x0f);
    Ok(if is_protected { Some((iv_size, constant, kid, pattern)) } else { None })
}

fn trak_handler(trak: &[u8]) -> Result<Fourcc> {
    let kids = parse_boxes(body(trak))?;
    let mdia = find(&kids, b"mdia").ok_or_else(|| anyhow!("trak without mdia"))?;
    let mdia_kids = parse_boxes(body(mdia))?;
    let hdlr = find(&mdia_kids, b"hdlr").ok_or_else(|| anyhow!("mdia without hdlr"))?;
    let h = body(hdlr);
    Ok([h[8], h[9], h[10], h[11]])
}

fn trak_track_id(trak: &[u8]) -> Result<u32> {
    let kids = parse_boxes(body(trak))?;
    let tkhd = find(&kids, b"tkhd").ok_or_else(|| anyhow!("trak without tkhd"))?;
    let b = body(tkhd);
    // version 1 has 64-bit times, so the track ID sits 8 bytes further in.
    if b[0] == 1 {
        be32(b, 20)
    } else {
        be32(b, 12)
    }
}

/// Finds the CENC info inside a track's sample description, if the track is protected.
/// The result is (cbcs, iv_size, constant IV, KID, pattern).
type SampleCrypt = (bool, usize, Option<Vec<u8>>, [u8; 16], (u8, u8));

fn sample_entry_crypt(trak: &[u8], handler: Fourcc) -> Result<Option<SampleCrypt>> {
    let kids = parse_boxes(body(trak))?;
    let mdia = find(&kids, b"mdia").ok_or_else(|| anyhow!("trak without mdia"))?;
    let minf = find(&parse_boxes(body(mdia))?, b"minf").ok_or_else(|| anyhow!("mdia without minf"))?;
    let stbl = find(&parse_boxes(body(minf))?, b"stbl").ok_or_else(|| anyhow!("minf without stbl"))?;
    let stsd = find(&parse_boxes(body(stbl))?, b"stsd").ok_or_else(|| anyhow!("stbl without stsd"))?;
    let entries = &body(stsd)[8..];
    for e in parse_boxes(entries)? {
        let Some(sinf) = sinf_of_entry(e, handler)? else { continue };
        let sinf_kids = parse_boxes(body(sinf))?;
        let schm = find(&sinf_kids, b"schm").ok_or_else(|| anyhow!("sinf without schm"))?;
        let scheme = &body(schm)[4..8];
        let cbcs = match scheme {
            b"cenc" => false,
            b"cbcs" => true,
            _ => bail!(
                "unsupported CENC scheme {:?} (only cenc and cbcs are implemented)",
                String::from_utf8_lossy(scheme)
            ),
        };
        let schi = find(&sinf_kids, b"schi").ok_or_else(|| anyhow!("sinf without schi"))?;
        let tenc = find(&parse_boxes(body(schi))?, b"tenc").ok_or_else(|| anyhow!("schi without tenc"))?;
        if let Some((iv_size, constant, kid, pattern)) = parse_tenc(body(tenc))? {
            return Ok(Some((cbcs, iv_size, constant, kid, pattern)));
        }
    }
    Ok(None)
}

/// Header bytes before a sample entry's child boxes. Visual and audio entries differ.
fn entry_header_len(handler: Fourcc) -> Result<usize> {
    match &handler {
        b"vide" => Ok(78),
        b"soun" => Ok(28),
        other => bail!("unsupported protected track type {:?}", String::from_utf8_lossy(other)),
    }
}

/// The sinf box of an encrypted sample entry, or None for other entries.
fn sinf_of_entry(entry: &[u8], handler: Fourcc) -> Result<Option<&[u8]>> {
    if !matches!(&kind(entry), b"encv" | b"enca") {
        return Ok(None);
    }
    let hl = entry_header_len(handler)?;
    let children = parse_boxes(&body(entry)[hl..])?;
    Ok(find(&children, b"sinf"))
}

/// Result of matching the init segment against the license keys.
#[derive(Debug)]
struct Plan {
    tracks: HashMap<u32, (TrackCrypt, [u8; 16])>,
}

fn build_plan(init: &[u8], keys: &[ContentKey]) -> Result<Plan> {
    let top = parse_boxes(init)?;
    let moov = find(&top, b"moov").ok_or_else(|| anyhow!("MP4 has no initialization segment"))?;
    let moov_kids = parse_boxes(body(moov))?;

    // trex default sample sizes, per track.
    let mut trex_size: HashMap<u32, u32> = HashMap::new();
    if let Some(mvex) = find(&moov_kids, b"mvex") {
        for trex in parse_boxes(body(mvex))?.into_iter().filter(|b| kind(b) == *b"trex") {
            let b = body(trex);
            trex_size.insert(be32(b, 4)?, be32(b, 16)?);
        }
    }

    let mut encrypted: Vec<(u32, TrackCrypt)> = Vec::new();
    for trak in moov_kids.iter().copied().filter(|b| kind(b) == *b"trak") {
        let handler = trak_handler(trak)?;
        let track_id = trak_track_id(trak)?;
        if let Some((cbcs, iv_size, constant_iv, kid, pattern)) = sample_entry_crypt(trak, handler)? {
            encrypted.push((
                track_id,
                TrackCrypt {
                    kid,
                    cbcs,
                    pattern,
                    iv_size,
                    constant_iv,
                    default_size: trex_size.get(&track_id).copied(),
                },
            ));
        }
    }
    if encrypted.is_empty() {
        bail!("MP4 has no encrypted tracks");
    }

    let mut tracks = HashMap::new();
    let mut unmatched = Vec::new();
    for (track_id, tc) in encrypted {
        match keys.iter().find(|k| k.kid == tc.kid) {
            Some(k) => {
                tracks.insert(track_id, (tc, k.key));
            }
            None => unmatched.push(hex(&tc.kid)),
        }
    }
    if !unmatched.is_empty() {
        bail!("no license key found for MP4 KID(s) {}", unmatched.join(", "));
    }
    Ok(Plan { tracks })
}

// ---------- moov rewrite: sample entries back to clear ----------

fn rewrite_moov(raw: &[u8]) -> Result<Vec<u8>> {
    map_children(raw, |child| {
        if kind(child) == *b"trak" {
            let handler = trak_handler(child)?;
            map_children(child, |c| {
                if kind(c) != *b"mdia" {
                    return Ok(c.to_vec());
                }
                map_children(c, |m| {
                    if kind(m) != *b"minf" {
                        return Ok(m.to_vec());
                    }
                    map_children(m, |s| {
                        if kind(s) != *b"stbl" {
                            return Ok(s.to_vec());
                        }
                        map_children(s, |st| {
                            if kind(st) != *b"stsd" {
                                return Ok(st.to_vec());
                            }
                            rewrite_stsd(st, handler)
                        })
                    })
                })
            })
        } else {
            Ok(child.to_vec())
        }
    })
}

fn rewrite_stsd(raw: &[u8], handler: Fourcc) -> Result<Vec<u8>> {
    let b = body(raw);
    let mut payload = b[..8].to_vec(); // version/flags + entry_count
    for e in parse_boxes(&b[8..])? {
        if matches!(&kind(e), b"encv" | b"enca") {
            payload.extend(clear_entry(e, handler)?);
        } else {
            payload.extend_from_slice(e);
        }
    }
    Ok(make_box(b"stsd", &payload))
}

/// Restores a protected sample entry: keeps its header and other children, drops
/// sinf, and uses the original format from frma as the box type.
fn clear_entry(e: &[u8], handler: Fourcc) -> Result<Vec<u8>> {
    let hl = entry_header_len(handler)?;
    let eb = body(e);
    let children = parse_boxes(&eb[hl..])?;
    let sinf = find(&children, b"sinf").ok_or_else(|| anyhow!("encrypted sample entry without sinf"))?;
    let frma = find(&parse_boxes(body(sinf))?, b"frma").ok_or_else(|| anyhow!("sinf without frma"))?;
    let format: Fourcc = body(frma)[..4].try_into().context("frma too short")?;

    let mut payload = eb[..hl].to_vec();
    for c in children {
        if kind(c) != *b"sinf" {
            payload.extend_from_slice(c);
        }
    }
    Ok(make_box(&format, &payload))
}

// ---------- sample encryption info ----------

/// Per-sample auxiliary info: IV plus (clear, encrypted) subsample pairs.
#[derive(Debug, Clone, PartialEq)]
struct Aux {
    iv: Vec<u8>,
    subs: Vec<(u64, u64)>,
}

/// Reads one sample's IV and optional subsample map from `b` at `*pos`.
fn read_aux(b: &[u8], pos: &mut usize, iv_size: usize, constant: Option<&[u8]>, subs_present: bool) -> Result<Aux> {
    let iv = if iv_size > 0 {
        let v = b.get(*pos..*pos + iv_size).ok_or_else(|| anyhow!("truncated sample IV"))?.to_vec();
        *pos += iv_size;
        v
    } else {
        constant.ok_or_else(|| anyhow!("no per-sample IV and no constant IV"))?.to_vec()
    };
    let mut subs = Vec::new();
    if subs_present {
        let n = be16(b, *pos)? as usize;
        *pos += 2;
        for _ in 0..n {
            let clear = be16(b, *pos)? as u64;
            let enc = be32(b, *pos + 2)? as u64;
            subs.push((clear, enc));
            *pos += 6;
        }
    }
    Ok(Aux { iv, subs })
}

fn parse_senc(b: &[u8], iv_size: usize, constant: Option<&[u8]>) -> Result<Vec<Aux>> {
    let flags = u32::from_be_bytes([0, b[1], b[2], b[3]]);
    let count = be32(b, 4)? as usize;
    let mut pos = 8;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(read_aux(b, &mut pos, iv_size, constant, flags & 0x2 != 0)?);
    }
    Ok(out)
}

/// Sample aux info from saiz/saio, which lives in the moof at the saio offset.
fn parse_saiz_saio(moof: &[u8], saiz: &[u8], saio: &[u8], iv_size: usize, constant: Option<&[u8]>) -> Result<Vec<Aux>> {
    let iz = body(saiz);
    let mut p = 4;
    if iz[3] & 1 != 0 {
        p += 8;
    }
    let default = *iz.get(p).ok_or_else(|| anyhow!("truncated saiz"))? as usize;
    let count = be32(iz, p + 1)? as usize;
    let mut sizes = Vec::with_capacity(count);
    for i in 0..count {
        sizes.push(if default != 0 { default } else { *iz.get(p + 5 + i).unwrap_or(&0) as usize });
    }

    let io = body(saio);
    let mut q = 4;
    if io[3] & 1 != 0 {
        q += 8;
    }
    let entries = be32(io, q)?;
    if entries == 0 {
        bail!("saio has no offsets");
    }
    let offset = if io[0] == 0 {
        be32(io, q + 4)? as usize
    } else {
        u64::from_be_bytes(io.get(q + 4..q + 12).ok_or_else(|| anyhow!("truncated saio"))?.try_into().unwrap()) as usize
    };

    let mut out = Vec::with_capacity(count);
    let mut pos = offset;
    for size in sizes {
        let end = pos + size;
        let slice = moof.get(pos..end).ok_or_else(|| anyhow!("sample aux outside moof"))?;
        let mut sp = 0;
        let subs_present = size > iv_size;
        out.push(read_aux(slice, &mut sp, iv_size, constant, subs_present)?);
        pos = end;
    }
    Ok(out)
}

/// Counter block for AES-CTR: an 8-byte IV is padded with zeros, a 16-byte IV is used as is.
fn counter_block(iv: &[u8]) -> Result<[u8; 16]> {
    let mut block = [0u8; 16];
    match iv.len() {
        8 => block[..8].copy_from_slice(iv),
        16 => block.copy_from_slice(iv),
        n => bail!("unsupported IV length {n}"),
    }
    Ok(block)
}

/// One run of encrypted bytes in the input file. `ks_off` is where its keystream starts.
#[derive(Debug, Clone)]
struct Span {
    start: u64,
    len: u64,
    key: [u8; 16],
    /// Counter block for CTR, or the 16-byte IV for CBC.
    block: [u8; 16],
    ks_off: u64,
    /// Some((crypt, skip)) for a cbcs span, which is decrypted as one CBC chain.
    pattern: Option<(u8, u8)>,
}

/// Encrypted spans of one sample at `start` with `size` bytes.
fn sample_spans(start: u64, size: u64, aux: &Aux, key: [u8; 16], block: [u8; 16]) -> Result<Vec<Span>> {
    if aux.subs.is_empty() {
        return Ok(vec![Span { start, len: size, key, block, ks_off: 0, pattern: None }]);
    }
    let mut out = Vec::new();
    let mut p = start;
    let mut ks = 0u64;
    for &(clear, enc) in &aux.subs {
        p += clear;
        if enc > 0 {
            out.push(Span { start: p, len: enc, key, block, ks_off: ks, pattern: None });
        }
        ks += enc;
        p += enc;
    }
    if p != start + size {
        bail!("subsample sizes add up to {} bytes, sample is {size}", p - start);
    }
    Ok(out)
}

// ---------- moof rewrite ----------

/// Rebuilds a moof without its encryption boxes and returns it with the encrypted spans
/// of the samples that follow it. `moof_abs` is the moof's position in the input.
fn process_moof(moof: &[u8], moof_abs: u64, plan: &Plan) -> Result<(Vec<u8>, Vec<Span>)> {
    let kids = parse_boxes(body(moof))?;

    // Bytes that will leave the moof. Every data offset is relative to the moof start, so
    // all trun offsets move back by this amount.
    let mut removed = 0usize;
    for traf in kids.iter().copied().filter(|b| kind(b) == *b"traf") {
        for c in parse_boxes(body(traf))? {
            if matches!(&kind(c), b"senc" | b"saiz" | b"saio") {
                removed += c.len();
            }
        }
    }

    let mut spans = Vec::new();
    let mut payload = Vec::with_capacity(moof.len());
    for k in kids {
        if kind(k) != *b"traf" {
            payload.extend_from_slice(k);
            continue;
        }
        let traf_kids = parse_boxes(body(k))?;
        let tfhd = find(&traf_kids, b"tfhd").ok_or_else(|| anyhow!("traf without tfhd"))?;
        let tb = body(tfhd);
        let tflags = u32::from_be_bytes([0, tb[1], tb[2], tb[3]]);
        if tflags & 0x1 != 0 {
            bail!("tfhd base-data-offset is not supported");
        }
        let track_id = be32(tb, 4)?;
        let mut p = 8;
        if tflags & 0x2 != 0 {
            p += 4;
        }
        if tflags & 0x8 != 0 {
            p += 4;
        }
        let tfhd_default = if tflags & 0x10 != 0 { Some(be32(tb, p)?) } else { None };

        let encrypted = plan.tracks.get(&track_id);

        // Sample positions and sizes, in input coordinates.
        let mut samples: Vec<(u64, u64)> = Vec::new();
        let mut cursor = moof_abs as i64;
        for t in traf_kids.iter().copied().filter(|b| kind(b) == *b"trun") {
            let tb = body(t);
            let flags = u32::from_be_bytes([0, tb[1], tb[2], tb[3]]);
            let count = be32(tb, 4)? as usize;
            let mut q = 8;
            let data_offset = if flags & 0x1 != 0 {
                let v = be32(tb, q)? as i32;
                q += 4;
                Some(v)
            } else {
                None
            };
            if let Some(v) = data_offset {
                cursor = moof_abs as i64 + v as i64;
            }
            if flags & 0x4 != 0 {
                q += 4;
            }
            for _ in 0..count {
                if flags & 0x100 != 0 {
                    q += 4;
                }
                let size = if flags & 0x200 != 0 {
                    let s = be32(tb, q)? as u64;
                    q += 4;
                    s
                } else {
                    tfhd_default.map(u64::from).or(encrypted.and_then(|(tc, _)| tc.default_size).map(u64::from)).ok_or_else(|| anyhow!("no sample size for track {track_id}"))?
                };
                if flags & 0x400 != 0 {
                    q += 4;
                }
                if flags & 0x800 != 0 {
                    q += 4;
                }
                samples.push((cursor as u64, size));
                cursor += size as i64;
            }
        }

        if let Some((tc, key)) = encrypted {
            if !samples.is_empty() {
                let auxes = if let Some(senc) = find(&traf_kids, b"senc") {
                    parse_senc(body(senc), tc.iv_size, tc.constant_iv.as_deref())?
                } else if let (Some(saiz), Some(saio)) = (find(&traf_kids, b"saiz"), find(&traf_kids, b"saio")) {
                    parse_saiz_saio(moof, saiz, saio, tc.iv_size, tc.constant_iv.as_deref())?
                } else {
                    bail!("track {track_id} has encrypted samples but no senc or saiz/saio");
                };
                if auxes.len() != samples.len() {
                    bail!("track {track_id}: {} aux entries for {} samples", auxes.len(), samples.len());
                }
                for ((start, size), aux) in samples.iter().zip(&auxes) {
                    let block = if tc.cbcs {
                        <[u8; 16]>::try_from(aux.iv.as_slice())
                            .map_err(|_| anyhow!("track {track_id}: cbcs needs a 16-byte IV, got {}", aux.iv.len()))?
                    } else {
                        counter_block(&aux.iv)?
                    };
                    let pattern = tc.cbcs.then_some(tc.pattern);
                    for mut span in sample_spans(*start, *size, aux, *key, block)? {
                        span.pattern = pattern;
                        spans.push(span);
                    }
                }
            }
        }

        // Rebuild the traf: patch trun offsets, drop encryption boxes.
        let mut traf_payload = Vec::new();
        for c in traf_kids {
            match &kind(c) {
                b"senc" | b"saiz" | b"saio" => {}
                b"trun" => {
                    let mut t = c.to_vec();
                    let hl = header_len(&t);
                    let flags = u32::from_be_bytes([0, t[hl + 1], t[hl + 2], t[hl + 3]]);
                    if flags & 0x1 != 0 {
                        let at = hl + 8;
                        let old = i32::from_be_bytes(t[at..at + 4].try_into().unwrap());
                        let new = old - removed as i32;
                        t[at..at + 4].copy_from_slice(&new.to_be_bytes());
                    }
                    traf_payload.extend(t);
                }
                _ => traf_payload.extend_from_slice(c),
            }
        }
        payload.extend(make_box(b"traf", &traf_payload));
    }
    Ok((make_box(b"moof", &payload), spans))
}

/// Block indices (16-byte units) that a cbcs pattern encrypts, in order. With the
/// pattern (0, 0) every full block is encrypted. A trailing partial block stays clear.
fn cbc_indices(blocks: usize, crypt: u8, skip: u8) -> Vec<usize> {
    if crypt == 0 && skip == 0 {
        return (0..blocks).collect();
    }
    let mut out = Vec::new();
    let mut b = 0usize;
    while b < blocks {
        let e = (b + crypt as usize).min(blocks);
        out.extend(b..e);
        b = e + skip as usize;
    }
    out
}

/// Decrypts one cbcs span in place. `data` must be exactly the span's bytes. The CBC
/// chain starts from the span's IV and runs through the encrypted blocks only.
fn decrypt_cbc_span(s: &Span, data: &mut [u8]) -> Result<()> {
    use aes::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};
    let (crypt, skip) = s.pattern.ok_or_else(|| anyhow!("internal error: not a cbcs span"))?;
    let idx = cbc_indices(data.len() / 16, crypt, skip);
    if idx.is_empty() {
        return Ok(());
    }
    let mut gathered: Vec<u8> = idx.iter().flat_map(|&i| data[i * 16..i * 16 + 16].iter().copied()).collect();
    let dec = cbc::Decryptor::<Aes128>::new_from_slices(&s.key, &s.block).map_err(|e| anyhow!("{e}"))?;
    dec.decrypt_padded_mut::<NoPadding>(&mut gathered).map_err(|e| anyhow!("cbc decrypt failed: {e:?}"))?;
    for (k, &i) in idx.iter().enumerate() {
        data[i * 16..i * 16 + 16].copy_from_slice(&gathered[k * 16..k * 16 + 16]);
    }
    Ok(())
}

// ---------- media streaming ----------

/// Applies the spans that overlap `buf`, which holds the input bytes at `pos`.
/// `si` tracks the first span that can still overlap, so spans are visited once.
fn apply_spans(spans: &[Span], pos: u64, buf: &mut [u8], si: &mut usize) -> Result<()> {
    let chunk_end = pos + buf.len() as u64;
    while *si < spans.len() && spans[*si].start + spans[*si].len <= pos {
        *si += 1;
    }
    let mut j = *si;
    while j < spans.len() && spans[j].start < chunk_end {
        let s = &spans[j];
        if s.pattern.is_some() {
            // cbcs spans are kept whole inside one chunk by stream_mdat.
            if s.start < pos || s.start + s.len > chunk_end {
                bail!("internal error: cbcs span split across chunks");
            }
            let range = (s.start - pos) as usize..(s.start + s.len - pos) as usize;
            decrypt_cbc_span(s, &mut buf[range])?;
            j += 1;
            continue;
        }
        let from = s.start.max(pos);
        let to = (s.start + s.len).min(chunk_end);
        if from < to {
            let mut cipher = AesCtr::new_from_slices(&s.key, &s.block).map_err(|e| anyhow!("{e}"))?;
            cipher.seek(s.ks_off + (from - s.start));
            cipher.apply_keystream(&mut buf[(from - pos) as usize..(to - pos) as usize]);
        }
        j += 1;
    }
    Ok(())
}

/// Copies an mdat payload from `reader` to `out`, decrypting the pending spans.
fn stream_mdat<R: Read, W: Write>(
    reader: &mut R,
    out: &mut W,
    start: u64,
    end: u64,
    pending: &mut Vec<Span>,
) -> Result<()> {
    pending.sort_by_key(|s| s.start);
    for s in pending.iter() {
        if s.start < start || s.start + s.len > end {
            bail!("encrypted sample data lies outside the mdat that follows its moof");
        }
    }
    let mut buf = vec![0u8; CHUNK];
    let mut pos = start;
    let mut si = 0usize;
    while pos < end {
        let mut chunk_end = (pos + CHUNK as u64).min(end);
        // A cbcs span is one CBC chain, so the chunk must not end inside it. Extend the
        // chunk to the span's end, repeating in case that brings in another span.
        loop {
            let mut grew = false;
            for s in pending.iter().filter(|s| s.pattern.is_some()) {
                if s.start >= pos && s.start < chunk_end && s.start + s.len > chunk_end {
                    chunk_end = (s.start + s.len).min(end);
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        let n = (chunk_end - pos) as usize;
        if buf.len() < n {
            buf.resize(n, 0);
        }
        reader.read_exact(&mut buf[..n])?;
        apply_spans(pending, pos, &mut buf[..n], &mut si)?;
        out.write_all(&buf[..n])?;
        pos = chunk_end;
    }
    pending.clear();
    Ok(())
}

/// Decrypts `input` (init segment followed by media, as written by the downloader) into
/// `output`. `init` is the init segment on its own. It selects the keys, by KID.
pub fn decrypt_file(init: &[u8], input: &Path, output: &Path, keys: &[ContentKey]) -> Result<()> {
    let plan = build_plan(init, keys)?;
    let mut reader = BufReader::new(File::open(input).with_context(|| format!("opening {}", input.display()))?);
    let file_len = reader.get_ref().metadata()?.len();
    let mut out = BufWriter::new(File::create(output).with_context(|| format!("creating {}", output.display()))?);

    let mut pos = 0u64;
    let mut pending: Vec<Span> = Vec::new();
    while pos < file_len {
        if file_len - pos < 8 {
            bail!("truncated box header at offset {pos}");
        }
        let mut head: Vec<u8> = vec![0u8; 8];
        reader.read_exact(&mut head)?;
        let mut size = u32::from_be_bytes(head[..4].try_into().unwrap()) as u64;
        let mut hl = 8u64;
        if size == 1 {
            if file_len - pos < 16 {
                bail!("truncated largesize box at offset {pos}");
            }
            let mut ext = [0u8; 8];
            reader.read_exact(&mut ext)?;
            head.extend_from_slice(&ext);
            size = u64::from_be_bytes(ext);
            hl = 16;
        } else if size == 0 {
            size = file_len - pos;
        }
        if size < hl || pos + size > file_len {
            bail!("box at offset {pos} overruns the file");
        }
        let t: Fourcc = head[4..8].try_into().unwrap();

        // The rest of the box. mdat payloads are streamed instead of read whole.
        let mut rest = Vec::new();
        if &t != b"mdat" {
            rest = vec![0u8; (size - hl) as usize];
            reader.read_exact(&mut rest)?;
        }

        match &t {
            b"moov" => {
                let mut raw = head;
                raw.extend(rest);
                out.write_all(&rewrite_moov(&raw)?)?;
            }
            b"moof" => {
                let mut raw = head;
                raw.extend(rest);
                let (new_moof, spans) = process_moof(&raw, pos, &plan)?;
                out.write_all(&new_moof)?;
                pending.extend(spans);
            }
            b"mdat" => {
                out.write_all(&head)?;
                stream_mdat(&mut reader, &mut out, pos + hl, pos + size, &mut pending)?;
            }
            b"sidx" | b"mfra" => {} // byte offsets would be stale after the rewrite
            _ => {
                let mut raw = head;
                raw.extend(rest);
                out.write_all(&raw)?;
            }
        }
        pos += size;
    }
    if !pending.is_empty() {
        bail!("encrypted samples found without a following mdat");
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 16] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
    const KID: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

    /// Fixture made by tests/fixtures/make_cenc_fixture.py, which encrypts a plain
    /// ffmpeg fragmented MP4 independently of this code. ffmpeg's own -decryption_key
    /// reproduces the plaintext mdat bytes exactly.
    const FIX_INIT: &[u8] = include_bytes!("../tests/fixtures/cenc/init.mp4");
    const FIX_ENC: &[u8] = include_bytes!("../tests/fixtures/cenc/encrypted.mp4");
    const FIX_PLAIN: &[u8] = include_bytes!("../tests/fixtures/cenc/plain_mdat.bin");

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("crdl-cenc-{}-{name}", uuid::Uuid::new_v4()))
    }

    /// Concatenated payloads of every top-level mdat in an MP4 byte string.
    fn mdat_payloads(file: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for b in parse_boxes(file).unwrap() {
            if kind(b) == *b"mdat" {
                out.extend_from_slice(body(b));
            }
        }
        out
    }

    #[test]
    fn fixture_decrypts_to_plaintext() {
        let enc = scratch("enc.mp4");
        let out = scratch("out.mp4");
        std::fs::write(&enc, FIX_ENC).unwrap();
        decrypt_file(FIX_INIT, &enc, &out, &[ContentKey { kid: KID, key: KEY }]).unwrap();

        let produced = std::fs::read(&out).unwrap();
        assert_eq!(mdat_payloads(&produced), FIX_PLAIN, "decrypted media differs from plaintext");

        // The output has no encryption boxes and the sample entries are clear again.
        let top = parse_boxes(&produced).unwrap();
        let moov = find(&top, b"moov").unwrap();
        let text = String::from_utf8_lossy(moov);
        assert!(!text.contains("sinf") && !text.contains("encv") && !text.contains("enca"));
        assert!(text.contains("avc1") && text.contains("mp4a"));
        for m in top.iter().filter(|b| kind(b) == *b"moof") {
            assert!(!String::from_utf8_lossy(m).contains("senc"));
        }
        let _ = std::fs::remove_file(&enc);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn unmatched_kid_is_reported() {
        let enc = scratch("enc2.mp4");
        let out = scratch("out2.mp4");
        std::fs::write(&enc, FIX_ENC).unwrap();
        let err = decrypt_file(FIX_INIT, &enc, &out, &[ContentKey { kid: [9; 16], key: KEY }]).unwrap_err();
        assert!(err.to_string().contains("no license key found for MP4 KID(s)"), "{err}");
        let _ = std::fs::remove_file(&enc);
    }

    #[test]
    fn clear_init_has_no_encrypted_tracks() {
        // The plaintext init from the fixture (ftyp + moov) with its sample entries
        // restored has nothing to decrypt.
        let cleared = rewrite_moov(&parse_boxes(FIX_INIT).unwrap().iter().copied().find(|b| kind(b) == *b"moov").unwrap().to_vec()).unwrap();
        let mut init = make_box(b"ftyp", &[]);
        init.extend(cleared);
        let err = build_plan(&init, &[ContentKey { kid: KID, key: KEY }]).unwrap_err();
        assert!(err.to_string().contains("no encrypted tracks"), "{err}");
    }

    #[test]
    fn subsample_spans_skip_clear_bytes() {
        let aux = Aux { iv: vec![0; 8], subs: vec![(4, 10), (2, 6)] };
        let spans = sample_spans(100, 22, &aux, KEY, counter_block(&aux.iv).unwrap()).unwrap();
        assert_eq!(spans.iter().map(|s| (s.start, s.len, s.ks_off)).collect::<Vec<_>>(), vec![(104, 10, 0), (116, 6, 10)]);
    }

    #[test]
    fn subsample_sizes_must_match_sample() {
        let aux = Aux { iv: vec![0; 8], subs: vec![(4, 10)] };
        assert!(sample_spans(0, 20, &aux, KEY, [0; 16]).is_err());
    }

    /// Decryption across chunk boundaries equals decryption of the whole buffer.
    #[test]
    fn spans_applied_across_chunk_boundaries() {
        let iv = [7u8; 8];
        let aux = Aux { iv: iv.to_vec(), subs: vec![(3, 40), (5, 25)] };
        let size = 3 + 40 + 5 + 25;
        let block = counter_block(&iv).unwrap();
        let spans = sample_spans(0, size, &aux, KEY, block).unwrap();

        let plain: Vec<u8> = (0..size as usize).map(|i| (i * 31 % 251) as u8).collect();
        let mut whole = plain.clone();
        let mut si = 0;
        apply_spans(&spans, 0, &mut whole, &mut si).unwrap();

        for chunk in [1usize, 5, 7, 16] {
            let mut split = plain.clone();
            let mut si = 0;
            let mut pos = 0usize;
            while pos < split.len() {
                let end = (pos + chunk).min(split.len());
                apply_spans(&spans, pos as u64, &mut split[pos..end], &mut si).unwrap();
                pos = end;
            }
            assert_eq!(split, whole, "chunk size {chunk}");
        }
        assert_ne!(whole, plain, "encryption changed nothing");
    }

    #[test]
    fn counter_block_layout() {
        assert_eq!(counter_block(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap(), [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(counter_block(&[9; 16]).unwrap(), [9; 16]);
        assert!(counter_block(&[0; 4]).is_err());
    }

    #[test]
    fn truncated_box_is_an_error() {
        assert!(parse_boxes(&[0, 0, 0, 40, b'm', b'o', b'o', b'v']).is_err());
    }

    /// NIST SP 800-38A F.2.1 (AES-128 CBC decrypt), four blocks, fully encrypted.
    #[test]
    fn cbcs_matches_nist_cbc_vector() {
        let key = hexarr("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = hexarr("000102030405060708090a0b0c0d0e0f");
        let cipher = "7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b273bed6b8e3c1743b7116e69e22229516\
                      3ff1caa1681fac09120eca307586e1a7";
        let plain = "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e5130c81c46a35ce411e5fbc1191a0a52eff69f2445df4f9b17ad2b417be66c3710";
        let mut data = hexvec(cipher);
        let span = Span { start: 0, len: data.len() as u64, key, block: iv, ks_off: 0, pattern: Some((0, 0)) };
        decrypt_cbc_span(&span, &mut data).unwrap();
        assert_eq!(data, hexvec(plain));
    }

    /// Pattern 1:1 over four blocks. Encrypted blocks 0 and 2 form one CBC chain, so
    /// block 2 chains from ciphertext block 0. Clear blocks 1 and 3 must be untouched.
    #[test]
    fn cbcs_pattern_chains_through_encrypted_blocks_only() {
        let key = hexarr("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = hexarr("000102030405060708090a0b0c0d0e0f");
        let c0 = "7649abac8119b246cee98e9b12e9197d";
        let c1 = "5086cb9b507219ee95db113a917678b2";
        let x = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let y = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let mut data = hexvec(&format!("{c0}{x}{c1}{y}"));
        let span = Span { start: 0, len: 64, key, block: iv, ks_off: 0, pattern: Some((1, 1)) };
        decrypt_cbc_span(&span, &mut data).unwrap();
        // P0 and P2 (the NIST plaintext blocks 0 and 1) with the clear blocks kept.
        let expect = format!("{}{x}{}{y}", "6bc1bee22e409f96e93d7e117393172a", "ae2d8a571e03ac9c9eb76fac45af8e51");
        assert_eq!(data, hexvec(&expect));
    }

    /// A trailing partial block is never encrypted under cbcs.
    #[test]
    fn cbcs_partial_tail_stays_clear() {
        let key = hexarr("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = hexarr("000102030405060708090a0b0c0d0e0f");
        let mut data = hexvec("7649abac8119b246cee98e9b12e9197d");
        data.extend_from_slice(b"tail!");
        let span = Span { start: 0, len: 21, key, block: iv, ks_off: 0, pattern: Some((1, 0)) };
        decrypt_cbc_span(&span, &mut data).unwrap();
        assert_eq!(&data[16..], b"tail!");
        assert_eq!(&data[..16], hexvec("6bc1bee22e409f96e93d7e117393172a").as_slice());
    }

    #[test]
    fn cbc_indices_follow_the_pattern() {
        assert_eq!(cbc_indices(4, 1, 1), vec![0, 2]);
        assert_eq!(cbc_indices(5, 2, 1), vec![0, 1, 3, 4]);
        assert_eq!(cbc_indices(3, 0, 0), vec![0, 1, 2]);
    }

    /// cbcs chunked across mdat reads gives the same result as one buffer.
    #[test]
    fn cbcs_span_is_not_split_across_chunks() {
        let key = [9u8; 16];
        let iv = [3u8; 16];
        let len = CHUNK as u64 + 4096;
        let plain: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let mut encrypted = plain.clone();
        let whole = Span { start: 0, len, key, block: iv, ks_off: 0, pattern: Some((1, 9)) };
        // Encrypt with the same CBC chain, then check the streaming path decrypts it back.
        {
            use aes::cipher::{block_padding::NoPadding, BlockEncryptMut, KeyIvInit};
            let idx = cbc_indices((len / 16) as usize, 1, 9);
            let mut gathered: Vec<u8> = idx.iter().flat_map(|&i| plain[i * 16..i * 16 + 16].iter().copied()).collect();
            let n = gathered.len();
            cbc::Encryptor::<Aes128>::new_from_slices(&key, &iv)
                .unwrap()
                .encrypt_padded_mut::<NoPadding>(&mut gathered, n)
                .unwrap();
            for (k, &i) in idx.iter().enumerate() {
                encrypted[i * 16..i * 16 + 16].copy_from_slice(&gathered[k * 16..k * 16 + 16]);
            }
        }
        let mut pending = vec![whole];
        let mut out = Vec::new();
        stream_mdat(&mut encrypted.as_slice(), &mut out, 0, len, &mut pending).unwrap();
        assert_eq!(out, plain);
    }

    fn hexvec(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn hexarr(s: &str) -> [u8; 16] {
        hexvec(s).try_into().unwrap()
    }
}
