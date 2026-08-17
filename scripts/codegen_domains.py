#!/usr/bin/env python3
"""Codegen Rust DOMAINS from docs/it-ops-taxonomy.yaml.

Prints the static DOMAINS array. With --write, splices it into src/main.rs
between BEGIN/END GENERATED DOMAINS markers.

    python scripts/codegen_domains.py
    python scripts/codegen_domains.py --write
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

try:
    import yaml
except ImportError:
    sys.exit("PyYAML required: pip install pyyaml")

REPO = Path(__file__).resolve().parents[1]
YAML_PATH = REPO / "docs" / "it-ops-taxonomy.yaml"
MAIN_RS = REPO / "src" / "main.rs"
BEGIN = "// BEGIN GENERATED DOMAINS"
END = "// END GENERATED DOMAINS"


def rust_str(s: str) -> str:
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def load_taxonomy() -> dict:
    with YAML_PATH.open(encoding="utf-8") as fh:
        return yaml.safe_load(fh)


def rust_domains(tax: dict) -> str:
    lines = ["static DOMAINS: &[DomainDef] = &["]
    for cat in tax["categories"]:
        cid = cat["id"]
        for domain in cat["domains"]:
            subs = ", ".join(rust_str(s) for s in domain["subdomains"])
            lines.append(
                f"    DomainDef {{ category: {rust_str(cid)}, name: {rust_str(domain['name'])}, subdomains: &[{subs}] }},"
            )
    lines.append("];")
    return "\n".join(lines)


def stats(tax: dict) -> tuple[int, int, list[str]]:
    n_dom = 0
    n_sub = 0
    ids = []
    for cat in tax["categories"]:
        ids.append(cat["id"])
        n_dom += len(cat["domains"])
        n_sub += sum(len(d["subdomains"]) for d in cat["domains"])
    return n_dom, n_sub, ids


def splice(block: str) -> None:
    text = MAIN_RS.read_text(encoding="utf-8")
    start = text.find(BEGIN)
    end = text.find(END)
    if start < 0 or end < 0 or end <= start:
        sys.exit(f"missing {BEGIN} / {END} markers in {MAIN_RS}")
    end += len(END)
    replacement = f"{BEGIN}\n{block}\n{END}"
    MAIN_RS.write_text(text[:start] + replacement + text[end:], encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true", help=f"splice into {MAIN_RS}")
    args = parser.parse_args()

    tax = load_taxonomy()
    n_dom, n_sub, ids = stats(tax)
    block = rust_domains(tax)
    print(f"{len(ids)} categories, {n_dom} domains, {n_sub} subdomains", file=sys.stderr)
    print("categories: " + ", ".join(ids), file=sys.stderr)

    if args.write:
        splice(block)
        print(f"wrote DOMAINS into {MAIN_RS}", file=sys.stderr)
    else:
        print(block)


if __name__ == "__main__":
    main()
