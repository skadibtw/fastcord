#!/usr/bin/env python3
"""Architectural audit of socket/client creation, not a security sandbox.

Run from any directory. Tests/fake sockets are explicit exceptions; production
HTTP and sockets must enter fastcord-net. New transport libraries require this
manifest to be reviewed together with the dependency graph.
"""
import re
import sys
from pathlib import Path
import tomllib

ROOT = Path(__file__).resolve().parents[1]
RAW = re.compile(r"\b(?:TcpStream|TcpSocket|UdpSocket|TcpListener|Client|ClientBuilder|TlsConnector)\s*::\s*(?:connect|bind|new|builder|default|from)\s*\(|\breqwest\s*::\s*(?:get|ClientBuilder)\s*\(|\b(?:connect_async|connect_async_tls_with_config|lookup_host)\s*\(")
# These complete source modules are cfg(test) in their parents. The fake
# service is publish=false, debug/test only and binds only 127.0.0.1.
TEST_MODULES = {"crates/fastcord-discord/src/gateway/live.rs"}
TRANSPORT_LIBS = {"hyper", "hyper-util", "socket2", "socks", "async-socks5", "hickory-resolver", "native-tls"}

def masked(source):
    # Preserve offsets/newlines while ignoring comments, string and char
    # literals; raw strings in test vectors are not treated as Rust code.
    pattern = re.compile(r'//[^\n]*|/\*[\s\S]*?\*/|r(?P<hash>\#*)"[\s\S]*?"(?P=hash)|"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])\'')
    return pattern.sub(lambda m: re.sub(r"[^\n]", " ", m.group()), source)

def test_ranges(code):
    ranges = []
    for match in re.finditer(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]", code):
        start = code.find("{", match.end())
        if start < 0: continue
        depth = 1
        end = start + 1
        while depth and end < len(code):
            depth += (code[end] == "{") - (code[end] == "}")
            end += 1
        ranges.append((match.start(), end))
    return ranges

def main():
    failures = []
    for path in (ROOT / "crates").rglob("*.rs"):
        rel = path.relative_to(ROOT).as_posix()
        if rel.startswith(("crates/fastcord-net/", "crates/fastcord-fake/")): continue
        if "/tests/" in rel or path.stem == "tests" or rel in TEST_MODULES: continue
        source = path.read_text(encoding="utf-8-sig")
        code = masked(source)
        ranges = test_ranges(code)
        for match in RAW.finditer(code):
            if not any(start <= match.start() < end for start, end in ranges):
                line = code.count("\n", 0, match.start()) + 1
                failures.append(f"{rel}:{line}: transport construction outside fastcord-net")
    for path in (ROOT / "crates").glob("*/Cargo.toml"):
        if path.parent.name in {"fastcord-net", "fastcord-fake"}: continue
        doc = tomllib.loads(path.read_text())
        deps = set(doc.get("dependencies", {}))
        for target in doc.get("target", {}).values(): deps.update(target.get("dependencies", {}))
        for dependency in sorted(deps & TRANSPORT_LIBS):
            failures.append(f"{path.relative_to(ROOT)}: unaudited network dependency {dependency}")
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print("Network boundary audit passed (production constructors; explicit test/fake exceptions).")
    return 0

if __name__ == "__main__": sys.exit(main())
