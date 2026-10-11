#!/usr/bin/env python3
"""Generates fixtures/voice/h264-rtp-vectors.json.

An independent implementation of RFC 6184 packetization mode 1 (single NAL
unit, STAP-A, FU-A), RFC 4588 RTX, and Discord's rtpsize transport sealing,
written without reference to the Rust code the vectors test. Keys, SSRCs, and
frame contents are fabricated; nothing here comes from a Discord session.

    python fixtures/voice/gen_h264_rtp_vectors.py

needs pycryptodome (AES-GCM; ChaCha20-Poly1305 with a 24-byte nonce is
XChaCha20-Poly1305). The output is deterministic.
"""
import json
import pathlib
import struct

from Crypto.Cipher import AES, ChaCha20_Poly1305

OUT = pathlib.Path(__file__).with_name("h264-rtp-vectors.json")
KEY = bytes(range(32))
PRIMARY_SSRC, RTX_SSRC = 13960, 13961
PAYLOAD_TYPE, RTX_PAYLOAD_TYPE = 103, 104
BASE_TIMESTAMP = 90_000
START_NONCE = 0x0102_0304
RID = b"100"
START_CODE = b"\x00\x00\x00\x01"


def lcg_bytes(seed, length):
    """Pseudo-random bytes that never contain 00 00 (so no start code can
    form inside a NAL unit, like real emulation-prevented H.264)."""
    state, out = seed, bytearray()
    while len(out) < length:
        state = (state * 1664525 + 1013904223) & 0xFFFFFFFF
        byte = state >> 24
        if byte == 0 and out and out[-1] == 0:
            byte = 0x55
        out.append(byte)
    return bytes(out)


def annexb(nals):
    return b"".join(START_CODE + nal for nal in nals)


def packetize(nals, max_payload, aggregate):
    """RTP payloads of one access unit (RFC 6184 mode 1)."""
    payloads, pending = [], []

    def flush():
        if len(pending) == 1:
            payloads.append(pending[0])
        elif pending:
            nri = max(nal[0] & 0x60 for nal in pending)
            body = b"".join(struct.pack(">H", len(nal)) + nal for nal in pending)
            payloads.append(bytes([nri | 24]) + body)
        pending.clear()

    for nal in nals:
        if len(nal) > max_payload:
            flush()
            indicator = (nal[0] & 0xE0) | 28
            data, first = nal[1:], True
            while data:
                chunk, data = data[: max_payload - 2], data[max_payload - 2:]
                header = (nal[0] & 0x1F) | (0x80 if first else 0) | (0x40 if not data else 0)
                payloads.append(bytes([indicator, header]) + chunk)
                first = False
        elif aggregate:
            size = 1 + sum(2 + len(n) for n in pending) + 2 + len(nal)
            if pending and size > max_payload:
                flush()
            pending.append(nal)
        else:
            payloads.append(nal)
    flush()
    return payloads


def extension_block(elements):
    block = b"".join(bytes([(i << 4) | (len(d) - 1)]) + d for i, d in elements)
    block += b"\x00" * (-len(block) % 4)
    return block


def rtp_header(marker, payload_type, sequence, timestamp, ssrc, ext_words):
    first = 0x80 | 0x10
    return struct.pack(">BBHII", first, (0x80 if marker else 0) | payload_type,
                       sequence, timestamp, ssrc) + struct.pack(">HH", 0xBEDE, ext_words)


def seal(mode, clear, body, nonce):
    suffix = struct.pack(">I", nonce)
    if mode == "aead_aes256_gcm_rtpsize":
        cipher = AES.new(KEY, AES.MODE_GCM, nonce=suffix + b"\x00" * 8)
    else:
        cipher = ChaCha20_Poly1305.new(key=KEY, nonce=suffix + b"\x00" * 20)
    cipher.update(clear)
    ciphertext, tag = cipher.encrypt_and_digest(body)
    return clear + ciphertext + tag + suffix


def primary_packets(payloads, first_sequence, timestamp):
    """(header-clear, body, plaintext payload, sequence) per packet."""
    out = []
    for index, payload in enumerate(payloads):
        sequence = (first_sequence + index) & 0xFFFF
        elements = [(3, bytes([0x12, 0x34, 0x56])), (6, bytes([0, 0, 0])),
                    (7, bytes([1])), (11, RID)]
        block = extension_block(elements)
        clear = rtp_header(index == len(payloads) - 1, PAYLOAD_TYPE, sequence,
                           timestamp, PRIMARY_SSRC, len(block) // 4)
        out.append((clear, block + payload, payload, sequence))
    return out


def rtx_packet(original, original_sequence, rtx_sequence, timestamp, marker):
    block = extension_block([(3, bytes([0x12, 0x34, 0x56])), (12, RID)])
    body = block + struct.pack(">H", original_sequence) + original
    clear = rtp_header(marker, RTX_PAYLOAD_TYPE, rtx_sequence, timestamp,
                       RTX_SSRC, len(block) // 4)
    return clear, body


def main():
    sps = bytes.fromhex("6742c01eda083640")
    pps = bytes.fromhex("68ce3c80")
    idr = bytes([0x65]) + lcg_bytes(1, 2600)
    small_p = bytes([0x41]) + lcg_bytes(2, 299)
    large_p = bytes([0x41]) + lcg_bytes(3, 3999)

    frames = []
    specs = [
        ("keyframe-single-nal-and-fu-a", [sps, pps, idr], 1138, False, 65_533, 0),
        ("keyframe-stap-a-and-fu-a", [sps, pps, idr], 1138, True, 100, 3000),
        ("delta-single-nal", [small_p], 1138, False, 110, 6000),
        ("delta-fu-a", [large_p], 1138, False, 111, 9000),
    ]
    transport_source = None
    for name, nals, max_payload, aggregate, first_sequence, offset in specs:
        payloads = packetize(nals, max_payload, aggregate)
        frames.append({
            "name": name,
            "annexb": annexb(nals).hex(),
            "max_payload": max_payload,
            "first_sequence": first_sequence,
            "timestamp": BASE_TIMESTAMP + offset,
            "payloads": [payload.hex() for payload in payloads],
        })
        if name == "keyframe-stap-a-and-fu-a":
            transport_source = (payloads, first_sequence, BASE_TIMESTAMP + offset)

    payloads, first_sequence, timestamp = transport_source
    packets = primary_packets(payloads, first_sequence, timestamp)
    modes = []
    for mode in ("aead_aes256_gcm_rtpsize", "aead_xchacha20_poly1305_rtpsize"):
        entries, nonce = [], START_NONCE
        for clear, body, payload, sequence in packets:
            entries.append({
                "kind": "media", "sequence": sequence, "nonce": nonce,
                "marker": clear[1] & 0x80 != 0, "payload": payload.hex(),
                "datagram": seal(mode, clear, body, nonce).hex(),
            })
            nonce += 1
        # The server repairs the second packet: RTX stream, original sequence
        # number first, same timestamp and marker bit as the original.
        original = packets[1]
        clear, body = rtx_packet(original[2], original[3], 7, timestamp,
                                 original[0][1] & 0x80 != 0)
        entries.append({
            "kind": "rtx", "sequence": 7, "original_sequence": original[3],
            "nonce": nonce, "marker": clear[1] & 0x80 != 0,
            "payload": original[2].hex(),
            "datagram": seal(mode, clear, body, nonce).hex(),
        })
        modes.append({"mode": mode, "packets": entries})

    document = {
        "_source": (
            "Generated by fixtures/voice/gen_h264_rtp_vectors.py: RFC 6184 "
            "packetization mode 1 (single NAL unit, STAP-A, FU-A), RFC 4588 "
            "RTX, and rtpsize transport sealing with pycryptodome, written "
            "independently of the Rust code under test. Fabricated key, SSRCs "
            "and frames; no Discord traffic. The one-byte header extension "
            "block (abs-send-time 3, playout-delay 6, content-type 7, RID 11; "
            "RTX: 3 and repaired-RID 12) is part of the encrypted body."
        ),
        "key": KEY.hex(),
        "primary_ssrc": PRIMARY_SSRC,
        "rtx_ssrc": RTX_SSRC,
        "payload_type": PAYLOAD_TYPE,
        "rtx_payload_type": RTX_PAYLOAD_TYPE,
        "frames": frames,
        "transport": {"frame": "keyframe-stap-a-and-fu-a", "modes": modes},
    }
    OUT.write_text(json.dumps(document, indent=1) + "\n", encoding="utf-8", newline="\n")


if __name__ == "__main__":
    main()
