#!/usr/bin/env python3
"""Set the release version across every file that carries it, so a single
semantic-release run keeps package.json, the Tauri config, and the Rust crate
(plus its lockfile) in lockstep.

Usage: python3 scripts/set-version.py <version>   e.g. 1.4.0
Called by semantic-release (@semantic-release/exec prepareCmd).
"""

import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent


def set_json_version(path, version):
    p = ROOT / path
    data = json.loads(p.read_text())
    data["version"] = version
    p.write_text(json.dumps(data, indent=2) + "\n")
    print(f"{path}: version -> {version}")


def sub_once(path, pattern, repl, label):
    p = ROOT / path
    text = p.read_text()
    new, n = re.subn(pattern, repl, text, count=1)
    if n != 1:
        sys.exit(f"set-version: expected exactly one match for {label} in {path}, got {n}")
    p.write_text(new)
    print(f"{path}: {label} -> updated")


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: set-version.py <version>")
    version = sys.argv[1].strip()
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:[-+].+)?", version):
        sys.exit(f"set-version: not a valid semver: {version!r}")

    # package.json + Tauri config are plain JSON.
    set_json_version("package.json", version)
    set_json_version("src-tauri/tauri.conf.json", version)

    # Cargo.toml: the version line inside the [package] table only.
    sub_once(
        "src-tauri/Cargo.toml",
        r'(\[package\][\s\S]*?\nversion = ")[^"]*(")',
        lambda m: m.group(1) + version + m.group(2),
        "[package] version",
    )

    # Cargo.lock: the version line inside the hacksor package block, so the
    # committed lockfile matches Cargo.toml.
    sub_once(
        "src-tauri/Cargo.lock",
        r'(name = "hacksor"\nversion = ")[^"]*(")',
        lambda m: m.group(1) + version + m.group(2),
        "hacksor lock entry",
    )


if __name__ == "__main__":
    main()
