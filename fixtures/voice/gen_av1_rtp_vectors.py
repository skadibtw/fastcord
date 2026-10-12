#!/usr/bin/env python3
"""Generate fixtures/voice/av1-rtp-vectors.json independently of Rust.

Sources (not implementations from fastcord):
  * AOM RTP Payload Format for AV1 v1.0.0, 15 December 2024, sections 4-5:
    https://aomediacodec.github.io/av1-rtp-spec/v1.0.0.html
  * AV1 Bitstream & Decoding Process Specification, sections 4.10.5, 5.2-5.3,
    and 6.2: https://aomediacodec.github.io/av1-spec/
  * RFC 4588 section 4: https://www.rfc-editor.org/rfc/rfc4588#section-4
  * RFC 8285: https://www.rfc-editor.org/rfc/rfc8285

The RTP aggregation/fragment layout is constructed from synthetic OBU objects.
Expected bytes are serialized from those objects, NOT by parsing the generated
RTP or calling the Rust implementation. OBU payload bodies are fabricated;
these are framing/transport vectors, not decodable video or DAVE ciphertext.
All keys, SSRCs, nonces, timestamps, and header extensions are fabricated.

RTP normally carries OBUs without their size field. Section 4.5 says SHOULD,
not MUST, so size-bearing OBUs are also covered. Canonical output has minimal
LEB128 OBU body sizes. Temporal delimiters and tile lists are discarded as
required by section 5; padding is retained. dave_obu_stream is the same output
except its final OBU omits its size field, matching the libdave/davey AV1 frame
boundary before a DAVE trailer. That field is NOT an authentication vector.

Negative reserved-bit cases exercise a strict-zero input policy. The RTP
specification labels aggregation bits 5-7 reserved without prescribing receiver
handling; AV1 requires zero OBU reserved bits but says decoders ignore them.
Nonminimal LEB128 representations ARE legal, up to eight bytes and UINT32_MAX.

    python fixtures/voice/gen_av1_rtp_vectors.py

Requires Python 3.10+ and pycryptodome (AES-GCM; ChaCha20-Poly1305 with a 24-byte
nonce is XChaCha20-Poly1305). Output is deterministic, with no random inputs.
The rtpsize layout and fixture schema follow the independent M25 H.264 vectors:
clear fixed RTP header plus extension preamble is AAD, the extension data and
media/RTX body are encrypted, followed by a 16-byte tag and 4-byte BE counter.
AEAD nonce is that counter followed by zeros, to 12 or 24 bytes respectively.
"""
import json
import pathlib
import struct
from dataclasses import dataclass

from Crypto.Cipher import AES, ChaCha20_Poly1305

OUT = pathlib.Path(__file__).with_name("av1-rtp-vectors.json")
KEY = bytes(range(32))
PRIMARY_SSRC, RTX_SSRC = 13970, 13971
PAYLOAD_TYPE, RTX_PAYLOAD_TYPE = 111, 112
BASE_TIMESTAMP = 90_000
START_NONCE = 0x0102_0304
MAX_FRAME_BYTES = 2 * 1024 * 1024
RID = b"100"

SEQUENCE_HEADER, TEMPORAL_DELIMITER = 1, 2
FRAME_HEADER, TILE_GROUP, METADATA, FRAME, TILE_LIST, PADDING = 3, 4, 5, 6, 8, 15


@dataclass(frozen=True)
class Obu:
    kind: int
    body: bytes
    extension: int | None = None


def lcg_bytes(seed, length):
    state, out = seed, bytearray()
    for _ in range(length):
        state = (state * 1664525 + 1013904223) & 0xFFFFFFFF
        out.append(state >> 24)
    return bytes(out)


def leb128(value, width=None):
    """AV1 unsigned LEB128; optional legal nonminimal width, at most 8 bytes."""
    if not 0 <= value <= 0xFFFFFFFF:
        raise ValueError("AV1 LEB128 value must fit uint32")
    out = bytearray()
    while True:
        byte, value = value & 0x7F, value >> 7
        out.append(byte | (0x80 if value else 0))
        if not value:
            break
    if width is not None:
        if not len(out) <= width <= 8:
            raise ValueError("LEB128 width must fit its value and be at most 8")
        if width > len(out):
            out[-1] |= 0x80
            out.extend(b"\x80" * (width - len(out) - 1))
            out.append(0)
    return bytes(out)


def wire_obu(obu, size_field=False, size_width=None):
    """The internal size counts body bytes, excluding both OBU headers."""
    header = (obu.kind << 3) | (4 if obu.extension is not None else 0)
    header |= 2 if size_field else 0
    extension = b"" if obu.extension is None else bytes([obu.extension])
    size = leb128(len(obu.body), size_width) if size_field else b""
    return bytes([header]) + extension + size + obu.body


def obu_stream(obus, dave_boundary=False):
    """Serialize source OBUs directly; never use generated RTP as the oracle."""
    retained = [obu for obu in obus if obu.kind not in (TEMPORAL_DELIMITER, TILE_LIST)]
    return b"".join(
        wire_obu(obu, size_field=not (dave_boundary and i == len(retained) - 1))
        for i, obu in enumerate(retained)
    )


def aggregate(elements, w, z=False, y=False, n=False, length_widths=None):
    """AOM 4.4-4.5: Z/Y mark fragments; W=0 sizes all, W>0 omits last size."""
    if not elements or any(not element for element in elements):
        raise ValueError("an OBU element must contain at least one byte")
    if w not in (0, 1, 2, 3) or (w and w != len(elements)):
        raise ValueError("W must be zero or the exact element count (up to 3)")
    if z and n:
        raise ValueError("N and Z cannot both be set")
    if length_widths is None:
        length_widths = [None] * len(elements)
    if len(length_widths) != len(elements):
        raise ValueError("one length width is required per element")
    header = (0x80 if z else 0) | (0x40 if y else 0) | (w << 4) | (8 if n else 0)
    out = bytearray([header])
    for index, element in enumerate(elements):
        if w == 0 or index < len(elements) - 1:
            out.extend(leb128(len(element), length_widths[index]))
        out.extend(element)
    return bytes(out)


def make_frame(name, obus, payloads, first_sequence, timestamp, new_sequence=False,
               notes=""):
    return {
        "name": name,
        "obu_stream": obu_stream(obus).hex(),
        "dave_obu_stream": obu_stream(obus, dave_boundary=True).hex(),
        "first_sequence": first_sequence,
        "timestamp": timestamp,
        "new_sequence": new_sequence,
        "payloads": [payload.hex() for payload in payloads],
        "markers": [i == len(payloads) - 1 for i in range(len(payloads))],
        "notes": notes,
    }


def delivery_case(name, frame, arrival_order):
    missing = sorted(set(range(len(frame["payloads"]))) - set(arrival_order))
    complete = not missing
    return {
        "name": name,
        "frame": frame["name"],
        "arrival_order": arrival_order,
        "missing_sequences": [(frame["first_sequence"] + i) & 0xFFFF for i in missing],
        "expect_complete": complete,
        "expected_frame_count": 1 if complete else 0,
        "finish": "packet" if complete else "deadline",
    }


def extension_block(elements):
    block = b"".join(bytes([(i << 4) | (len(data) - 1)]) + data for i, data in elements)
    return block + b"\x00" * (-len(block) % 4)


def rtp_header(marker, payload_type, sequence, timestamp, ssrc, ext_words):
    return struct.pack(
        ">BBHIIHH", 0x90, (0x80 if marker else 0) | payload_type,
        sequence, timestamp, ssrc, 0xBEDE, ext_words,
    )


def seal(mode, clear, body, nonce):
    suffix = struct.pack(">I", nonce)
    if mode == "aead_aes256_gcm_rtpsize":
        cipher = AES.new(KEY, AES.MODE_GCM, nonce=suffix + b"\x00" * 8)
    elif mode == "aead_xchacha20_poly1305_rtpsize":
        cipher = ChaCha20_Poly1305.new(key=KEY, nonce=suffix + b"\x00" * 20)
    else:
        raise ValueError("unknown transport mode")
    cipher.update(clear)
    ciphertext, tag = cipher.encrypt_and_digest(body)
    return clear + ciphertext + tag + suffix


def transported_frame(frame):
    """Primary packets and fresh-counter RTX repairs of middle/final packets."""
    primary_block = extension_block([
        (3, bytes([0x12, 0x34, 0x56])), (6, bytes([0, 0, 0])),
        (7, bytes([1])), (11, RID),
    ])
    repair_block = extension_block([(3, bytes([0x12, 0x34, 0x56])), (12, RID)])
    plain_packets = []
    for i, payload_hex in enumerate(frame["payloads"]):
        payload = bytes.fromhex(payload_hex)
        sequence = (frame["first_sequence"] + i) & 0xFFFF
        marker = frame["markers"][i]
        clear = rtp_header(marker, PAYLOAD_TYPE, sequence, frame["timestamp"],
                           PRIMARY_SSRC, len(primary_block) // 4)
        plain_packets.append(({
            "kind": "media", "sequence": sequence, "marker": marker,
            "timestamp": frame["timestamp"], "ssrc": PRIMARY_SSRC,
            "payload_type": PAYLOAD_TYPE, "payload": payload_hex,
        }, clear, primary_block + payload))

    # RFC 4588: same timestamp/M as the original, distinct SSRC/PT/sequence;
    # original sequence number is the first two encrypted RTX payload bytes.
    # The third RTX is a repeated repair with another fresh transport nonce.
    for rtx_sequence, original_index in ((7, 2), (8, 4), (9, 2)):
        original = plain_packets[original_index][0]
        payload = bytes.fromhex(original["payload"])
        clear = rtp_header(original["marker"], RTX_PAYLOAD_TYPE, rtx_sequence,
                           frame["timestamp"], RTX_SSRC, len(repair_block) // 4)
        body = repair_block + struct.pack(">H", original["sequence"]) + payload
        plain_packets.append(({
            "kind": "rtx", "sequence": rtx_sequence,
            "original_sequence": original["sequence"], "marker": original["marker"],
            "timestamp": frame["timestamp"], "ssrc": RTX_SSRC,
            "payload_type": RTX_PAYLOAD_TYPE, "payload": original["payload"],
        }, clear, body))

    modes = []
    for mode in ("aead_aes256_gcm_rtpsize", "aead_xchacha20_poly1305_rtpsize"):
        entries = []
        for index, (entry, clear, body) in enumerate(plain_packets):
            nonce = START_NONCE + index
            entries.append({
                **entry, "nonce": nonce, "clear": clear.hex(), "plaintext": body.hex(),
                "datagram": seal(mode, clear, body, nonce).hex(),
            })
        modes.append({"mode": mode, "packets": entries})

    delivery_cases = []
    schedules = [
        ("in-order", [0, 1, 2, 3, 4], [], True),
        ("lost-middle-no-repair", [0, 1, 3, 4], [65535], False),
        ("lost-middle-rtx-repair", [0, 1, 3, 4, 5], [65535], True),
        ("lost-final-marker-rtx-repair", [0, 1, 2, 3, 6], [1], True),
        ("rtx-before-future-primary", [0, 1, 5, 3, 4], [65535], True),
        ("late-fresh-nonce-repair-and-original", [0, 1, 3, 4, 5, 7, 2], [65535], True),
        ("wrap-reorder-and-identical-datagram-duplicate", [0, 3, 1, 2, 4, 4], [], True),
    ]
    for name, order, requested, complete in schedules:
        delivery_cases.append({
            "name": name, "arrival_order": order, "requested_sequences": requested,
            "missing_sequences": [] if complete else requested,
            "expect_complete": complete, "expected_frame_count": 1 if complete else 0,
            "finish": "packet" if complete else "deadline",
        })
    return {"frame": frame["name"], "modes": modes, "delivery_cases": delivery_cases}


def main():
    frames = []

    def add(name, obus, payloads, first_sequence, new_sequence=False, notes=""):
        frame = make_frame(name, obus, payloads, first_sequence,
                           BASE_TIMESTAMP + len(frames) * 3000, new_sequence, notes)
        frames.append(frame)
        return frame

    sequence = Obu(SEQUENCE_HEADER, lcg_bytes(1, 12))
    key_frame = Obu(FRAME, lcg_bytes(2, 37))
    metadata = Obu(METADATA, bytes.fromhex("04b5003180"))
    fh = Obu(FRAME_HEADER, lcg_bytes(3, 11), extension=0)
    tg = Obu(TILE_GROUP, lcg_bytes(4, 41), extension=0)
    sh_wire, key_wire = wire_obu(sequence), wire_obu(key_frame)

    add("sequence-start-keyframe-w1", [sequence, key_frame], [
        aggregate([sh_wire], 1, n=True), aggregate([key_wire], 1),
    ], 100, True, "N is the sequence-start/keyframe hint; only the first packet sets N.")
    add("sequence-start-keyframe-w2", [sequence, key_frame], [
        aggregate([sh_wire, key_wire], 2, n=True),
    ], 120, True)
    add("sequence-start-keyframe-w3", [sequence, metadata, key_frame], [
        aggregate([sh_wire, wire_obu(metadata), key_wire], 3, n=True),
    ], 130, True)
    add("sequence-start-keyframe-w0-four-obus", [sequence, metadata, fh, tg], [
        aggregate([wire_obu(obu) for obu in (sequence, metadata, fh, tg)], 0, n=True),
    ], 140, True)

    delta = Obu(FRAME, lcg_bytes(5, 29))
    add("delta-w1", [delta], [aggregate([wire_obu(delta)], 1)], 150)
    add("delta-w2-extension-headers", [fh, tg], [
        aggregate([wire_obu(fh), wire_obu(tg)], 2),
    ], 160)
    layer_fh = Obu(FRAME_HEADER, lcg_bytes(6, 13), extension=0x28)
    layer_tg = Obu(TILE_GROUP, lcg_bytes(7, 27), extension=0x28)
    add("delta-w3-nonzero-layer-ids", [metadata, layer_fh, layer_tg], [
        aggregate([wire_obu(obu) for obu in (metadata, layer_fh, layer_tg)], 3),
    ], 170, notes="Both extension headers carry temporal_id=1 and spatial_id=1.")
    padding = Obu(PADDING, bytes.fromhex("008000"))
    add("delta-w0-four-obus", [metadata, fh, tg, padding], [
        aggregate([wire_obu(obu) for obu in (metadata, fh, tg, padding)], 0),
    ], 180)
    boundary_fh = Obu(FRAME_HEADER, lcg_bytes(8, 127))
    boundary_tg = Obu(TILE_GROUP, lcg_bytes(9, 128))
    add("delta-two-byte-element-and-obu-lengths", [boundary_fh, boundary_tg], [
        aggregate([wire_obu(boundary_fh), wire_obu(boundary_tg)], 0),
    ], 190, notes="Element sizes 128/129; canonical OBU body sizes 127/128.")

    fragmented = Obu(FRAME, lcg_bytes(10, 131), extension=0x20)
    data = wire_obu(fragmented)
    add("fragmented-extension-header-w1", [fragmented], [
        aggregate([data[:1]], 1, y=True),
        aggregate([data[1:57]], 1, z=True, y=True),
        aggregate([data[57:]], 1, z=True),
    ], 200, notes="First fragment ends before the OBU extension byte.")

    mixed_fh = Obu(FRAME_HEADER, lcg_bytes(11, 80), extension=0)
    mixed_tg = Obu(TILE_GROUP, lcg_bytes(12, 200), extension=0)
    first, second = wire_obu(mixed_fh), wire_obu(mixed_tg)
    transport_frame = add("sequence-start-mixed-fragments-wrap", [
        sequence, mixed_fh, metadata, mixed_tg,
    ], [
        aggregate([sh_wire, first[:29]], 2, y=True, n=True),
        aggregate([first[29:48]], 1, z=True, y=True),
        aggregate([first[48:], wire_obu(metadata), second[:55]], 3, z=True, y=True),
        aggregate([second[55:118]], 0, z=True, y=True),
        aggregate([second[118:]], 1, z=True),
    ], 65533, True, "Packet 2 ends the old fragment, contains a complete OBU, and begins "
       "a new fragment. Sequence numbers are 65533,65534,65535,0,1.")

    sized = Obu(FRAME, lcg_bytes(13, 140), extension=0x20)
    add("legal-rtp-size-fields-w2", [metadata, sized], [
        aggregate([wire_obu(metadata, True, 2), wire_obu(sized, True)], 2),
    ], 220, notes="Size fields are legal on RTP; the first is nonminimal and normalized.")
    sized_fragment = Obu(FRAME, lcg_bytes(14, 160), extension=0x20)
    data = wire_obu(sized_fragment, True)
    add("legal-rtp-size-field-split-leb128-w0", [sized_fragment], [
        aggregate([data[:3]], 0, y=True),
        aggregate([data[3:71]], 0, z=True, y=True),
        aggregate([data[71:]], 0, z=True),
    ], 230, notes="First fragment ends inside the two-byte internal OBU size field.")
    tiny = Obu(FRAME, bytes.fromhex("0102"))
    add("legal-eight-byte-element-length", [tiny], [
        aggregate([wire_obu(tiny)], 0, length_widths=[8]),
    ], 240, notes="Nonminimal eight-byte LEB128 encodes element length 3.")
    add("legal-eight-byte-obu-size", [tiny], [
        aggregate([wire_obu(tiny, True, 8)], 1),
    ], 250, notes="Nonminimal eight-byte LEB128 body size 2 is canonicalized.")

    delimiter = Obu(TEMPORAL_DELIMITER, b"")
    tile_list = Obu(TILE_LIST, lcg_bytes(15, 29))
    add("ignore-temporal-delimiter-and-tile-list", [delimiter, sequence, tile_list, key_frame], [
        aggregate([wire_obu(obu) for obu in (delimiter, sequence, tile_list, key_frame)],
                  0, n=True),
    ], 260, True, "Receiver MUST ignore both TD and tile list, even if sender kept them.")
    tile_wire = wire_obu(tile_list)
    add("ignore-fragmented-tile-list", [fh, tile_list, tg], [
        aggregate([wire_obu(fh), tile_wire[:7]], 2, y=True),
        aggregate([tile_wire[7:20]], 0, z=True, y=True),
        aggregate([tile_wire[20:], wire_obu(tg)], 2, z=True),
    ], 270, notes="Discard type 8 only after its fragments have been consumed correctly.")
    add("retain-padding-including-zero-size", [delta, padding, Obu(PADDING, b"")], [
        aggregate([wire_obu(delta), wire_obu(padding), wire_obu(Obu(PADDING, b""))], 3),
    ], 280, notes="Generic RTP keeps padding. Last empty OBU is 7a00, or 78 at DAVE boundary.")

    delivery_cases = [delivery_case(name, transport_frame, order) for name, order in [
        ("in-order-wrap", [0, 1, 2, 3, 4]),
        ("reorder-two-fragments-across-wrap", [0, 2, 1, 4, 3]),
        ("duplicate-and-reorder-across-wrap", [0, 1, 1, 3, 2, 4, 4]),
        ("missing-first-fragment", [1, 2, 3, 4]),
        ("missing-middle-old-fragment", [0, 2, 3, 4]),
        ("missing-packet-ending-old-and-starting-new-fragment", [0, 1, 3, 4]),
        ("missing-middle-new-fragment", [0, 1, 2, 4]),
        ("missing-final-marker-fragment", [0, 1, 2, 3]),
    ]]

    malformed = []

    def bad(name, reason, payloads, markers=None, offsets=None, outcome="reject", finish="packet"):
        index = len(malformed)
        if markers is None:
            markers = [i == len(payloads) - 1 for i in range(len(payloads))]
        if offsets is None:
            offsets = [0] * len(payloads)
        malformed.append({
            "name": name, "reason": reason, "outcome": outcome,
            "expect_complete": False, "expected_frame_count": 0, "finish": finish,
            "packets": [{
                "sequence": 2000 + index * 4 + i,
                "timestamp": BASE_TIMESTAMP + 120_000 + index * 3000 + offsets[i],
                "marker": markers[i], "payload": payload.hex(),
            } for i, payload in enumerate(payloads)],
        })

    valid_obu = wire_obu(Obu(FRAME, bytes.fromhex("aabb")))
    valid_packet = aggregate([valid_obu], 1)
    nine_byte_three = b"\x83" + b"\x80" * 7 + b"\x00"
    uint32_overflow = bytes.fromhex("8080808010")
    bad("empty-payload", "No aggregation header.", [b""])
    bad("aggregation-without-elements-w0", "At least one OBU element is required.", [b"\x00"])
    bad("aggregation-without-element-w1", "Implicit final OBU length is zero.", [b"\x10"])
    bad("zero-element-length-w0", "An element cannot omit even its OBU header.", [b"\x00\x00"])
    bad("truncated-element-body", "Length is present but no element bytes follow.", [b"\x00\x01"])
    bad("truncated-element-leb128", "Continuation byte has no next LEB128 byte.", [b"\x00\x80"])
    bad("truncated-multibyte-element-leb128", "Length ends before a terminating byte.",
        [b"\x00\x81\x80"])
    bad("unterminated-eight-byte-element-length", "Byte 8 must have its continuation bit clear.",
        [b"\x00" + b"\x80" * 8])
    bad("overlong-nine-byte-element-length", "Length 3 encoded using forbidden ninth byte.",
        [b"\x00" + nine_byte_three + valid_obu])
    bad("element-length-uint32-overflow", "Decoded length 2^32 violates AV1's UINT32_MAX bound.",
        [b"\x00" + uint32_overflow + valid_obu])
    bad("eight-byte-element-length-uint32-overflow", "Terminated 8-byte value is greater than UINT32_MAX.",
        [b"\x00" + b"\xff" * 7 + b"\x7f" + valid_obu])
    bad("element-length-overlong-and-uint64-overflow", "Malicious ten-byte integer is neither AV1 LEB128 nor u64.",
        [b"\x00" + b"\xff" * 9 + b"\x02" + valid_obu])
    bad("element-length-exceeds-frame-bound", "Declared length exceeds 2 MiB and available packet bytes.",
        [b"\x00" + leb128(MAX_FRAME_BYTES + 1) + valid_obu])
    bad("element-length-exceeds-payload", "Declared length is larger than the payload remainder.",
        [b"\x00\x07" + valid_obu])
    bad("truncated-length-after-complete-element", "Trailing LEB128 is incomplete, not a second OBU.",
        [aggregate([valid_obu], 0) + b"\x80"])
    bad("w2-missing-final-element", "Explicit first element consumes all bytes; implicit second is empty.",
        [b"\x20" + leb128(len(valid_obu)) + valid_obu])
    bad("w3-only-two-elements", "Second OBU header is misread as an impossible explicit element length.",
        [bytes([0x30]) + aggregate([valid_obu, valid_obu], 2)[1:]])
    bad("w2-missing-first-length", "W=2 requires a length before its first element.",
        [b"\x20" + valid_obu + valid_obu])
    bad("w0-missing-final-length", "W=0 requires a length before every element.",
        [b"\x00" + aggregate([valid_obu, valid_obu], 2)[1:]])
    bad("w1-unexpected-length-before-final", "Forbidden final length 3 becomes an invalid reserved-bit OBU header.",
        [b"\x10\x03" + valid_obu])
    for bit in (1, 2, 4):
        bad(f"reserved-aggregation-bit-{bit}", "Strict-zero reserved aggregation-bit policy.",
            [bytes([valid_packet[0] | bit]) + valid_packet[1:]])
    bad("obu-forbidden-bit", "AV1 obu_forbidden_bit must be zero.",
        [aggregate([bytes([valid_obu[0] | 0x80]) + valid_obu[1:]], 1)])
    bad("obu-reserved-bit", "AV1 obu_reserved_1bit must be zero; strict input policy rejects it.",
        [aggregate([bytes([valid_obu[0] | 1]) + valid_obu[1:]], 1)])
    for bit in (1, 2, 4):
        bad(f"obu-extension-reserved-bit-{bit}", "Extension's low three reserved bits must be zero.",
            [aggregate([bytes([0x34, 0x20 | bit, 0xAA])], 1)])
    bad("truncated-obu-extension", "Extension flag is set but extension header is missing.", [b"\x10\x34"])
    bad("missing-internal-obu-size", "Size flag is set but no size field follows.", [b"\x10\x32"])
    bad("truncated-internal-obu-size", "Internal OBU size LEB128 is unterminated.", [b"\x10\x32\x80"])
    bad("internal-obu-size-exceeds-element", "Internal body size 5 exceeds two available bytes.",
        [b"\x10\x32\x05\xaa\xbb"])
    bad("internal-obu-size-leaves-trailing-data", "An OBU element contains bytes beyond its declared OBU size.",
        [b"\x10\x32\x01\xaa\xbb"])
    bad("overlong-nine-byte-internal-obu-size", "Internal body size uses a forbidden ninth LEB128 byte.",
        [b"\x10\x32" + nine_byte_three + b"\xaa\xbb\xcc"])
    bad("internal-obu-size-uint32-overflow", "Internal body size 2^32 violates AV1's size bound.",
        [b"\x10\x32" + uint32_overflow + b"\xaa"])
    bad("n-and-z-set", "New sequence cannot start with a continuation: N=1 requires Z=0.",
        [bytes([valid_packet[0] | 0x88]) + valid_packet[1:]])
    bad("continuation-without-start", "Z=1 but there is no preceding fragmented OBU.", [b"\x90\xaa\xbb"])
    bad("middle-continuation-without-start", "Z=Y=1 cannot create a fragment without an earlier start.",
        [b"\xd0\xaa\xbb"], markers=[False], finish="deadline")

    broken_obu = wire_obu(Obu(FRAME, b"\xe0\xe0\xe0\xe0\xe0"))
    start = aggregate([broken_obu[:3]], 1, y=True)
    tail = aggregate([broken_obu[3:]], 1, z=True)
    bad("fragment-tail-without-z", "Continuation packet incorrectly claims to start a new OBU.",
        [start, aggregate([broken_obu[3:]], 1)])
    bad("z-after-complete-obu", "A complete nonfragmented OBU cannot be continued.",
        [valid_packet, tail])
    bad("marker-on-incomplete-final-y", "Marker packet still advertises an unfinished final OBU.", [start])
    bad("incomplete-final-y-at-deadline", "No marker or continuation completes the last Y=1 fragment.",
        [start], markers=[False], outcome="drop_incomplete", finish="deadline")
    bad("middle-packet-ends-old-starts-unfinished-new", "Old OBU ends, but the new last OBU has Y=1 at marker.",
        [start, aggregate([broken_obu[3:], valid_obu[:2]], 2, z=True, y=True)])
    bad("fragment-crosses-timestamp", "A continuation cannot cross a temporal-unit timestamp boundary.",
        [start, tail], offsets=[0, 3000], outcome="drop_incomplete")

    document = {
        "schema_version": 1,
        "_source": (
            "Generated by fixtures/voice/gen_av1_rtp_vectors.py, independent of Rust: "
            "AOM AV1 RTP v1.0.0 sections 4-5, AV1 LEB128/OBU syntax, RFC 4588 RTX, "
            "RFC 8285 one-byte extensions, and pycryptodome rtpsize sealing. "
            "All keys, packets, and codec bodies are synthetic. These are not "
            "decoder or DAVE authentication vectors. Expected output is serialized "
            "directly from source OBU objects, not decoded from generated packets."
        ),
        "_sources": {
            "av1_rtp": "https://aomediacodec.github.io/av1-rtp-spec/v1.0.0.html",
            "av1_bitstream": "https://aomediacodec.github.io/av1-spec/",
            "rtx": "https://www.rfc-editor.org/rfc/rfc4588#section-4",
            "extensions": "https://www.rfc-editor.org/rfc/rfc8285",
        },
        "_schema": {
            "frames": "In-order payloads; sequence=(first_sequence+index)&65535, one timestamp, "
                      "markers[index]. Compare obu_stream or dave_obu_stream byte-for-byte. "
                      "new_sequence is the first aggregation header N bit, not parsed codec data.",
            "delivery_cases": "frame names a frames[] entry. arrival_order indexes its payloads "
                              "and may repeat indices. Only complete frames may be emitted, once.",
            "malformed": "Each case starts with fresh receiver state and explicit packet headers. "
                         "No frame is emitted. reject means malformed input; drop_incomplete means "
                         "a frame never completes. finish=deadline requires expiring pending state. "
                         "Reserved bits use strict-zero validation, not a claim that AOM mandates "
                         "all receiver rejections. Error names are semantic, not Rust enum variants.",
            "transport": "frame identifies both output expectations. Each mode has five media "
                         "packets then three RTX repairs. arrival_order indexes mode.packets. "
                         "clear is AAD; plaintext includes encrypted extension bytes and RTX OSN; "
                         "payload excludes extensions and RTX OSN. RTX retains timestamp/marker "
                         "and uses new SSRC/PT/sequence/nonce. Repeated datagram indices model "
                         "network duplication; fresh RTX indices model authenticated duplicate repair.",
            "dave_obu_stream": "After ignoring TD/type8, only the terminal retained OBU clears "
                               "obu_has_size_field and omits its length; all earlier sizes are canonical. "
                               "Padding remains. This is framing only, not encrypted DAVE traffic.",
        },
        "max_frame_bytes": MAX_FRAME_BYTES,
        "key": KEY.hex(),
        "primary_ssrc": PRIMARY_SSRC,
        "rtx_ssrc": RTX_SSRC,
        "payload_type": PAYLOAD_TYPE,
        "rtx_payload_type": RTX_PAYLOAD_TYPE,
        "frames": frames,
        "delivery_cases": delivery_cases,
        "malformed": malformed,
        "transport": transported_frame(transport_frame),
    }
    OUT.write_text(json.dumps(document, indent=1) + "\n", encoding="utf-8", newline="\n")


if __name__ == "__main__":
    main()
