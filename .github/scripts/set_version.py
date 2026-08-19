#!/usr/bin/env python3
"""Write a semver into Cargo.toml and the root package entry in Cargo.lock."""

from __future__ import annotations

import re
import sys
from pathlib import Path


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: set_version.py X.Y.Z", file=sys.stderr)
        return 2
    version = sys.argv[1].lstrip("v")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        print(f"invalid semver: {version!r} (expected X.Y.Z)", file=sys.stderr)
        return 2
    root = Path.cwd()

    toml = root / "Cargo.toml"
    text = toml.read_text()
    new, n = re.subn(
        r'^version\s*=\s*"[^"]+"',
        f'version = "{version}"',
        text,
        count=1,
        flags=re.M,
    )
    if n != 1:
        print("failed to update Cargo.toml version", file=sys.stderr)
        return 1
    toml.write_text(new)

    lock = root / "Cargo.lock"
    ltext = lock.read_text()
    lnew, n = re.subn(
        r'(name = "taskgen"\nversion = ")[^"]+',
        rf"\g<1>{version}",
        ltext,
        count=1,
    )
    if n != 1:
        print("failed to update Cargo.lock taskgen version", file=sys.stderr)
        return 1
    lock.write_text(lnew)
    print(f"set version {version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
