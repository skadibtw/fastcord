#!/usr/bin/env python3
"""Checks that AGENTS.md still describes the workspace (#185).

Every `crates/*` directory needs a `crates/<name>` row in the Key Directories
table, and the dependency block must list, for each crate, exactly its normal
(non-dev) workspace dependencies. Reads the manifests only; no network.
"""
import re
import sys
from pathlib import Path
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def workspace_deps(manifest):
    data = tomllib.loads(manifest.read_text(encoding="utf-8"))
    tables = [data.get("dependencies", {})]
    tables += [t.get("dependencies", {}) for t in data.get("target", {}).values()]
    return {
        name.removeprefix("fastcord-")
        for table in tables
        for name in table
        if name.startswith("fastcord-")
    }


def main():
    agents = (ROOT / "AGENTS.md").read_text(encoding="utf-8")
    crates = sorted(p.name for p in (ROOT / "crates").iterdir() if (p / "Cargo.toml").exists())
    problems = []
    for name in crates:
        if f"| `crates/{name}` |" not in agents:
            problems.append(f"no Key Directories row for crates/{name}")
        if agents.count(f"| `crates/{name}` |") > 1:
            problems.append(f"more than one Key Directories row for crates/{name}")

    block = agents.split("```", 2)[1]
    declared = {}
    for line in block.splitlines():
        text = re.sub(r"\(.*\)", "", line).strip()
        if not text:
            continue
        if "<-" in text:
            deps, crate = (part.strip() for part in text.split("<-"))
            declared[crate] = {d.strip() for d in deps.split(",") if d.strip()}
        else:
            declared[text] = set()
    for name in crates:
        short = name.removeprefix("fastcord-")
        actual = workspace_deps(ROOT / "crates" / name / "Cargo.toml")
        if short not in declared:
            problems.append(f"dependency block has no line for {short}")
        elif declared[short] != actual:
            problems.append(
                f"dependency block says {short} <- {sorted(declared[short])}, manifest has {sorted(actual)}"
            )
    if problems:
        sys.exit("AGENTS.md is out of date:\n" + "\n".join(problems))
    print(f"AGENTS.md matches {len(crates)} crates.")


if __name__ == "__main__":
    main()
