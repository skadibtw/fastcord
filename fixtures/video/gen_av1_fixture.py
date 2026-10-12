#!/usr/bin/env python3
"""Generate genuine AV1 and independent raw-pixel oracles, without FFmpeg.

Requires Python 3 and libaom 3.15.2's aomenc and aomdec (not dav1d or this Rust
decoder; aomdec is looked up next to aomenc unless --aomdec is given):
    python fixtures/video/gen_av1_fixture.py --aomenc /path/to/aomenc

The committed files were generated with the pinned MSYS2 UCRT64 package below.
Use the same package for byte-identical encoded output. Other libaom builds may
encode differently; the lossless source pixels are unchanged.

Oracle: the expected NV12 comes from libaom's reference decoder (aomdec), the
decoder every AV1 decoder is conformance-tested against, converted by the
documented NV12 contract below. Each stream is also checked, sample by sample,
against the synthetic arithmetic source: libaom's lossless mode reproduces every
luma, monochrome and 4:4:4 sample exactly, and every chroma sample except those in
the last column/row of an odd-dimension subsampled plane, which its encoder does not
preserve (the reference decoder defines them). Any other deviation aborts.
The lossy streams (filters, hidden and show-existing frames, film grain, screen
content) have no source oracle, since lossy decoding is defined by the AV1
specification: their expected pictures are the reference decoder's output only.
No source/encoder binaries or temporary raw input files are shipped by fastcord.
"""

import argparse
import hashlib
import json
import pathlib
import struct
import subprocess
import tempfile

WIDTH, HEIGHT, FRAMES = 35, 27, 3
PACKAGE_URL = (
    "https://mirror.msys2.org/mingw/ucrt64/"
    "mingw-w64-ucrt-x86_64-aom-3.15.2-1-any.pkg.tar.zst"
)
PACKAGE_SHA256 = "e81f01d419f262e31275771fad18f93617f7b321deeb416e5ac39d70f295b286"
AOMENC_SHA256 = "34bb1e90173b7a5271ca93a01e85c367ef053e2c37e2538b2bb9ca5887f7d858"
LIBAOM_SHA256 = "61c2c12b0fd9fb42b4cdb5eca107eded4abe6ba8d59941e33737260da6f5355e"
AOMDEC_SHA256 = "002770b850f4409cbd71b2a36b2f42cf51cc15872b5327163a4b36e88341efb0"
COMMON_OPTIONS = [
    "--debug", "--codec=av1", "--good", "--cpu-used=6", "--passes=1",
    "--threads=1", "--row-mt=0", "--lag-in-frames=0", "--auto-alt-ref=0",
    "--lossless=1", "--end-usage=q", "--cq-level=0", "--min-q=0", "--max-q=0",
    "--kf-min-dist=3", "--kf-max-dist=3", "--fps=30/1", "--ivf",
]
FORMATS = [
    ("420-8", "420", 8, 0),
    ("420-10", "420", 10, 0),
    ("420-12", "420", 12, 2),
    ("mono-8", "mono", 8, 0),
    ("422-10", "422", 10, 2),
    ("444-8", "444", 8, 1),
]

# Lossy 8-bit 4:2:0 streams with the encoder's in-loop tools (deblocking, CDEF,
# loop restoration, palette/intra block copy for screen content, film grain),
# 130x66 so superblock and 8x8 edges are partial. The lossless streams above
# disable every filter, so these are what exercise dav1d's filter code.
LOSSY_WIDTH, LOSSY_HEIGHT = 130, 66
LOSSY_OPTIONS = [
    "--debug", "--codec=av1", "--good", "--threads=1", "--row-mt=0",
    "--end-usage=q", "--cq-level=38", "--min-q=20", "--max-q=63", "--fps=30/1", "--ivf",
    "--enable-cdef=1", "--enable-restoration=1", "--profile=0",
    "--color-primaries=bt709", "--transfer-characteristics=bt709",
    "--matrix-coefficients=bt709",
]
LOSSY_STREAMS = [
    ("lossy-420-8", 5, "natural",
     ["--passes=1", "--cpu-used=4", "--lag-in-frames=0", "--auto-alt-ref=0",
      "--kf-min-dist=5", "--kf-max-dist=5"],
     "Gradients, stripes and texture translating between pictures: a keyframe and four "
     "inter frames with the default filters."),
    ("lagged-420-8", 16, "natural",
     ["--passes=2", "--cpu-used=4", "--lag-in-frames=16", "--auto-alt-ref=1",
      "--min-gf-interval=4", "--max-gf-interval=8",
      "--kf-min-dist=16", "--kf-max-dist=16"],
     "Two-pass lookahead: temporal units that carry several hidden frames before the "
     "shown one, and show-existing-frame units that carry only a frame header."),
    ("grain-420-8", 4, "natural",
     ["--passes=1", "--cpu-used=4", "--lag-in-frames=0", "--auto-alt-ref=0",
      "--film-grain-test=1", "--kf-min-dist=4", "--kf-max-dist=4"],
     "Film grain synthesis parameters (libaom test vector 1) applied on output."),
    ("screen-420-8", 4, "screen",
     ["--passes=1", "--cpu-used=1", "--lag-in-frames=0", "--auto-alt-ref=0",
      "--tune-content=screen", "--kf-min-dist=4", "--kf-max-dist=4"],
     "Flat colours, glyph-like bitmaps and rules scrolling three rows per picture, tuned "
     "for screen content (palette and intra block copy)."),
]


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def source_planes(frame, layout, bitdepth):
    """Pure arithmetic oracle, independent of encoder/decoder implementation."""
    shift = bitdepth - 8
    low_mask = (1 << shift) - 1

    def value(plane, x, y):
        if plane == 0:
            base = (17 + frame * 29 + x * 7 + y * 11 + (x * y) % 19) & 255
        else:
            base = (73 + plane * 61 + frame * 13 + x * 9 + y * 5) & 255
        return (base << shift) | ((x * 3 + y * 5 + frame) & low_mask)

    cw = WIDTH if layout == "444" else (WIDTH + 1) // 2
    ch = (HEIGHT + 1) // 2 if layout in ("420", "mono") else HEIGHT
    y = [[value(0, x, row) for x in range(WIDTH)] for row in range(HEIGHT)]
    u = [[value(1, x, row) for x in range(cw)] for row in range(ch)]
    v = [[value(2, x, row) for x in range(cw)] for row in range(ch)]
    return y, u, v


def planar_bytes(planes, bitdepth):
    samples = [sample for plane in planes for row in plane for sample in row]
    if bitdepth == 8:
        return bytes(samples)
    return struct.pack("<" + "H" * len(samples), *samples)


def expected_nv12(planes, layout, bitdepth, width=WIDTH, height=HEIGHT):
    """The documented 8-bit output contract for planar pictures.

    Chroma box averages floor in source precision, then drop low 2/4 bits;
    odd last rows/columns replicate. The planes are libaom's reference-decoder
    output (see reference_planes), never dav1d's or this Rust decoder's.
    """
    shift = bitdepth - 8
    result = bytearray(sample >> shift for row in planes[0] for sample in row)
    for row in range((height + 1) // 2):
        for column in range((width + 1) // 2):
            for plane in planes[1:]:
                if layout == "mono":
                    result.append(128)
                    continue
                ys = [row] if layout == "420" else [
                    row * 2, min(row * 2 + 1, height - 1)
                ]
                xs = [column] if layout != "444" else [
                    column * 2, min(column * 2 + 1, width - 1)
                ]
                samples = [plane[y][x] for y in ys for x in xs]
                result.append((sum(samples) // len(samples)) >> shift)
    return bytes(result)


def reference_planes(frame_bytes, layout, bitdepth, width=WIDTH, height=HEIGHT):
    """Split one picture of aomdec --rawvideo output into rows of samples.

    The output is planar, 16-bit little endian above 8 bits, and luma-only for
    monochrome (neutral chroma planes stand in for the unused ones).
    """
    sample_bytes = 1 if bitdepth == 8 else 2
    cw = width if layout == "444" else (width + 1) // 2
    ch = (height + 1) // 2 if layout in ("420", "mono") else height
    sizes = [(width, height)] + ([] if layout == "mono" else [(cw, ch)] * 2)
    if len(frame_bytes) != sum(w * h for w, h in sizes) * sample_bytes:
        raise ValueError("aomdec produced a raw picture of an unexpected size")
    planes, position = [], 0
    for width, height in sizes:
        count = width * height
        if bitdepth == 8:
            samples = list(frame_bytes[position:position + count])
        else:
            samples = list(struct.unpack_from("<" + "H" * count, frame_bytes, position))
        position += count * sample_bytes
        planes.append([samples[row * width:(row + 1) * width] for row in range(height)])
    if layout == "mono":
        neutral = [[1 << (bitdepth - 1)] * cw for _ in range(ch)]
        planes += [neutral, neutral]
    return planes


def odd_edge_mismatches(reference, source, layout):
    """Counts reference-vs-source differences, all of which must be chroma
    samples in the last column/row of an odd-dimension subsampled plane."""
    count = 0
    for plane_index, (got, want) in enumerate(zip(reference, source)):
        if layout == "mono" and plane_index:
            continue
        for row, (got_row, want_row) in enumerate(zip(got, want)):
            for column, (a, b) in enumerate(zip(got_row, want_row)):
                if a == b:
                    continue
                edge = (plane_index > 0 and layout in ("420", "422") and (
                    (WIDTH % 2 == 1 and column == len(got_row) - 1)
                    or (layout == "420" and HEIGHT % 2 == 1 and row == len(got) - 1)))
                if not edge:
                    raise ValueError(
                        "libaom's lossless decode differs from the source in plane "
                        + str(plane_index) + " at x=" + str(column) + " y=" + str(row))
                count += 1
    return count



def lcg(seed):
    return (seed * 1664525 + 1013904223) & 0xFFFFFFFF


def natural_planes(frame, width, height):
    """Gradients, moving stripes and deterministic texture. The whole pattern
    translates by (2, 1) pixels per picture, so inter prediction has real work."""
    def luma(x, y):
        tx, ty = x + 2 * frame, y + frame
        base = (tx * 3 + ty * 2) // 4 % 200
        stripes = 40 if ((tx + 2 * ty) // 7) % 3 == 0 else 0
        noise = (lcg((tx * 73856093) ^ (ty * 19349663)) >> 28) - 8
        return max(16, min(235, base + stripes + noise + 16))

    def chroma(plane, x, y):
        tx, ty = x + frame, y + (frame + 1) // 2
        return 128 + ((tx * 5 + ty * 3 + plane * 40) % 64) - 32

    cw, ch = (width + 1) // 2, (height + 1) // 2
    return (
        [[luma(x, y) for x in range(width)] for y in range(height)],
        [[chroma(1, x, y) for x in range(cw)] for y in range(ch)],
        [[chroma(2, x, y) for x in range(cw)] for y in range(ch)],
    )


def screen_planes(frame, width, height):
    """UI-like content: eight flat colors, glyph-like bitmaps in 12x16 cells,
    rules every 64 rows, scrolling three rows per picture. Chroma is the colour of
    the top-left pixel of each 2x2 block: flat regions stay flat."""
    palette = [(235, 128, 128), (16, 128, 128), (82, 90, 240), (145, 54, 34),
               (210, 16, 146), (41, 240, 110), (170, 166, 16), (106, 202, 222)]

    def pixel(x, y):
        ty = y + 3 * frame
        if ty % 64 == 0:
            return palette[1]
        cell = lcg((x // 12) * 2654435761 ^ (ty // 16) * 97)
        bitmap = lcg(cell)
        gx, gy = x % 12 - 3, ty % 16 - 4
        inside = 0 <= gx < 5 and 0 <= gy < 7
        lit = inside and (bitmap >> ((gx + gy * 5) % 31)) & 1
        return palette[2 + (cell >> 20) % 6] if lit else palette[(cell >> 12) % 2]

    cw, ch = (width + 1) // 2, (height + 1) // 2
    return (
        [[pixel(x, y)[0] for x in range(width)] for y in range(height)],
        [[pixel(2 * x, 2 * y)[1] for x in range(cw)] for y in range(ch)],
        [[pixel(2 * x, 2 * y)[2] for x in range(cw)] for y in range(ch)],
    )


def ivf_packets(data):
    if data[:4] != b"DKIF" or data[8:12] != b"AV01":
        raise ValueError("aomenc did not produce AV1 IVF")
    position = struct.unpack_from("<H", data, 6)[0]
    packets = []
    while position < len(data):
        size, timestamp = struct.unpack_from("<IQ", data, position)
        position += 12
        packet = data[position:position + size]
        if len(packet) != size:
            raise ValueError("aomenc produced truncated IVF")
        packets.append((timestamp, packet, position))
        position += size
    return packets


def obus(data):
    position = 0
    while position < len(data):
        start = position
        header = data[position]
        position += 1
        if header & 4:
            position += 1
        if not header & 2:
            raise ValueError("aomenc emitted an unsized OBU")
        size, shift = 0, 0
        while True:
            byte = data[position]
            position += 1
            size |= (byte & 127) << shift
            if not byte & 128:
                break
            shift += 7
        position += size
        if position > len(data):
            raise ValueError("aomenc emitted truncated OBU data")
        yield (header >> 3) & 15, data[start:position]


def encode(aomenc, work, label, raw, width, height, layout, bitdepth, profile,
           frames, identity=False):
    source = work / (label + ".yuv")
    destination = work / (label + ".ivf")
    source.write_bytes(raw)
    options = COMMON_OPTIONS + [
        "--width=" + str(width), "--height=" + str(height),
        "--limit=" + str(frames), "--profile=" + str(profile),
        "--bit-depth=" + str(bitdepth), "--input-bit-depth=" + str(bitdepth),
        "--i" + ("420" if layout == "mono" else layout),
        "--color-primaries=bt709",
        "--transfer-characteristics=" + ("srgb" if identity else "bt709"),
        "--matrix-coefficients=" + ("identity" if identity else "bt709"),
    ]
    if layout == "mono":
        options.append("--monochrome")
    subprocess.run([str(aomenc), *options, "-o", str(destination), str(source)],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    return destination.read_bytes(), options


def encode_lossy(aomenc, work, label, raw, frames, extra):
    source = work / (label + ".yuv")
    destination = work / (label + ".ivf")
    source.write_bytes(raw)
    options = LOSSY_OPTIONS + extra + [
        "--width=" + str(LOSSY_WIDTH), "--height=" + str(LOSSY_HEIGHT),
        "--limit=" + str(frames), "--i420",
    ]
    subprocess.run([str(aomenc), *options, "-o", str(destination), str(source)],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    return destination.read_bytes(), options


def decode_reference(aomdec, work, label):
    """libaom's reference decode of work/<label>.ivf as raw planar pictures."""
    source = work / (label + ".ivf")
    destination = work / (label + "-reference.yuv")
    subprocess.run([str(aomdec), "--rawvideo", "-o", str(destination), str(source)],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    return destination.read_bytes()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--aomenc", type=pathlib.Path, required=True)
    parser.add_argument("--aomdec", type=pathlib.Path)
    args = parser.parse_args()
    aomenc = args.aomenc.resolve()
    aomdec = (args.aomdec or aomenc.with_name(aomenc.name.replace("aomenc", "aomdec"))).resolve()
    directory = pathlib.Path(__file__).resolve().parent
    encoder_hash = sha256(aomenc.read_bytes())
    decoder_hash = sha256(aomdec.read_bytes())
    sibling_dll = aomenc.with_name("libaom.dll")
    library_hash = sha256(sibling_dll.read_bytes()) if sibling_dll.exists() else None
    metadata = {
        "schema": 1,
        "provenance": {
            "encoder": "libaom aomenc 3.15.2 (independent of dav1d)",
            "package_url": PACKAGE_URL,
            "package_sha256": PACKAGE_SHA256,
            "aomenc_sha256": encoder_hash,
            "aomdec_sha256": decoder_hash,
            "libaom_dll_sha256": library_hash,
            "matches_pinned_windows_encoder": encoder_hash == AOMENC_SHA256
            and decoder_hash == AOMDEC_SHA256 and library_hash == LIBAOM_SHA256,
            "source": "Synthetic arithmetic YUV, owned by fastcord; no third-party media.",
            "oracle": "libaom aomdec reference-decoder pictures converted to NV12 by the "
                      "documented contract. Every luma/monochrome/4:4:4 sample and every "
                      "other chroma sample is asserted equal to the arithmetic source; "
                      "only odd-dimension edge chroma, which libaom's lossless encoder "
                      "does not preserve, follows the reference decoder. Never dav1d.",
            "generator": "gen_av1_fixture.py",
            "regenerate": "python fixtures/video/gen_av1_fixture.py --aomenc /path/to/aomenc",
        },
        "width": WIDTH, "height": HEIGHT, "frames": FRAMES,
        "nv12_bytes_per_frame": WIDTH * HEIGHT + ((WIDTH + 1) // 2) * 2 * ((HEIGHT + 1) // 2),
        "time_base": {"rate": 30, "scale": 1},
        "streams": [], "rejected_streams": [],
    }
    with tempfile.TemporaryDirectory(prefix="fastcord-av1-source-") as temp:
        work = pathlib.Path(temp)
        for label, layout, bitdepth, profile in FORMATS:
            sources = [source_planes(frame, layout, bitdepth) for frame in range(FRAMES)]
            raw = b"".join(planar_bytes(planes, bitdepth) for planes in sources)
            encoded, options = encode(aomenc, work, label, raw, WIDTH, HEIGHT,
                                      layout, bitdepth, profile, FRAMES)
            decoded = decode_reference(aomdec, work, label)
            if len(decoded) % FRAMES:
                raise ValueError("aomdec did not produce whole pictures")
            picture_size = len(decoded) // FRAMES
            references = [
                reference_planes(decoded[i * picture_size:(i + 1) * picture_size],
                                 layout, bitdepth)
                for i in range(FRAMES)
            ]
            edge_mismatches = [odd_edge_mismatches(reference, source, layout)
                               for reference, source in zip(references, sources)]
            expected = [expected_nv12(planes, layout, bitdepth) for planes in references]
            encoded_name = "av1-lossless-" + label + ".ivf"
            expected_name = "av1-lossless-" + label + ".nv12"
            (directory / encoded_name).write_bytes(encoded)
            (directory / expected_name).write_bytes(b"".join(expected))
            packets = ivf_packets(encoded)
            if len(packets) != FRAMES:
                raise ValueError("lossless fixture must have one packet per input picture")
            metadata["streams"].append({
                "encoded": encoded_name, "encoded_sha256": sha256(encoded),
                "expected_nv12": expected_name,
                "expected_nv12_sha256": sha256(b"".join(expected)),
                "layout": layout, "bitdepth": bitdepth,
                "source_mismatches_at_odd_chroma_edge": edge_mismatches,
                "encoder_options": options,
                "pictures": [
                    {"index": frame, "ivf_timestamp": timestamp,
                     "timestamp_ns": frame * 1_000_000_000 // 30,
                     "packet_offset": offset, "packet_bytes": len(packet),
                     "packet_sha256": sha256(packet),
                     "nv12_sha256": sha256(expected[frame]),
                     "pixels": {"y_0_0": expected[frame][0],
                                "y_last": expected[frame][WIDTH * HEIGHT - 1],
                                "uv_first": list(expected[frame][WIDTH * HEIGHT:WIDTH * HEIGHT + 2]),
                                "uv_last": list(expected[frame][-2:])}}
                    for frame, (timestamp, packet, offset) in enumerate(packets)
                ],
            })
        lossy_size = LOSSY_WIDTH * LOSSY_HEIGHT + 2 * ((LOSSY_WIDTH + 1) // 2) * (
            (LOSSY_HEIGHT + 1) // 2)
        metadata["lossy_streams"] = []
        for label, frames, kind, extra, description in LOSSY_STREAMS:
            make = natural_planes if kind == "natural" else screen_planes
            sources = [make(frame, LOSSY_WIDTH, LOSSY_HEIGHT) for frame in range(frames)]
            raw = b"".join(planar_bytes(planes, 8) for planes in sources)
            encoded, options = encode_lossy(aomenc, work, label, raw, frames, extra)
            decoded = decode_reference(aomdec, work, label)
            if len(decoded) != frames * lossy_size:
                raise ValueError("aomdec did not produce every lossy picture")
            references = [
                reference_planes(decoded[i * lossy_size:(i + 1) * lossy_size], "420", 8,
                                 LOSSY_WIDTH, LOSSY_HEIGHT)
                for i in range(frames)
            ]
            if all(reference == list(source) for reference, source in zip(references, sources)):
                raise ValueError("a lossy stream decoded exactly to its source")
            expected = [expected_nv12(planes, "420", 8, LOSSY_WIDTH, LOSSY_HEIGHT)
                        for planes in references]
            encoded_name = "av1-" + label + ".ivf"
            (directory / encoded_name).write_bytes(encoded)
            packets = ivf_packets(encoded)
            if len(packets) != frames:
                raise ValueError("a lossy fixture needs one temporal unit per input picture")
            metadata["lossy_streams"].append({
                "encoded": encoded_name, "encoded_sha256": sha256(encoded),
                "description": description,
                "width": LOSSY_WIDTH, "height": LOSSY_HEIGHT, "frames": frames,
                "expected_nv12_sha256": sha256(b"".join(expected)),
                "encoder_options": options,
                "pictures": [
                    {"index": index, "ivf_timestamp": timestamp,
                     "timestamp_ns": index * 1_000_000_000 // 30,
                     "packet_bytes": len(packet), "packet_sha256": sha256(packet),
                     "nv12_sha256": sha256(expected[index])}
                    for index, (timestamp, packet, _) in enumerate(packets)
                ],
            })
        rgb_raw = planar_bytes(source_planes(0, "444", 8), 8)
        encoded, options = encode(aomenc, work, "identity", rgb_raw, WIDTH, HEIGHT,
                                  "444", 8, 1, 1, identity=True)
        name = "av1-rgb-identity.ivf"
        (directory / name).write_bytes(encoded)
        metadata["rejected_streams"].append({
            "encoded": name, "encoded_sha256": sha256(encoded),
            "reason": "Identity-matrix GBR cannot be labelled NV12 YUV",
            "encoder_options": options,
        })
        for label, width, height in [
            ("too-wide", 4097, 16), ("too-tall", 16, 4097),
            ("too-large-area", 4096, 2305),
        ]:
            luma = bytes([96]) * (width * height)
            chroma = bytes([128]) * (((width + 1) // 2) * ((height + 1) // 2))
            encoded, options = encode(aomenc, work, label, luma + chroma * 2,
                                      width, height, "420", 8, 0, 1)
            packet = ivf_packets(encoded)[0][1]
            header = next(obu for kind, obu in obus(packet) if kind == 1)
            name = "av1-" + label + ".obu"
            (directory / name).write_bytes(header)
            metadata["rejected_streams"].append({
                "encoded": name, "encoded_sha256": sha256(header),
                "width": width, "height": height,
                "reason": "Genuine encoder sequence header exceeds decoder dimension/area limits",
                "encoder_options": options,
            })
    (directory / "av1-fixture.json").write_text(
        json.dumps(metadata, indent=2) + "\n", encoding="utf-8", newline="\n"
    )
    print("Generated", len(metadata["streams"]), "lossless and",
          len(metadata["lossy_streams"]), "filtered lossy AV1 streams and",
          len(metadata["rejected_streams"]), "rejection fixtures")


if __name__ == "__main__":
    main()
