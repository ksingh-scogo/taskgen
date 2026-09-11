"""Offline repackaging of historically reviewed tasks; never creates review decisions.

Run with Python 3.11+ and jsonschema. Original files stay unchanged. An accepted
review from an interrupted ancestor is reusable only when a hash-verified,
successful descendant published the exact task. Truncated trailing journal rows
are retained as an origin caveat and are never used as review evidence.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import tempfile
from collections import Counter
from pathlib import Path
from typing import Any

from jsonschema import Draft202012Validator


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def canonical(value: Any) -> bytes:
    return json.dumps(
        value, sort_keys=True, ensure_ascii=False, separators=(",", ":"), allow_nan=False
    ).encode()


def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON key")
        result[key] = value
    return result


def read_json(data: bytes) -> Any:
    return json.loads(
        data,
        object_pairs_hook=unique_object,
        parse_constant=lambda _: (_ for _ in ()).throw(ValueError("nonfinite JSON")),
    )


def identity(task: dict[str, Any]) -> str:
    keys = ("schema_version", "prompt", "category", "domain", "subdomain", "difficulty")
    value = {key: task.get(key) for key in keys}
    value["coordinates"] = task.get("coordinates", {})
    return "task_" + sha(canonical(value))


def lines(
    data: bytes, *, allow_truncated_tail: bool = False
) -> tuple[list[tuple[int, dict, bytes]], int]:
    result = []
    raw_lines = data.splitlines()
    ignored = 0
    for number, raw in enumerate(raw_lines, 1):
        if not raw.strip():
            raise ValueError("blank JSONL row")
        try:
            row = read_json(raw)
        except (ValueError, UnicodeError):
            if allow_truncated_tail and number == len(raw_lines) and not data.endswith(b"\n"):
                ignored += 1
                continue
            raise
        if not isinstance(row, dict):
            raise ValueError("JSONL row must be an object")
        result.append((number, row, raw))
    return result, ignored


def descriptor(file: str, data: bytes) -> dict[str, Any]:
    return {"file": file, "sha256": sha(data), "bytes": len(data)}


def gather_origins(taskgen: Path, tasks: dict[str, dict]) -> tuple[dict, dict, dict]:
    origins = {}
    published: dict[str, list[dict]] = {}
    reviews: dict[str, list[dict]] = {}
    review_schema = Draft202012Validator(
        read_json((taskgen / "schemas/prompt-review-v3.schema.json").read_bytes())
    )
    for directory in sorted([*(taskgen / "runs").glob("*"), *(taskgen / "pilot-runs").glob("*")]):
        path = directory / "run.json"
        if not path.is_file():
            continue
        manifest_bytes = path.read_bytes()
        manifest = read_json(manifest_bytes)
        if manifest.get("schema_version") != "scogo.taskgen.run.v3":
            continue
        run_id = manifest["run_id"]
        if run_id in origins:
            raise ValueError("duplicate historical run ID")
        origin = {
            "manifest": manifest,
            "manifest_bytes": manifest_bytes,
            "directory": directory,
            "files": {},
            "file_digests": {},
            "rows": {},
            "truncated_tails": {},
        }
        origins[run_id] = origin
        for name in ("tasks", "candidates", "reviews"):
            file = directory / f"{name}.jsonl"
            if not file.is_file():
                origin["rows"][name] = []
                continue
            if file.is_symlink():
                raise ValueError("historical evidence must be regular files, not symlinks")
            data = file.read_bytes()
            declared = manifest.get("artifacts", {}).get(name)
            if declared:
                if (
                    declared.get("file") != file.name
                    or declared.get("sha256") != sha(data)
                    or declared.get("bytes") != len(data)
                ):
                    raise ValueError(f"historical artifact hash/size mismatch: {run_id}/{name}")
            elif manifest.get("status") == "success":
                raise ValueError("successful publication lacks an artifact hash")
            parsed, truncated = lines(
                data, allow_truncated_tail=not declared and manifest.get("status") != "success"
            )
            origin["files"][name] = data
            origin["file_digests"][name] = sha(data)
            origin["rows"][name] = parsed
            if truncated:
                origin["truncated_tails"][name] = truncated
        if manifest.get("status") == "success":
            for number, row, raw in origin["rows"]["tasks"]:
                task_id = identity(row)
                if task_id in tasks and row == tasks[task_id]:
                    published.setdefault(task_id, []).append(
                        {"run_id": run_id, "line": number, "line_sha256": sha(raw)}
                    )
        candidates = {}
        for number, row, raw in origin["rows"]["candidates"]:
            if "candidate_id" not in row:
                continue
            candidate_id = row["candidate_id"]
            if candidate_id in candidates:
                raise ValueError("duplicate historical candidate ID")
            candidates[candidate_id] = (number, row, raw)
        for number, review, raw in origin["rows"]["reviews"]:
            if review.get("final_disposition") != "accepted":
                continue
            if review.get("schema_version") != "scogo.taskgen.review-record.v3":
                raise ValueError("accepted review has an unsupported schema")
            linked = candidates.get(review["candidate_id"])
            if linked is None:
                raise ValueError("accepted review has no source candidate")
            candidate_number, candidate, candidate_raw = linked
            task = candidate["candidate"]
            task_id = identity(task)
            if task_id not in tasks or task != tasks[task_id]:
                continue
            expected_id = sha(
                int(candidate["sequence"]).to_bytes(8, "little") + task["prompt"].encode()
            )
            if (
                review["candidate_id"] != expected_id
                or review["sequence"] != candidate["sequence"]
                or candidate["prompt_sha256"] != sha(task["prompt"].encode())
            ):
                raise ValueError("historical candidate/review identity mismatch")
            review_schema.validate(review["decision"])
            reviews.setdefault(task_id, []).append(
                {
                    "run_id": run_id,
                    "review": review,
                    "review_bytes": raw,
                    "review_line": number,
                    "candidate_bytes": candidate_raw,
                    "candidate_line": candidate_number,
                }
            )
    for task_id in tasks:
        if task_id not in published or task_id not in reviews:
            raise ValueError(
                f"task lacks original accepted review or successful publication: {task_id}"
            )
    return origins, published, reviews


def build(plan_root: Path, taskgen: Path, source_root: Path) -> dict[str, bytes]:
    plan_bytes = (plan_root / "batch-manifest.json").read_bytes()
    manifest = read_json(plan_bytes)
    task_schema = Draft202012Validator(
        read_json((taskgen / "schemas/task-v2.schema.json").read_bytes())
    )
    source_tasks = {}
    for item in manifest["source"]["files"]:
        file = Path(item["file"])
        if file.is_absolute() or ".." in file.parts:
            raise ValueError("invalid source file path")
        data = (source_root / file).read_bytes()
        if sha(data) != item["sha256"] or len(data) != item["bytes"]:
            raise ValueError("pinned source changed")
        parsed, _ = lines(data)
        if len(parsed) != item["rows"]:
            raise ValueError("source row count mismatch")
        for _, row, _ in parsed:
            task_schema.validate(row)
            key = identity(row)
            if key in source_tasks:
                raise ValueError("duplicate source task identity")
            source_tasks[key] = row
    if len(source_tasks) != manifest["total_rows"]:
        raise ValueError("source total mismatch")
    origins, publications, reviews = gather_origins(taskgen, source_tasks)
    files = {
        "batch-manifest.json": plan_bytes,
        "assignments.jsonl": (plan_root / "assignments.jsonl").read_bytes(),
    }
    if sha(files["assignments.jsonl"]) != manifest["assignment_sha256"]:
        raise ValueError("assignment file hash mismatch")
    assignment_rows, _ = lines(files["assignments.jsonl"])
    assignments = {row["source_task_id"]: row["batch"] for _, row, _ in assignment_rows}
    if len(assignments) != len(assignment_rows) or set(assignments) != set(source_tasks):
        raise ValueError("assignments do not cover each source task exactly once")
    seen = set()
    summary = []
    for batch in manifest["batches"]:
        batch_name = batch["batch"]
        if (
            batch["input"] != f"inputs/{batch_name}.jsonl"
            or batch["sealed_run"] != f"sealed/{batch_name}"
        ):
            raise ValueError("unexpected batch paths")
        input_bytes = (plan_root / batch["input"]).read_bytes()
        if sha(input_bytes) != batch["sha256"] or len(input_bytes) != batch["bytes"]:
            raise ValueError("batch input changed")
        input_rows, _ = lines(input_bytes)
        if len(input_rows) != batch["rows"]:
            raise ValueError("batch count mismatch")
        files[batch["input"]] = input_bytes
        evidence = []
        review_lines = []
        candidate_lines = []
        used_origins = set()
        models = Counter()
        origin_statuses = Counter()
        for _, task, _ in input_rows:
            task_id = identity(task)
            if (
                task_id in seen
                or source_tasks.get(task_id) != task
                or assignments.get(task_id) != batch_name
            ):
                raise ValueError("batch task changed or is assigned more than once")
            seen.add(task_id)
            chosen = sorted(
                reviews[task_id],
                key=lambda row: (
                    origins[row["run_id"]]["manifest"].get("status") != "success",
                    row["run_id"],
                ),
            )[0]
            publication = sorted(publications[task_id], key=lambda row: row["run_id"])[0]
            origin = origins[chosen["run_id"]]
            used_origins |= {chosen["run_id"], publication["run_id"]}
            review_lines.append(chosen["review_bytes"])
            candidate_lines.append(chosen["candidate_bytes"])
            models[chosen["review"]["review_model"]] += 1
            origin_statuses[origin["manifest"]["status"]] += 1
            evidence.append(
                {
                    "source_task_id": task_id,
                    "candidate_id": chosen["review"]["candidate_id"],
                    "review_origin_run_id": chosen["run_id"],
                    "review_origin_status": origin["manifest"]["status"],
                    "review_origin_file_sha256": origin["file_digests"]["reviews"],
                    "review_origin_line": chosen["review_line"],
                    "review_line_sha256": sha(chosen["review_bytes"]),
                    "candidate_origin_file_sha256": origin["file_digests"]["candidates"],
                    "candidate_origin_line": chosen["candidate_line"],
                    "candidate_line_sha256": sha(chosen["candidate_bytes"]),
                    "publication": {
                        **publication,
                        "tasks_file_sha256": origins[publication["run_id"]]["file_digests"][
                            "tasks"
                        ],
                    },
                }
            )
        payloads = {
            "tasks": input_bytes,
            "candidates": input_bytes,
            "reviews": b"\n".join(review_lines) + b"\n",
            "rejected": b"",
            "original_candidates": b"\n".join(candidate_lines) + b"\n",
            "review_lineage": b"".join(canonical(row) + b"\n" for row in evidence),
        }
        artifacts = {name: descriptor(name + ".jsonl", data) for name, data in payloads.items()}
        for name, data in payloads.items():
            files[f"{batch['sealed_run']}/{name}.jsonl"] = data
        origin_inventory = []
        for run_id in sorted(used_origins):
            origin = origins[run_id]
            original = origin["manifest"]
            origin_inventory.append(
                {
                    "run_id": run_id,
                    "status": original["status"],
                    "manifest_sha256": sha(origin["manifest_bytes"]),
                    "source_files": {
                        name: descriptor(name + ".jsonl", data)
                        for name, data in origin["files"].items()
                    },
                    "ignored_truncated_trailing_rows": origin["truncated_tails"],
                }
            )
            name = f"origin_manifest_{run_id}"
            file = f"origins/{run_id}.json"
            artifacts[name] = descriptor(file, origin["manifest_bytes"])
            files[f"{batch['sealed_run']}/{file}"] = origin["manifest_bytes"]
        run_id = "repacked_" + sha(
            canonical(
                {
                    "plan": sha(plan_bytes),
                    "batch": batch_name,
                    "reviews": sha(payloads["reviews"]),
                    "origins": origin_inventory,
                }
            )
        )
        run = {
            "schema_version": "scogo.taskgen.run.v3",
            "run_id": run_id,
            "status": "success",
            "operation": "offline_repack_original_accepted_reviews",
            "taxonomy_id": "scogo-enterprise-netops-v2",
            "input_records": len(input_rows),
            "accepted_records": len(input_rows),
            "rejected_records": 0,
            "review_errors": 0,
            "fresh_model_calls": 0,
            "reused_review_records": len(review_lines),
            "batch_manifest_sha256": sha(plan_bytes),
            "original_review_models": dict(models),
            "original_review_run_statuses": dict(origin_statuses),
            "original_runs": origin_inventory,
            "artifacts": {**artifacts, "run": {"file": "run.json"}},
        }
        files[f"{batch['sealed_run']}/run.json"] = canonical(run) + b"\n"
        summary.append(
            {
                "batch": batch_name,
                "rows": len(input_rows),
                "run_id": run_id,
                "run_manifest_sha256": sha(files[f"{batch['sealed_run']}/run.json"]),
                "review_models": dict(models),
                "review_origin_statuses": dict(origin_statuses),
            }
        )
    if seen != set(source_tasks):
        raise ValueError("batch union does not equal source")
    report = {
        "schema_version": "scogo.taskgen.review-reuse-report.v1",
        "source_repo_id": manifest["source"]["repo_id"],
        "source_revision": manifest["source"]["revision"],
        "rows": len(seen),
        "parts": len(summary),
        "new_model_calls": 0,
        "overlapping_rows": 0,
        "missing_original_reviews": 0,
        "batches": summary,
    }
    files["review-reuse-report.json"] = canonical(report) + b"\n"
    # Recheck the historical bytes after assembling the complete output.
    for origin in origins.values():
        if (origin["directory"] / "run.json").read_bytes() != origin["manifest_bytes"]:
            raise ValueError("historical manifest changed during repackaging")
        for name, data in origin["files"].items():
            if (origin["directory"] / f"{name}.jsonl").read_bytes() != data:
                raise ValueError("historical evidence changed during repackaging")
    return files


def write_files(output: Path, files: dict[str, bytes]) -> None:
    if output.exists():
        for name, payload in files.items():
            if (output / name).read_bytes() != payload:
                raise ValueError("existing repackaged evidence differs")
        return
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".review-repack-", dir=output.parent) as temporary:
        stage = Path(temporary) / "batches"
        stage.mkdir(mode=0o700)
        for name, payload in files.items():
            path = stage / name
            path.parent.mkdir(parents=True, exist_ok=True)
            with path.open("xb") as handle:
                handle.write(payload)
                handle.flush()
                os.fsync(handle.fileno())
        stage.rename(output)


def main() -> None:
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--plan-root", type=Path, required=True)
    parser.add_argument("--taskgen-root", type=Path, required=True)
    parser.add_argument("--source-root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    files = build(args.plan_root.resolve(), args.taskgen_root.resolve(), args.source_root.resolve())
    write_files(args.output.resolve(), files)
    print(files["review-reuse-report.json"].decode())


if __name__ == "__main__":
    main()
