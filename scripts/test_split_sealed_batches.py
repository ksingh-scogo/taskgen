from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from seal_reviewed_batches import canonical, descriptor, identity, read_json
from split_sealed_batches import split


class SourceShardTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.parent = self.root / "sealed/part-001"
        self.parent.mkdir(parents=True)
        task = read_json(
            (
                Path(__file__).resolve().parents[1] / "tests/fixtures/canonical/valid-task.json"
            ).read_bytes()
        )
        tasks = [{**task, "prompt": task["prompt"] + f" Fixture case {i}."} for i in range(8)]
        reviews = [
            {
                "candidate_id": f"fixture-{i}",
                "review_model": "fixture-reviewer",
                "final_disposition": "accepted",
            }
            for i in range(8)
        ]
        lineage = [
            {
                "source_task_id": identity(row),
                "candidate_id": f"fixture-{i}",
                "review_origin_status": "success",
            }
            for i, row in enumerate(tasks)
        ]
        values = {
            "tasks": tasks,
            "candidates": tasks,
            "reviews": reviews,
            "original_candidates": [{"candidate": row} for row in tasks],
            "review_lineage": lineage,
            "rejected": [],
        }
        artifacts = {}
        for name, rows in values.items():
            data = b"".join(canonical(row) + b"\n" for row in rows)
            (self.parent / f"{name}.jsonl").write_bytes(data)
            artifacts[name] = descriptor(name + ".jsonl", data)
        run = {
            "status": "success",
            "operation": "offline_repack_original_accepted_reviews",
            "artifacts": {**artifacts, "run": {"file": "run.json"}},
        }
        (self.parent / "run.json").write_bytes(canonical(run))
        (self.root / "batch-manifest.json").write_bytes(
            canonical(
                {
                    "total_rows": 8,
                    "batches": [{"batch": "part-001", "rows": 8, "sealed_run": "sealed/part-001"}],
                }
            )
        )

    def test_pilots_and_remaining_chunks_cover_every_row_once(self):
        files = split(self.root, 3, 1, 2)
        manifest = read_json(files["shards.json"])
        self.assertEqual([job["rows"] for job in manifest["jobs"]], [1, 2, 3, 2])
        ids = [task for job in manifest["jobs"] for task in job["source_task_ids"]]
        self.assertEqual(len(ids), len(set(ids)))
        self.assertEqual(len(ids), 8)
        self.assertEqual(files, split(self.root, 3, 1, 2))

    def test_changed_parent_artifact_is_rejected(self):
        (self.parent / "reviews.jsonl").write_bytes(b"changed")
        with self.assertRaisesRegex(ValueError, "hash mismatch"):
            split(self.root, 3)

    def test_scale_cap_is_not_bypassed(self):
        with self.assertRaises(ValueError):
            split(self.root, 101)


if __name__ == "__main__":
    unittest.main()
