#!/usr/bin/env python3
"""Summarize a fastcord-fake request log (fake-requests.jsonl). Python 3 stdlib only.

    python scripts/fake_traffic.py fake-requests.jsonl [--burst-n 3]
        [--start-ms N] [--end-ms N] [--allow FILE]

Prints requests per route template (ids collapsed), bursts of more than N
requests in any 1 s window, UNIMPLEMENTED routes, 429s with the back-off seen
after them, and the Identify count. Exit status is 1 when a burst, an
UNIMPLEMENTED route or a write outside the --allow list (one "METHOD path"
template per line) is found, so a change can be gated on it.
"""
import argparse
import json
import re
import sys
from collections import Counter, defaultdict

ID = re.compile(r"\d{15,21}")
PREFIX = re.compile(r"^/api/v\d+")


def template(entry):
    """`kind METHOD /path` with ids, invite codes and emoji collapsed."""
    path = PREFIX.sub("", entry.get("path") or "/")
    path = ID.sub("{id}", path)
    path = re.sub(r"(/invites/)[^/?]+", r"\1{code}", path)
    path = re.sub(r"(/reactions/)[^/]+", r"\1{emoji}", path)
    return f"{entry.get('kind')} {entry.get('method')} {path}"


def load(path, start_ms, end_ms):
    entries = []
    with open(path, encoding="utf-8") as lines:
        for line in lines:
            try:
                entry = json.loads(line)
            except ValueError:
                continue
            if isinstance(entry, dict) and isinstance(entry.get("ts_ms"), int):
                entries.append(entry)
    entries.sort(key=lambda e: e["ts_ms"])
    if not entries:
        return entries, 0
    origin = entries[0]["ts_ms"]
    window = [
        e for e in entries
        if (start_ms is None or e["ts_ms"] - origin >= start_ms)
        and (end_ms is None or e["ts_ms"] - origin <= end_ms)
    ]
    return window, origin


def bursts(times, limit):
    """(window start, count) for the busiest 1 s window when it exceeds limit."""
    best = (0, 0)
    low = 0
    for high, now in enumerate(times):
        while now - times[low] >= 1000:
            low += 1
        if high - low + 1 > best[1]:
            best = (times[low], high - low + 1)
    return best if best[1] > limit else None


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("log")
    parser.add_argument("--burst-n", type=int, default=3)
    parser.add_argument("--start-ms", type=int)
    parser.add_argument("--end-ms", type=int)
    parser.add_argument("--allow", help="file of allowed write templates, e.g. 'http POST /channels/{id}/messages'")
    args = parser.parse_args()

    entries, origin = load(args.log, args.start_ms, args.end_ms)
    if not entries:
        print("no requests in range")
        return 0
    keys = defaultdict(list)
    status = defaultdict(Counter)
    notes = Counter()
    for entry in entries:
        key = template(entry)
        keys[key].append(entry["ts_ms"])
        status[key][entry.get("status")] += 1
        if entry.get("note"):
            notes[(key, entry["note"])] += 1

    print(f"{'count':>5}  {'first':>7}  {'last':>7}  route (status counts)")
    for key, times in sorted(keys.items(), key=lambda item: (-len(item[1]), item[0])):
        counts = ", ".join(f"{code}x{n}" for code, n in sorted(status[key].items(), key=str))
        print(f"{len(times):>5}  {times[0] - origin:>6}ms {times[-1] - origin:>6}ms  {key} ({counts})")

    failed = False
    print(f"\nbursts (more than {args.burst_n} in any 1 s):")
    http = sorted(e["ts_ms"] for e in entries if e.get("kind") == "http")
    found = []
    for key, times in [("all http", http)] + list(keys.items()):
        burst = bursts(times, args.burst_n)
        if burst:
            found.append(f"  {key}: {burst[1]} at +{burst[0] - origin}ms")
    print("\n".join(found) if found else "  none")
    failed |= bool(found)

    print("\nUNIMPLEMENTED:")
    missing = sorted(
        ((count, key) for (key, note), count in notes.items() if note == "UNIMPLEMENTED"),
        reverse=True,
    )
    print("\n".join(f"  {count:>4}  {key}" for count, key in missing) if missing else "  none")
    failed |= bool(missing)

    print("\n429s and back-off:")
    limited = [e for e in entries if e.get("note") == "RATE_LIMITED" or e.get("status") == 429]
    if not limited:
        print("  none")
    for key in sorted({template(e) for e in limited}):
        hits = [e["ts_ms"] for e in limited if template(e) == key]
        after = sum(1 for t in keys[key] for h in hits if h < t <= h + 2000)
        print(f"  {key}: {len(hits)} x 429, {after} request(s) of that route within 2 s after a 429")

    identifies = sum(1 for e in entries if e.get("method") == "OP2")
    resumes = sum(1 for e in entries if e.get("method") == "OP6")
    private = sum(1 for e in entries if e.get("path", "").endswith("/users/@me/channels") and e.get("method") == "GET")
    print(f"\nIdentify (OP2): {identifies}   Resume (OP6): {resumes}   GET /users/@me/channels: {private}")

    if args.allow:
        with open(args.allow, encoding="utf-8") as allowed:
            allow = {line.strip() for line in allowed if line.strip() and not line.startswith("#")}
        outside = sorted(
            key for key in keys
            if key.startswith("http ") and key.split()[1] in ("POST", "PUT", "PATCH", "DELETE") and key not in allow
        )
        print("\nwrites outside the allow list:")
        print("\n".join(f"  {key}" for key in outside) if outside else "  none")
        failed |= bool(outside)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
