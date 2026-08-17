#!/usr/bin/env python3
"""Codegen Rust DOMAINS and DEFAULT_DISTRIBUTION from docs/it-ops-taxonomy.yaml.

Prints the generated blocks. With --write, splices them into src/main.rs
between BEGIN/END GENERATED DOMAINS and BEGIN/END GENERATED DISTRIBUTION.

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
BEGIN_DIST = "// BEGIN GENERATED DISTRIBUTION"
END_DIST = "// END GENERATED DISTRIBUTION"


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


def rust_distribution(tax: dict) -> str:
    lines = ["const DEFAULT_DISTRIBUTION: &[(&str, f64)] = &["]
    for key, weight in tax["default_distribution"].items():
        lines.append(f'    ({rust_str(str(key))}, {float(weight):.2f}),')
    lines.append("];")
    return "\n".join(lines)


def splice_marked(text: str, begin: str, end: str, block: str) -> str:
    start = text.find(begin)
    stop = text.find(end)
    if start < 0 or stop < 0 or stop <= start:
        sys.exit(f"missing {begin} / {end} markers in {MAIN_RS}")
    stop += len(end)
    return text[:start] + f"{begin}\n{block}\n{end}" + text[stop:]


def splice(domains: str, distribution: str) -> None:
    text = MAIN_RS.read_text(encoding="utf-8")
    text = splice_marked(text, BEGIN, END, domains)
    text = splice_marked(text, BEGIN_DIST, END_DIST, distribution)
    MAIN_RS.write_text(text, encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true", help=f"splice into {MAIN_RS}")
    args = parser.parse_args()

    tax = load_taxonomy()
    n_dom, n_sub, ids = stats(tax)
    domains = rust_domains(tax)
    distribution = rust_distribution(tax)
    print(f"{len(ids)} categories, {n_dom} domains, {n_sub} subdomains", file=sys.stderr)
    print("categories: " + ", ".join(ids), file=sys.stderr)

    if args.write:
        splice(domains, distribution)
        print(f"wrote DOMAINS + DEFAULT_DISTRIBUTION into {MAIN_RS}", file=sys.stderr)
    else:
        print(domains)
        print()
        print(distribution)


if __name__ == "__main__":
    main()
