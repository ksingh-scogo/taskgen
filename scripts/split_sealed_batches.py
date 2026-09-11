"""Derive bounded, disjoint source shards from sealed review-reuse batches, offline."""

from __future__ import annotations

import argparse
from collections import Counter
from pathlib import Path

from seal_reviewed_batches import (
    canonical,
    descriptor,
    identity,
    lines,
    read_json,
    sha,
    write_files,
)


def split(
    root: Path, maximum: int, pilot_size: int = 0, followup_pilot_size: int = 0
) -> dict[str, bytes]:
    if maximum < 1 or maximum > 100:
        raise ValueError("source shard size must be between 1 and 100")
    if any(n < 0 or n > maximum for n in (pilot_size, followup_pilot_size)):
        raise ValueError("pilot size must fit within one shard")
    manifest_bytes = (root / "batch-manifest.json").read_bytes()
    manifest = read_json(manifest_bytes)
    files: dict[str, bytes] = {}
    jobs = []
    seen: set[str] = set()
    for batch in manifest["batches"]:
        parent = root / batch["sealed_run"]
        parent_bytes = (parent / "run.json").read_bytes()
        run = read_json(parent_bytes)
        if (
            run["status"] != "success"
            or run.get("operation") != "offline_repack_original_accepted_reviews"
        ):
            raise ValueError("expected a verified review-reuse source batch")
        payloads = {}
        for name, item in run["artifacts"].items():
            relative = Path(item["file"])
            if relative.is_absolute() or ".." in relative.parts:
                raise ValueError("unsafe source artifact path")
            if name == "run":
                continue
            data = (parent / relative).read_bytes()
            if sha(data) != item["sha256"] or len(data) != item["bytes"]:
                raise ValueError("source artifact hash mismatch")
            payloads[name] = data
        tables = {
            name: lines(payloads[name])[0]
            for name in ["tasks", "candidates", "reviews", "original_candidates", "review_lineage"]
        }
        count = len(tables["tasks"])
        if count != batch["rows"] or any(len(rows) != count for rows in tables.values()):
            raise ValueError("source task/review table lengths differ")
        prefixes = [n for n in (pilot_size, followup_pilot_size) if n] if not jobs else []
        ranges = []
        offset = 0
        for size in prefixes:
            if offset >= count:
                break
            ranges.append((offset, min(offset + size, count)))
            offset = min(offset + size, count)
        ranges.extend((i, min(i + maximum, count)) for i in range(offset, count, maximum))
        for index, (start, stop) in enumerate(ranges, 1):
            job_id = f"{batch['batch']}-chunk-{index:03d}"
            selected = {name: rows[start:stop] for name, rows in tables.items()}
            task_ids = [identity(row) for _, row, _ in selected["tasks"]]
            if len(task_ids) != len(set(task_ids)) or set(task_ids) & seen:
                raise ValueError("duplicate task across source shards")
            for task_id, (_, review, _), (_, lineage, _) in zip(
                task_ids, selected["reviews"], selected["review_lineage"], strict=True
            ):
                if (
                    lineage["source_task_id"] != task_id
                    or review["candidate_id"] != lineage["candidate_id"]
                    or review["final_disposition"] != "accepted"
                ):
                    raise ValueError("source review/task linkage mismatch")
            seen.update(task_ids)
            shard = {
                name: b"".join(raw + b"\n" for _, _, raw in rows) for name, rows in selected.items()
            }
            shard["rejected"] = b""
            artifacts = {name: descriptor(name + ".jsonl", data) for name, data in shard.items()}
            for name, data in shard.items():
                files[f"{job_id}/{name}.jsonl"] = data
            for name, data in payloads.items():
                if name not in tables and name != "rejected":
                    relative = run["artifacts"][name]["file"]
                    files[f"{job_id}/{relative}"] = data
                    artifacts[name] = descriptor(relative, data)
            files[f"{job_id}/parent_run.json"] = parent_bytes
            artifacts["parent_run"] = descriptor("parent_run.json", parent_bytes)
            updated = {
                **run,
                "operation": "offline_subset_of_sealed_review_batch",
                "run_id": "subset_"
                + sha(canonical({"parent": sha(parent_bytes), "ids": task_ids})),
                "input_records": len(task_ids),
                "accepted_records": len(task_ids),
                "reused_review_records": len(task_ids),
                "parent_run_sha256": sha(parent_bytes),
                "original_review_models": dict(
                    Counter(r["review_model"] for _, r, _ in selected["reviews"])
                ),
                "original_review_run_statuses": dict(
                    Counter(r["review_origin_status"] for _, r, _ in selected["review_lineage"])
                ),
                "artifacts": {**artifacts, "run": {"file": "run.json"}},
            }
            files[f"{job_id}/run.json"] = canonical(updated) + b"\n"
            jobs.append(
                {
                    "job_id": job_id,
                    "parent_batch": batch["batch"],
                    "rows": len(task_ids),
                    "source_task_ids": task_ids,
                    "run_manifest_sha256": sha(files[f"{job_id}/run.json"]),
                }
            )
    if len(seen) != manifest["total_rows"]:
        raise ValueError("source shard union differs from the complete dataset")
    files["shards.json"] = (
        canonical(
            {
                "schema_version": "scogo.taskgen.source-shards.v1",
                "parent_batch_manifest_sha256": sha(manifest_bytes),
                "total_rows": len(seen),
                "maximum_rows": maximum,
                "jobs": jobs,
            }
        )
        + b"\n"
    )
    return files


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--maximum", type=int, default=100)
    parser.add_argument("--pilot-size", type=int, default=0)
    parser.add_argument("--followup-pilot-size", type=int, default=0)
    args = parser.parse_args()
    files = split(args.root.resolve(), args.maximum, args.pilot_size, args.followup_pilot_size)
    write_files(args.output.resolve(), files)
    manifest = read_json(files["shards.json"])
    print(
        f"Prepared {len(manifest['jobs'])} source shards covering {manifest['total_rows']} unique tasks"
    )


if __name__ == "__main__":
    main()
