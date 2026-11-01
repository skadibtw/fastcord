#!/usr/bin/env python3
"""Regenerates the compact emoji-index.bin from pinned sources.

Inputs (all in this repository):
  source/emoji-data-16.0.0.json    iamcal/emoji-data v16.0.0 emoji.json (MIT)
  source/twemoji-17.0.3-files.txt  file stems of Twemoji v17.0.3 assets/72x72, sorted
  twemoji-17.0.3.bin               those PNGs concatenated in that order

  python -I assets/emoji/generate.py            write the index
  python -I assets/emoji/generate.py --check    fail if the committed index differs
  python -I assets/emoji/generate.py --twemoji DIR [--write-bin]
      DIR is the extracted assets/72x72 of the Twemoji release: verify (or with
      --write-bin rewrite) the bundle and the file list from it first.

Rules (preserve the logical table committed in M15):
  * emoji by sort_order, each followed by its skin variations in file order;
  * the image is the lowercase unpadded unified sequence, or, without ZWJ, that
    sequence without FE0F; emoji with no such image are skipped;
  * LOOKUP holds every text plus every multi-codepoint non_qualified spelling,
    sorted by UTF-8 bytes, first entry winning.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import struct
import sys

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
SRC = HERE / "source" / "emoji-data-16.0.0.json"
NAMES = HERE / "source" / "twemoji-17.0.3-files.txt"
BIN = HERE / "twemoji-17.0.3.bin"
OUT = HERE / "emoji-index.bin"
MAGIC = b"FCEMIDX1"
HEADER_SIZE = 32
ENTRY_SIZE = 44
LOOKUP_SIZE = 12
PNG_SIG = b"\x89PNG\r\n\x1a\n"


def text(unified: str) -> str:
    return "".join(chr(int(x, 16)) for x in unified.split("-"))


def split_png(blob: bytes) -> list[int]:
    """Lengths of the PNG files concatenated in blob."""
    lens, i = [], 0
    while i < len(blob):
        assert blob[i : i + 8] == PNG_SIG, f"not a PNG at {i}"
        j = i + 8
        while True:
            n, kind = struct.unpack(">I4s", blob[j : j + 8])
            j += 12 + n
            if kind == b"IEND":
                break
        lens.append(j - i)
        i = j
    return lens


def from_twemoji(directory: Path, write: bool) -> None:
    stems = sorted(p.stem for p in directory.glob("*.png"))
    blob = b"".join((directory / f"{s}.png").read_bytes() for s in stems)
    names = "".join(s + "\n" for s in stems).encode()
    if write:
        BIN.write_bytes(blob)
        NAMES.write_bytes(names)
    elif blob != BIN.read_bytes() or names != NAMES.read_bytes():
        sys.exit("twemoji directory differs from the committed bundle or file list")


def records():
    stems = NAMES.read_text(encoding="utf8").split("\n")[:-1]
    lens = split_png(BIN.read_bytes())
    assert len(stems) == len(lens), "file list and bundle disagree"
    place, off = {}, 0
    for stem, n in zip(stems, lens):
        place[stem] = (off, n)
        off += n

    def image(unified: str):
        hexes = ["%x" % int(x, 16) for x in unified.split("-")]
        for cand in ("-".join(hexes), None if "200d" in hexes else "-".join(h for h in hexes if h != "fe0f")):
            if cand in place:
                return place[cand]
        return None

    data = json.loads(SRC.read_text(encoding="utf8"))
    data.sort(key=lambda e: e["sort_order"])
    entries, lookup = [], {}
    for e in data:
        base = len(entries)
        spellings = [(e["unified"], e["non_qualified"])] + [
            (v["unified"], v["non_qualified"]) for v in (e.get("skin_variations") or {}).values()
        ]
        for unified, non_qualified in spellings:
            pos = image(unified)
            if pos is None:
                continue
            idx = len(entries)
            if unified == e["unified"]:
                base = idx
            entries.append((text(unified), e, base, pos))
            lookup.setdefault(text(unified), idx)
            if non_qualified and "-" in non_qualified:
                lookup.setdefault(text(non_qualified), idx)

    return entries, sorted(lookup.items(), key=lambda kv: kv[0].encode())


def render() -> bytes:
    entries, lookup = records()
    assert len(entries) <= 65536, "runtime emoji IDs are u16"
    strings, offsets = bytearray(), {}

    def intern(value):
        encoded = value.encode("utf8")
        if encoded not in offsets:
            offsets[encoded] = (len(strings), len(encoded))
            strings.extend(encoded)
        return offsets[encoded]

    def record(values):
        assert all(0 <= value <= 0xffffffff for value in values), "u32 index overflow"
        return struct.pack("<" + "I" * len(values), *values)

    entry_bytes = bytearray()
    for t, e, base, (offset, length) in entries:
        aliases = " ".join(e["short_names"]) + " " + e["name"].lower()
        fields = []
        for value in (t, e["short_name"], aliases, e["category"]):
            fields.extend(intern(value))
        entry_bytes.extend(record([*fields, base, offset, length]))
    lookup_bytes = b"".join(record([*intern(text), index]) for text, index in lookup)
    assert len(entry_bytes) == len(entries) * ENTRY_SIZE
    assert len(lookup_bytes) == len(lookup) * LOOKUP_SIZE
    lookup_start = HEADER_SIZE + len(entry_bytes)
    strings_start = lookup_start + len(lookup_bytes)
    header = MAGIC + record([len(entries), len(lookup), HEADER_SIZE,
                             lookup_start, strings_start, len(strings)])
    return header + entry_bytes + lookup_bytes + strings


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--twemoji", type=Path)
    ap.add_argument("--write-bin", action="store_true")
    args = ap.parse_args()
    if args.twemoji:
        from_twemoji(args.twemoji, args.write_bin)
    new = render()
    if args.check:
        if OUT.read_bytes() != new:
            sys.exit(f"{OUT.relative_to(ROOT)} is stale: run python -I assets/emoji/generate.py")
        print("emoji index is current")
    else:
        OUT.write_bytes(new)


if __name__ == "__main__":
    main()
