#!/usr/bin/env python3
"""Pick the next taskgen release version from Cargo.toml + GitHub releases."""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path


SEMVER = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")


def parse(v: str) -> tuple[int, int, int]:
    v = v.strip().lstrip("v")
    if not SEMVER.fullmatch(v):
        raise SystemExit(f"invalid semver: {v!r} (expected X.Y.Z)")
    a, b, c = (int(p) for p in v.split("."))
    return a, b, c


def fmt(t: tuple[int, int, int]) -> str:
    return f"{t[0]}.{t[1]}.{t[2]}"


def cargo_version(root: Path) -> str:
    text = (root / "Cargo.toml").read_text()
    m = re.search(r'^version\s*=\s*"([^"]+)"', text, re.M)
    if not m:
        raise SystemExit("could not read version from Cargo.toml")
    version = m.group(1)
    parse(version)
    return version


def latest_release_tag() -> str | None:
    r = subprocess.run(
        ["gh", "release", "list", "--limit", "50", "--json", "tagName,isPrerelease,isDraft"],
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        err = (r.stderr or r.stdout or "").strip()
        raise SystemExit(f"gh release list failed: {err}")
    try:
        releases = json.loads(r.stdout or "[]")
    except json.JSONDecodeError as e:
        raise SystemExit(f"gh release list returned invalid JSON: {e}") from e
    for rel in releases:
        if rel.get("isPrerelease") or rel.get("isDraft"):
            continue
        tag = (rel.get("tagName") or "").strip()
        if tag:
            return tag.lstrip("v")
    return None


def tag_commit(tag: str) -> str | None:
    r = subprocess.run(
        ["git", "rev-list", "-n", "1", tag],
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        return None
    return (r.stdout or "").strip() or None


def bump(v: str, kind: str) -> str:
    a, b, c = parse(v)
    if kind == "major":
        return fmt((a + 1, 0, 0))
    if kind == "minor":
        return fmt((a, b + 1, 0))
    return fmt((a, b, c + 1))


def write_output(pairs: dict[str, str]) -> None:
    out = os.environ.get("GITHUB_OUTPUT")
    if not out:
        for k, v in pairs.items():
            print(f"{k}={v}")
        return
    with open(out, "a", encoding="utf-8") as fh:
        for k, v in pairs.items():
            fh.write(f"{k}={v}\n")


def main() -> int:
    root = Path.cwd()
    cargo = cargo_version(root)
    latest = latest_release_tag()
    override = (os.environ.get("VERSION_OVERRIDE") or "").strip().lstrip("v")
    kind = (os.environ.get("BUMP") or "patch").strip().lower()
    head = (os.environ.get("GITHUB_SHA") or "").strip()
    if not head:
        r = subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True)
        head = (r.stdout or "").strip()

    if override:
        parse(override)
        version = override
    elif latest is None:
        version = cargo
    elif parse(cargo) > parse(latest):
        version = cargo
    else:
        version = bump(latest, kind)

    parse(version)  # validate
    tag = f"v{version}"
    skip = "false"
    # Re-running CI on a SHA that already is the latest release must not bump again.
    if not override and latest:
        tagged_latest = tag_commit(f"v{latest}") or tag_commit(latest)
        if tagged_latest and head and tagged_latest == head:
            skip = "true"
            version = latest
            tag = f"v{version}"

    write_output(
        {
            "version": version,
            "tag": tag,
            "skip": skip,
            "latest": latest or "",
            "cargo": cargo,
        }
    )
    print(
        f"cargo={cargo} latest={latest or '(none)'} next={version} skip={skip}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
