#!/usr/bin/env python3
"""Builds the CENC (ISO/IEC 23001-7, scheme 'cenc', AES-128-CTR) test fixture used by
src/cenc.rs. It is written independently of the Rust decryptor, from the spec:

  1. make a plain fragmented MP4 with ffmpeg (video H.264 + audio AAC);
  2. turn sample entries into encv/enca with sinf -> frma/schm/schi -> tenc;
  3. add a senc box (8-byte IVs, subsamples for video) to every traf;
  4. encrypt every sample in mdat (AES-CTR, counter block = IV || 0^8, keystream
     advanced only over encrypted bytes) and shift trun data offsets for the
     added senc bytes;
  5. drop sidx/mfra, which would otherwise point at stale offsets.

Outputs (in ./cenc next to this script):
  init.mp4          ftyp + encrypted moov (the init segment)
  encrypted.mp4     init.mp4 followed by the encrypted fragments
  plain_mdat.bin    concatenated plaintext mdat payloads (the expected decryption result)

Requires ffmpeg and pycryptodome.  Run: python3 make_cenc_fixture.py
"""
import hashlib
import os
import struct
import subprocess
import tempfile

from Crypto.Cipher import AES

KEY = bytes.fromhex("00112233445566778899aabbccddeeff")
KID = bytes.fromhex("0102030405060708090a0b0c0d0e0f10")
HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "cenc")


def box(t, body):
    return struct.pack(">I4s", 8 + len(body), t.encode()) + body


def header_len(raw):
    return 16 if struct.unpack(">I", raw[:4])[0] == 1 else 8


def body_of(raw):
    return raw[header_len(raw):]


def kind(raw):
    return raw[4:8].decode("latin1")


def parse(data):
    """Splits a byte string into complete boxes (raw bytes, header included)."""
    out, off = [], 0
    while off < len(data):
        size = struct.unpack(">I", data[off:off + 4])[0]
        if size == 1:
            size = struct.unpack(">Q", data[off + 8:off + 16])[0]
        elif size == 0:
            size = len(data) - off
        out.append(data[off:off + size])
        off += size
    return out


def find(raws, t):
    return [r for r in raws if kind(r) == t]


def tenc_box():
    # version 0, reserved, reserved, isProtected=1, per-sample IV size=8, KID
    return box("tenc", bytes(4) + bytes([0, 0, 1, 8]) + KID)


def sinf_box(orig_format):
    frma = box("frma", orig_format)
    schm = box("schm", bytes(4) + b"cenc" + struct.pack(">I", 0x00010000))
    schi = box("schi", tenc_box())
    return box("sinf", frma + schm + schi)


def fix_trak(raw):
    """Returns (new trak bytes, track_id, handler)."""
    kids = parse(body_of(raw))
    track_id = None
    handler = None
    new_kids = []
    for k in kids:
        t = kind(k)
        if t == "tkhd":
            track_id = struct.unpack(">I", body_of(k)[12:16])[0]
            new_kids.append(k)
        elif t == "mdia":
            mdia_kids = []
            for mk in parse(body_of(k)):
                mt = kind(mk)
                if mt == "hdlr":
                    handler = body_of(mk)[8:12].decode()
                    mdia_kids.append(mk)
                elif mt == "minf":
                    minf_kids = []
                    for ik in parse(body_of(mk)):
                        if kind(ik) == "stbl":
                            stbl_kids = []
                            for sk in parse(body_of(ik)):
                                if kind(sk) == "stsd":
                                    stbl_kids.append(encrypt_stsd(sk, handler))
                                else:
                                    stbl_kids.append(sk)
                            minf_kids.append(box("stbl", b"".join(stbl_kids)))
                        else:
                            minf_kids.append(ik)
                    mdia_kids.append(box("minf", b"".join(minf_kids)))
                else:
                    mdia_kids.append(mk)
            new_kids.append(box("mdia", b"".join(mdia_kids)))
        else:
            new_kids.append(k)
    return box("trak", b"".join(new_kids)), track_id, handler


def encrypt_stsd(raw, handler):
    b = body_of(raw)
    head, rest = b[:8], b[8:]  # version/flags + entry_count
    entries = []
    for e in parse(rest):
        fmt = kind(e)
        if fmt not in ("avc1", "mp4a"):
            raise SystemExit(f"unexpected sample entry {fmt}")
        eb = body_of(e)
        hl = 78 if handler == "vide" else 28  # visual / audio sample entry header
        children = parse(eb[hl:])
        new_body = eb[:hl] + b"".join(children) + sinf_box(fmt.encode())
        entries.append(box("encv" if handler == "vide" else "enca", new_body))
    return box("stsd", head + b"".join(entries))


def fix_moov(raw):
    kids = []
    tracks = {}
    for k in parse(body_of(raw)):
        if kind(k) == "trak":
            new_trak, tid, handler = fix_trak(k)
            tracks[tid] = handler
            kids.append(new_trak)
        else:
            kids.append(k)
    return box("moov", b"".join(kids)), tracks


def sample_sizes(trun_body, tfhd_default):
    flags = struct.unpack(">I", b"\x00" + trun_body[1:4])[0]
    count = struct.unpack(">I", trun_body[4:8])[0]
    off = 8
    data_offset = None
    if flags & 0x1:
        data_offset = struct.unpack(">i", trun_body[off:off + 4])[0]
        off += 4
    if flags & 0x4:
        off += 4
    sizes = []
    for _ in range(count):
        if flags & 0x100:
            off += 4
        if flags & 0x200:
            sizes.append(struct.unpack(">I", trun_body[off:off + 4])[0])
            off += 4
        else:
            sizes.append(tfhd_default)
        if flags & 0x400:
            off += 4
        if flags & 0x800:
            off += 4
    return data_offset, sizes


def build(plain):
    """Returns (init bytes, encrypted file bytes, plaintext mdat concatenation)."""
    top = parse(plain)
    moov_raw = find(top, "moov")[0]
    new_moov, tracks = fix_moov(moov_raw)
    ftyp = find(top, "ftyp")[0]
    init = ftyp + new_moov

    out = [init]
    plain_mdat = []
    i = 0
    while i < len(top):
        raw = top[i]
        t = kind(raw)
        if t in ("ftyp", "moov", "sidx", "mfra"):
            i += 1
            continue
        if t != "moof":
            raise SystemExit(f"unexpected top-level box {t}")
        mdat = top[i + 1]
        assert kind(mdat) == "mdat"
        mdat_payload = bytearray(body_of(mdat))
        plain_mdat.append(bytes(mdat_payload))
        moof_pos = sum(len(x) for x in top[:i])  # absolute offset in the plain file
        mdat_pos = moof_pos + len(raw)           # absolute offset of the mdat box
        mdat_base = mdat_pos + header_len(mdat)  # where the mdat payload starts in the plain file

        moof_kids = parse(body_of(raw))
        growth = 0
        new_moof_kids = []
        for k in moof_kids:
            if kind(k) != "traf":
                new_moof_kids.append(k)
                continue
            traf_kids = parse(body_of(k))
            tfhd = find(traf_kids, "tfhd")[0]
            tfb = body_of(tfhd)
            tflags = struct.unpack(">I", b"\x00" + tfb[1:4])[0]
            assert tflags & 0x1 == 0, "base-data-offset not supported by this fixture"
            assert tflags & 0x20000, "fixture expects default-base-is-moof"
            tid = struct.unpack(">I", tfb[4:8])[0]
            default_size = 0
            p = 8
            if tflags & 0x2:
                p += 4
            if tflags & 0x8:
                p += 4
            if tflags & 0x10:
                default_size = struct.unpack(">I", tfb[p:p + 4])[0]
            truns = find(traf_kids, "trun")
            assert len(truns) == 1, "fixture expects one trun per traf"
            data_offset, sizes = sample_sizes(body_of(truns[0]), default_size)
            start = moof_pos + data_offset
            handler = tracks[tid]

            entries = []
            cursor = start
            for n, size in enumerate(sizes):
                iv = hashlib.sha256(f"{tid}-{n}".encode()).digest()[:8]
                clear = min(32, size) if handler == "vide" else 0
                enc_len = size - clear
                # Encrypt in place inside the plaintext mdat buffer.
                rel = cursor - mdat_base
                sample = bytes(mdat_payload[rel:rel + size])
                ciphertext = AES.new(KEY, AES.MODE_CTR, nonce=iv).encrypt(sample[clear:])
                mdat_payload[rel + clear:rel + size] = ciphertext
                entries.append(iv + struct.pack(">HHI", 1, clear, enc_len))
                cursor += size
            senc = box("senc", b"\x00\x00\x00\x02" + struct.pack(">I", len(sizes)) + b"".join(entries))
            growth += len(senc)
            new_traf_kids = []
            for tk in traf_kids:
                if kind(tk) == "trun":
                    tb = bytearray(body_of(tk))
                    old = struct.unpack(">i", bytes(tb[8:12]))[0]
                    tb[8:12] = struct.pack(">i", old)  # patched below once growth is known
                    new_traf_kids.append(box("trun", bytes(tb)))
                else:
                    new_traf_kids.append(tk)
            new_traf_kids.append(senc)
            new_moof_kids.append(("traf", new_traf_kids))

        # Final moof: senc grows the moof, so every trun data offset moves by the total growth.
        rebuilt = []
        for item in new_moof_kids:
            if isinstance(item, tuple):
                kids = []
                for tk in item[1]:
                    if kind(tk) == "trun":
                        tb = bytearray(body_of(tk))
                        old = struct.unpack(">i", bytes(tb[8:12]))[0]
                        tb[8:12] = struct.pack(">i", old + growth)
                        kids.append(box("trun", bytes(tb)))
                    else:
                        kids.append(tk)
                rebuilt.append(box("traf", b"".join(kids)))
            else:
                rebuilt.append(item)
        out.append(box("moof", b"".join(rebuilt)))
        out.append(box("mdat", bytes(mdat_payload)))
        i += 2
    return init, b"".join(out), b"".join(plain_mdat)


def make_plain(path):
    subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
         "-f", "lavfi", "-i", "testsrc=duration=1:size=320x240:rate=10",
         "-f", "lavfi", "-i", "sine=duration=1",
         "-c:v", "libx264", "-c:a", "aac", "-shortest",
         "-movflags", "frag_keyframe+empty_moov+default_base_moof", path],
        check=True,
    )


def main():
    os.makedirs(OUT, exist_ok=True)
    with tempfile.TemporaryDirectory() as d:
        plain_path = os.path.join(d, "plain.mp4")
        make_plain(plain_path)
        with open(plain_path, "rb") as f:
            plain = f.read()
    init, encrypted, plain_mdat = build(plain)
    with open(os.path.join(OUT, "init.mp4"), "wb") as f:
        f.write(init)
    with open(os.path.join(OUT, "encrypted.mp4"), "wb") as f:
        f.write(encrypted)
    with open(os.path.join(OUT, "plain_mdat.bin"), "wb") as f:
        f.write(plain_mdat)
    print(f"wrote {OUT}: init {len(init)} B, encrypted {len(encrypted)} B, plaintext mdat {len(plain_mdat)} B")


if __name__ == "__main__":
    main()
