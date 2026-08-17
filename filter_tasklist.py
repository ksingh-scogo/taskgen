#!/usr/bin/env python3
"""Filter a Hugging Face dataset to selected technical categories.

Loads a dataset with `datasets.load_dataset`, keeps rows whose category
matches `--keep-categories` (case-insensitive; `prefix::rest` values match
on the prefix as well as the full cell), and writes UTF-8 JSONL (one JSON
object per line). Optionally pushes to the Hub.

Category matching
-----------------
* Strip surrounding whitespace, compare case-insensitively.
* A cell matches if its full normalized value **or** the token before the
  first ``::`` is in the keep-list. Original cell values are not rewritten.
* Example: ``--keep-categories Coding,CS,Conversation`` keeps
  ``coding::Python``, ``cs::Algorithms``, ``conversation::Debate``.

Gated / private datasets
------------------------
If load fails with 401/403/gated, set ``HF_TOKEN`` (or log in with
``huggingface-cli login``) and retry. Do not hardcode tokens.

Usage
-----
    python filter_tasklist.py \\
      --dataset empero-ai/tasklist-haiku4.5-6000x-unfiltered \\
      --keep-categories Coding,CS,Conversation \\
      --output ./data/tasklist-filtered
    # -> ./data/tasklist-filtered/train.jsonl (reload with load_dataset("json", ...))

    python filter_tasklist.py \\
      --dataset empero-ai/tasklist-haiku4.5-6000x-unfiltered \\
      --keep-categories Coding,CS,Conversation \\
      --category-column domain \\
      --split all \\
      --output ./data/tasklist-filtered \\
      --push-to-hub USER/REPO \\
      --private
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import sys
from collections import Counter
from pathlib import Path
from typing import Any

try:
    from datasets import Dataset, DatasetDict, load_dataset
    from datasets.features import Value
except ModuleNotFoundError:
    sys.stderr.write(
        "Missing `datasets`. Install: python3 -m pip install -r requirements.txt\n"
        "Or: python3 -m venv .venv && .venv/bin/pip install -r requirements.txt\n"
    )
    sys.exit(1)

log = logging.getLogger("filter_tasklist")

PREFERRED_CATEGORY_COLUMNS: tuple[str, ...] = (
    "category",
    "domain",
    "label",
    "labels",
    "topic",
    "type",
)

_AUTH_HINT = (
    "Dataset may be gated or private. Set HF_TOKEN (or HUGGING_FACE_HUB_TOKEN) "
    "or run `huggingface-cli login`, then retry."
)


def parse_csv_list(raw: str) -> list[str]:
    items = [part.strip() for part in raw.split(",")]
    items = [part for part in items if part]
    if not items:
        raise argparse.ArgumentTypeError("must contain at least one non-empty value")
    seen: set[str] = set()
    unique: list[str] = []
    for item in items:
        key = _norm(item)
        if key in seen:
            continue
        seen.add(key)
        unique.append(item)
    return unique


def _norm(value: object) -> str:
    return str(value).strip().lower()


def category_keys(value: object) -> set[str]:
    """Match tokens for a cell: full value plus optional `prefix` before `::`."""
    if value is None:
        return set()
    text = _norm(value)
    if not text:
        return set()
    keys = {text}
    if "::" in text:
        prefix = text.split("::", 1)[0].strip()
        if prefix:
            keys.add(prefix)
    return keys


def _is_scalar_string_feature(feature: Any) -> bool:
    if isinstance(feature, Value):
        dtype = str(getattr(feature, "dtype", "")).lower()
        return dtype in {"string", "large_string", "utf8"}
    return False


def _unique_values(dataset: Dataset, column: str) -> list[Any]:
    try:
        return list(dataset.unique(column))
    except Exception:
        seen: set[str] = set()
        values: list[Any] = []
        for value in dataset[column]:
            marker = repr(value)
            if marker in seen:
                continue
            seen.add(marker)
            values.append(value)
        return values


def detect_category_column(
    dataset: Dataset,
    keep_norm: set[str],
    override: str | None,
) -> str:
    columns = list(dataset.column_names)
    if override:
        if override not in columns:
            raise SystemExit(
                f"category column {override!r} not found. available columns: {columns}"
            )
        return override

    string_cols = [
        col
        for col in columns
        if _is_scalar_string_feature(dataset.features.get(col))
    ]
    candidates = string_cols or columns

    def score(column: str) -> tuple[int, int, int]:
        uniques = _unique_values(dataset, column)
        present: set[str] = set()
        for value in uniques:
            present |= category_keys(value)
        matched = len(keep_norm & present)
        preferred = 1 if column.lower() in PREFERRED_CATEGORY_COLUMNS else 0
        # Prefer label-like cardinality (not unique-per-row, not constant).
        n_unique = len(uniques)
        n_rows = max(len(dataset), 1)
        labelish = 1 if 1 < n_unique < n_rows else 0
        return (matched, preferred, labelish)

    ranked = sorted(candidates, key=score, reverse=True)
    if not ranked:
        raise SystemExit(f"no columns available to detect a category field: {columns}")

    chosen = ranked[0]
    best = score(chosen)
    preferred_hit = next(
        (
            col
            for col in columns
            if col.lower() in PREFERRED_CATEGORY_COLUMNS and score(col)[0] > 0
        ),
        None,
    )
    if preferred_hit and score(preferred_hit)[0] >= best[0]:
        chosen = preferred_hit
        best = score(chosen)

    if best[0] == 0:
        log.warning(
            "auto-detected %r but it matches 0 keep-categories; "
            "pass --category-column if this is wrong. candidates=%s",
            chosen,
            candidates,
        )
    else:
        log.info(
            "auto-detected category column %r (%d/%d keep-categories matched)",
            chosen,
            best[0],
            len(keep_norm),
        )
    return chosen


def hf_token() -> str | None:
    return os.environ.get("HF_TOKEN") or os.environ.get("HUGGING_FACE_HUB_TOKEN")


def _is_auth_error(exc: BaseException) -> bool:
    name = type(exc).__name__.lower()
    text = str(exc).lower()
    needles = ("gated", "401", "403", "unauthorized", "forbidden", "private")
    return "gated" in name or any(n in text for n in needles)


def load_source(dataset_id: str, split: str) -> DatasetDict:
    token = hf_token()
    kwargs: dict[str, Any] = {}
    if token:
        kwargs["token"] = token
    try:
        if split == "all":
            loaded: Dataset | DatasetDict = load_dataset(dataset_id, **kwargs)
        else:
            loaded = load_dataset(dataset_id, split=split, **kwargs)
    except Exception as exc:
        if _is_auth_error(exc):
            raise SystemExit(f"failed to load {dataset_id!r}: {exc}\n{_AUTH_HINT}") from exc
        raise SystemExit(f"failed to load {dataset_id!r}: {exc}") from exc

    if isinstance(loaded, DatasetDict):
        if not loaded:
            raise SystemExit(f"{dataset_id!r} has no splits")
        return loaded
    split_name = "train" if split == "all" else split
    return DatasetDict({split_name: loaded})


def truncate_splits(data: DatasetDict, max_rows: int | None) -> DatasetDict:
    if max_rows is None:
        return data
    if max_rows < 1:
        raise SystemExit("--max-rows must be >= 1")
    out: dict[str, Dataset] = {}
    for name, split in data.items():
        n = min(max_rows, len(split))
        out[name] = split.select(range(n)) if n < len(split) else split
        if n < len(split):
            log.info("split %r truncated %d -> %d rows (--max-rows)", name, len(split), n)
    return DatasetDict(out)


def filter_split(split: Dataset, column: str, keep_norm: set[str]) -> Dataset:
    def pred(batch: dict[str, list[Any]]) -> list[bool]:
        return [bool(category_keys(value) & keep_norm) for value in batch[column]]

    return split.filter(pred, batched=True, desc=f"filter {column}")


def matched_keep_label(value: object, keep_original: list[str], keep_norm: set[str]) -> str | None:
    keys = category_keys(value)
    hits = keys & keep_norm
    if not hits:
        return None
    for original in keep_original:
        if _norm(original) in hits:
            return original
    return next(iter(hits))


def count_keep_matches(
    split: Dataset,
    column: str,
    keep_original: list[str],
    keep_norm: set[str],
) -> Counter[str]:
    counts: Counter[str] = Counter()
    for value in split[column]:
        label = matched_keep_label(value, keep_original, keep_norm)
        if label is not None:
            counts[label] += 1
    return counts


def prefix_counts(split: Dataset, column: str) -> Counter[str]:
    counts: Counter[str] = Counter()
    for value in split[column]:
        keys = category_keys(value)
        if not keys:
            counts["<empty>"] += 1
            continue
        # Prefer the short prefix token when present.
        text = _norm(value)
        prefix = text.split("::", 1)[0].strip() if "::" in text else text
        counts[prefix or "<empty>"] += 1
    return counts


_HF_DISK_META = frozenset({"dataset_dict.json", "dataset_info.json", "state.json"})


def jsonl_paths_for_output(output: Path, split_names: list[str]) -> dict[str, Path]:
    """Map each remaining split to a .jsonl path.

    Directory (default): ``{output}/{split}.jsonl``.
    A ``.jsonl`` path is allowed only when a single split remains.
    """
    if output.suffix.lower() == ".jsonl":
        if len(split_names) != 1:
            raise SystemExit(
                f"--output {str(output)!r} is a .jsonl file but "
                f"{len(split_names)} splits remain ({split_names}). "
                "Pass a directory to write {split}.jsonl per split."
            )
        return {split_names[0]: output}
    return {name: output / f"{name}.jsonl" for name in split_names}


def clear_hf_arrow_artifacts(directory: Path) -> None:
    """Drop leftover ``save_to_disk`` Arrow/cache files so the folder is JSONL-only."""
    if not directory.exists() or not directory.is_dir():
        return
    removed = 0
    for path in sorted(directory.rglob("*"), key=lambda p: len(p.parts), reverse=True):
        if path.is_file() and (path.suffix == ".arrow" or path.name in _HF_DISK_META):
            path.unlink()
            removed += 1
        elif path.is_dir():
            try:
                path.rmdir()
            except OSError:
                pass
    if removed:
        log.info("removed %d leftover Arrow/cache files under %s", removed, directory)


def write_jsonl(dataset: Dataset, path: Path) -> int:
    path.parent.mkdir(parents=True, exist_ok=True)
    n = 0
    with path.open("w", encoding="utf-8") as fh:
        for row in dataset:
            fh.write(json.dumps(row, ensure_ascii=False) + "\n")
            n += 1
    return n


def reload_example(paths: dict[str, Path]) -> str:
    if len(paths) == 1:
        only = next(iter(paths.values()))
        data_files: str | dict[str, str] = str(only)
    else:
        data_files = {name: str(path) for name, path in paths.items()}
    return (
        'from datasets import load_dataset\n'
        f'ds = load_dataset("json", data_files={data_files!r})'
    )


def log_unmatched_keep_categories(
    data: DatasetDict,
    column: str,
    keep_original: list[str],
) -> None:
    present: set[str] = set()
    for split in data.values():
        for value in _unique_values(split, column):
            present |= category_keys(value)
    missing = [label for label in keep_original if _norm(label) not in present]
    if missing:
        log.warning(
            "keep-categories with no matches in %r: %s",
            column,
            ", ".join(missing),
        )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Filter a Hugging Face dataset to --keep-categories and write "
            "JSONL (optionally push to the Hub)."
        )
    )
    parser.add_argument(
        "--dataset",
        required=True,
        help="Hugging Face dataset id (e.g. empero-ai/tasklist-haiku4.5-6000x-unfiltered) or local path",
    )
    parser.add_argument(
        "--keep-categories",
        required=True,
        type=parse_csv_list,
        help="Comma-separated category names to keep (required). Example: Coding,CS,Conversation",
    )
    parser.add_argument(
        "--category-column",
        default=None,
        help="Column to filter on. Auto-detected if omitted (prefers domain/category/label).",
    )
    parser.add_argument(
        "--output",
        default="./data/tasklist-filtered",
        help=(
            "Directory for per-split JSONL ({split}.jsonl). If the path ends "
            "in .jsonl, write a single file (error if multiple splits remain). "
            "Default: ./data/tasklist-filtered"
        ),
    )
    parser.add_argument(
        "--split",
        default="all",
        help="Split to load, or 'all' (default) to keep every split",
    )
    parser.add_argument(
        "--push-to-hub",
        default=None,
        metavar="USER/REPO",
        help="If set, also push_to_hub to this repo id. Token from HF_TOKEN / huggingface-cli login.",
    )
    parser.add_argument(
        "--private",
        action="store_true",
        help="Create a private Hub repo when using --push-to-hub",
    )
    parser.add_argument(
        "--max-rows",
        type=int,
        default=None,
        metavar="N",
        help="Optional smoke-test cap: keep only the first N rows of each split before filtering",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(message)s")
    logging.getLogger("httpx").setLevel(logging.WARNING)
    logging.getLogger("huggingface_hub").setLevel(logging.WARNING)
    args = build_parser().parse_args(argv)

    keep_original: list[str] = args.keep_categories
    keep_norm = {_norm(label) for label in keep_original}
    log.info("dataset=%s split=%s", args.dataset, args.split)
    log.info("keep-categories=%s", keep_original)

    data = load_source(args.dataset, args.split)
    log.info("splits=%s", {name: len(split) for name, split in data.items()})

    probe_split = next(iter(data.values()))
    log.info("columns=%s", probe_split.column_names)
    column = detect_category_column(probe_split, keep_norm, args.category_column)
    for name, split in data.items():
        if column not in split.column_names:
            raise SystemExit(
                f"category column {column!r} missing from split {name!r}. "
                f"columns={split.column_names}"
            )

    log_unmatched_keep_categories(data, column, keep_original)
    data = truncate_splits(data, args.max_rows)

    original_total = 0
    kept_total = 0
    kept_by_label: Counter[str] = Counter()
    filtered: dict[str, Dataset] = {}

    for name, split in data.items():
        original_n = len(split)
        original_total += original_n
        prefixes = prefix_counts(split, column)
        log.info(
            "split %r original=%d category-prefix counts=%s",
            name,
            original_n,
            dict(prefixes.most_common()),
        )

        kept = filter_split(split, column, keep_norm)
        kept_n = len(kept)
        dropped_n = original_n - kept_n
        label_counts = count_keep_matches(kept, column, keep_original, keep_norm)
        kept_by_label.update(label_counts)
        kept_total += kept_n
        log.info(
            "split %r kept=%d dropped=%d per-category=%s",
            name,
            kept_n,
            dropped_n,
            dict(label_counts),
        )
        if kept_n == 0:
            log.info("split %r empty after filter; omitting", name)
            continue
        filtered[name] = kept

    dropped_total = original_total - kept_total
    log.info(
        "totals original=%d kept=%d dropped=%d per-category=%s",
        original_total,
        kept_total,
        dropped_total,
        dict(kept_by_label),
    )

    if kept_total == 0:
        log.error("keep-categories matched zero rows; not writing output")
        return 1

    out_dict = DatasetDict(filtered)
    output = Path(args.output).expanduser()
    paths = jsonl_paths_for_output(output, list(out_dict.keys()))
    if output.suffix.lower() != ".jsonl":
        output.mkdir(parents=True, exist_ok=True)
        clear_hf_arrow_artifacts(output)

    log.info("saving JSONL -> %s", output)
    for name, path in paths.items():
        n = write_jsonl(out_dict[name], path)
        log.info("wrote %s (%d rows, split %r)", path, n, name)
    log.info(
        "wrote %d rows across splits %s\nreload:\n%s",
        kept_total,
        list(out_dict.keys()),
        reload_example(paths),
    )

    if args.private and not args.push_to_hub:
        log.warning("--private has no effect without --push-to-hub")

    if args.push_to_hub:
        token = hf_token()
        log.info("pushing to hub %s private=%s", args.push_to_hub, args.private)
        try:
            out_dict.push_to_hub(
                args.push_to_hub,
                private=args.private,
                token=token,
            )
        except Exception as exc:
            if _is_auth_error(exc):
                raise SystemExit(f"push_to_hub failed: {exc}\n{_AUTH_HINT}") from exc
            raise SystemExit(f"push_to_hub failed: {exc}") from exc
        log.info("pushed %s", args.push_to_hub)

    return 0


if __name__ == "__main__":
    sys.exit(main())
