#!/usr/bin/env python3
"""Regenerate docs/THIRD_PARTY_CRATES.md from Cargo.lock.

Lists every third-party crate that can be compiled into a release binary
(normal and build dependencies of the workspace for the four release targets
of .github/workflows/release.yml) with its version, SPDX license expression
and source, then reproduces each distinct license/notice text found in the
crates' sources once, naming the crates it belongs to. A crate that publishes
no license file is covered by the standard text of the license its manifest
declares (MIT preferred), credited to the authors named in its manifest.

Run it after any Cargo.lock change, with all sources fetched:

    cargo fetch --locked
    python scripts/gen_third_party_licenses.py          # rewrite the file
    python scripts/gen_third_party_licenses.py --check  # exit 1 when stale
"""
import hashlib
import json
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "docs" / "THIRD_PARTY_CRATES.md"
TARGETS = (
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
)
NAMES = ("licen", "copying", "copyright", "unlicense", "notice")
SKIP_SUFFIXES = (".rs", ".toml", ".json", ".yml", ".yaml")
PREFERENCE = ("MIT", "Apache-2.0", "Zlib", "BSD-3-Clause", "BSD-2-Clause", "ISC")

MIT = """MIT License

Copyright (c) {holders}

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE."""

# Marker that identifies the standard text of a license inside a shipped file.
MARKERS = {
    "Apache-2.0": ("Version 2.0, January 2004", "TERMS AND CONDITIONS FOR USE"),
    "MPL-2.0": ("Mozilla Public License Version 2.0", "Exhibit A"),
    "BSL-1.0": ("Boost Software License - Version 1.0",),
    "CC0-1.0": ("CC0 1.0 Universal", "Statement of Purpose"),
}


def metadata(target):
    raw = subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", target],
        cwd=ROOT,
        check=True,
        capture_output=True,
    ).stdout
    return json.loads(raw)


def shipped(meta):
    """Packages reachable from the workspace through normal/build edges."""
    nodes = {node["id"]: node for node in meta["resolve"]["nodes"]}
    seen = set()
    todo = list(meta["workspace_members"])
    while todo:
        package = todo.pop()
        if package in seen:
            continue
        seen.add(package)
        for dep in nodes[package]["deps"]:
            if any(kind["kind"] in (None, "build") for kind in dep["dep_kinds"]):
                todo.append(dep["pkg"])
    workspace = set(meta["workspace_members"])
    return [p for p in meta["packages"] if p["id"] in seen and p["id"] not in workspace]


def license_files(package):
    directory = pathlib.Path(package["manifest_path"]).parent
    found = []
    for path in sorted(directory.iterdir()):
        name = path.name.lower()
        if (
            path.is_file()
            and name.startswith(NAMES)
            and not name.endswith(SKIP_SUFFIXES)
        ):
            text = path.read_bytes().decode("utf-8", errors="replace")
            text = text.replace("\r\n", "\n").strip("\n")
            if text.strip():
                found.append(text)
    return found


def source(package):
    if package["source"] is None:
        return "path"
    if package["source"].startswith("registry+"):
        return "crates.io"
    return package["source"].split("+", 1)[-1].split("#", 1)[0]


def declared(package):
    expression = package["license"] or ""
    return [part for part in re.split(r"[\s/()]+|\bOR\b|\bAND\b|\bWITH\b", expression) if part]


def fallback(package, corpus):
    """(key, text) covering a crate that ships no license file."""
    options = declared(package)
    if "MIT" in options:
        holders = ", ".join(
            re.sub(r"\s*<[^>]*>", "", author) for author in package["authors"]
        ) or f"the {package['name']} authors"
        return ("MIT", holders), MIT.format(holders=holders)
    for spdx in options:
        if spdx in corpus:
            return (spdx, ""), corpus[spdx]
    spdx = options[0] if options else "UNKNOWN"
    return (spdx, ""), f"License {spdx}: see https://spdx.org/licenses/{spdx}.html"


def render():
    by_id = {}
    for target in TARGETS:
        meta = metadata(target)
        for package in shipped(meta):
            by_id[package["id"]] = package
    packages = sorted(by_id.values(), key=lambda p: (p["name"], p["version"]))

    texts = {}
    unlicensed = []
    # Standard texts for licenses that no crate source ships in full.
    corpus = {
        "MPL-2.0": (ROOT / "scripts" / "licenses" / "MPL-2.0.txt")
        .read_text(encoding="utf-8")
        .replace("\r\n", "\n")
        .strip("\n")
    }
    for package in packages:
        files = license_files(package)
        if not files:
            unlicensed.append(package)
        for text in files:
            digest = hashlib.sha256(text.encode()).hexdigest()
            texts.setdefault(digest, (text, []))[1].append(package)
            for spdx, marker in MARKERS.items():
                if all(part in text for part in marker) and spdx not in corpus:
                    corpus[spdx] = text
    for package in unlicensed:
        key, text = fallback(package, corpus)
        digest = hashlib.sha256(repr(key).encode() + text.encode()).hexdigest()
        texts.setdefault(digest, (text, []))[1].append(package)

    lines = [
        "# Third-party Rust crates",
        "",
        "<!-- Generated by scripts/gen_third_party_licenses.py from Cargo.lock; do not edit. -->",
        "",
        "Every Rust crate that fastcord release binaries can contain (normal and build",
        "dependencies for the Windows, Linux and macOS release targets), with the license",
        "text each crate ships. Native components that are not plain Rust crates (libopus,",
        "dav1d, RNNoise, the DeepFilterNet model, davey and OpenMLS) are also covered in",
        "`THIRD_PARTY_NOTICES.md`. Where a crate offers a choice of licenses, any one of the",
        "offered licenses applies. A crate that publishes no license file is covered by the",
        "standard text of its declared license, credited to the authors in its manifest.",
        "",
        f"## Crates ({len(packages)})",
        "",
        "| Crate | Version | License | Source |",
        "| --- | --- | --- | --- |",
    ]
    for package in packages:
        license_name = (package["license"] or "see crate").replace("|", "\\|")
        lines.append(
            f"| {package['name']} | {package['version']} | {license_name} | {source(package)} |"
        )

    lines += ["", f"## License texts ({len(texts)})", ""]
    for index, (text, users) in enumerate(
        sorted(texts.values(), key=lambda item: (item[1][0]["name"], item[0])), 1
    ):
        names = ", ".join(sorted({f"{p['name']} {p['version']}" for p in users}))
        fence = "````" if "```" in text else "```"
        lines += [f"### {index}. Used by: {names}", "", f"{fence}text", text, fence, ""]
    return "\n".join(lines).rstrip("\n") + "\n"


def main():
    rendered = render()
    if "--check" in sys.argv:
        current = OUT.read_text(encoding="utf-8") if OUT.exists() else ""
        if current != rendered:
            print(f"{OUT.name} is stale; run scripts/gen_third_party_licenses.py")
            sys.exit(1)
        return
    OUT.write_text(rendered, encoding="utf-8", newline="\n")


if __name__ == "__main__":
    main()
