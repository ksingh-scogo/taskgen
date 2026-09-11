from __future__ import annotations

import json
import shutil
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import seal_reviewed_batches as seal

REPO = Path(__file__).resolve().parents[1]


class ReviewReuseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.taskgen = self.root / "taskgen"
        shutil.copytree(REPO / "schemas", self.taskgen / "schemas")
        self.task = json.loads((REPO / "tests/fixtures/canonical/valid-task.json").read_bytes())
        decision = json.loads((REPO / "tests/fixtures/canonical/valid-review-v3.json").read_bytes())
        self.task_bytes = seal.canonical(self.task) + b"\n"
        self.task_id = seal.identity(self.task)
        candidate_id = seal.sha((1).to_bytes(8, "little") + self.task["prompt"].encode())
        self.candidate = {
            "candidate_id": candidate_id,
            "sequence": 1,
            "candidate": self.task,
            "prompt_sha256": seal.sha(self.task["prompt"].encode()),
        }
        self.review = {
            "schema_version": "scogo.taskgen.review-record.v3",
            "candidate_id": candidate_id,
            "sequence": 1,
            "review_model": "fixture-original-reviewer",
            "decision": decision,
            "final_disposition": "accepted",
            "references": [],
            "adjudication": None,
        }
        self.origin = self.taskgen / "runs/origin"
        self.origin.mkdir(parents=True)
        self.write_origin()
        self.source = self.root / "source"
        self.source.mkdir()
        (self.source / "tasks.jsonl").write_bytes(self.task_bytes)
        self.plan = self.root / "plan"
        (self.plan / "inputs").mkdir(parents=True)
        (self.plan / "inputs/part-001.jsonl").write_bytes(self.task_bytes)
        assignments = seal.canonical({"source_task_id": self.task_id, "batch": "part-001"}) + b"\n"
        (self.plan / "assignments.jsonl").write_bytes(assignments)
        self.manifest = {
            "source": {
                "repo_id": "fixture/source",
                "revision": "a" * 40,
                "files": [{**seal.descriptor("tasks.jsonl", self.task_bytes), "rows": 1}],
            },
            "total_rows": 1,
            "assignment_sha256": seal.sha(assignments),
            "batches": [
                {
                    "batch": "part-001",
                    "input": "inputs/part-001.jsonl",
                    "sealed_run": "sealed/part-001",
                    "rows": 1,
                    "sha256": seal.sha(self.task_bytes),
                    "bytes": len(self.task_bytes),
                }
            ],
        }
        (self.plan / "batch-manifest.json").write_bytes(seal.canonical(self.manifest))

    def write_origin(self, *, status="success", raw_review=None, declared=True):
        files = {
            "tasks": self.task_bytes,
            "candidates": seal.canonical(self.candidate) + b"\n",
            "reviews": raw_review
            if raw_review is not None
            else seal.canonical(self.review) + b"\n",
        }
        for name, data in files.items():
            (self.origin / f"{name}.jsonl").write_bytes(data)
        manifest = {
            "schema_version": "scogo.taskgen.run.v3",
            "run_id": "origin",
            "status": status,
            "artifacts": {
                name: seal.descriptor(name + ".jsonl", data) for name, data in files.items()
            }
            if declared
            else {},
        }
        (self.origin / "run.json").write_bytes(seal.canonical(manifest))

    def build(self):
        with patch(
            "socket.socket", side_effect=AssertionError("offline repackaging attempted network")
        ):
            return seal.build(self.plan, self.taskgen, self.source)

    def test_reuses_exact_review_without_model_or_network(self):
        files = self.build()
        self.assertEqual(
            files["sealed/part-001/reviews.jsonl"], (self.origin / "reviews.jsonl").read_bytes()
        )
        run = json.loads(files["sealed/part-001/run.json"])
        self.assertEqual(run["fresh_model_calls"], 0)
        self.assertEqual(run["reused_review_records"], 1)
        self.assertEqual(files, self.build())

    def test_missing_review_fails(self):
        self.write_origin(raw_review=b"")
        with self.assertRaisesRegex(ValueError, "lacks original accepted review"):
            self.build()

    def test_failed_attempt_requires_successful_publication(self):
        self.write_origin(status="failed")
        with self.assertRaisesRegex(ValueError, "successful publication"):
            self.build()

    def test_interrupted_review_is_reusable_with_verified_descendant(self):
        self.write_origin(
            status="running",
            raw_review=seal.canonical(self.review) + b'\n{"unfinished":',
            declared=False,
        )
        descendant = self.taskgen / "runs/descendant"
        descendant.mkdir()
        for name, data in {"tasks": self.task_bytes, "reviews": b"", "candidates": b""}.items():
            (descendant / f"{name}.jsonl").write_bytes(data)
        manifest = {
            "schema_version": "scogo.taskgen.run.v3",
            "run_id": "descendant",
            "status": "success",
            "artifacts": {
                name: seal.descriptor(name + ".jsonl", (descendant / f"{name}.jsonl").read_bytes())
                for name in ["tasks", "reviews", "candidates"]
            },
        }
        (descendant / "run.json").write_bytes(seal.canonical(manifest))
        run = json.loads(self.build()["sealed/part-001/run.json"])
        self.assertEqual(run["original_review_run_statuses"], {"running": 1})
        self.assertEqual(
            next(r for r in run["original_runs"] if r["run_id"] == "origin")[
                "ignored_truncated_trailing_rows"
            ],
            {"reviews": 1},
        )

    def test_candidate_identity_mismatch_fails(self):
        self.review["sequence"] = 2
        self.write_origin()
        with self.assertRaisesRegex(ValueError, "identity mismatch"):
            self.build()

    def test_changed_historical_bytes_fail(self):
        (self.origin / "reviews.jsonl").write_bytes(b"{}\n")
        with self.assertRaisesRegex(ValueError, "hash/size mismatch"):
            self.build()

    def test_existing_output_is_not_overwritten(self):
        output = self.root / "output"
        files = self.build()
        seal.write_files(output, files)
        (output / "sealed/part-001/tasks.jsonl").write_bytes(b"changed")
        with self.assertRaisesRegex(ValueError, "existing repackaged evidence differs"):
            seal.write_files(output, files)
        self.assertEqual((output / "sealed/part-001/tasks.jsonl").read_bytes(), b"changed")


if __name__ == "__main__":
    unittest.main()
