"""AxiomMegaDoc benchmark integration tests; no external checkout needed."""
from __future__ import annotations
import csv
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import megadoc


def write(root: Path, name: str, content: str) -> str:
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    return hashlib.sha256(content.encode("utf-8")).hexdigest()


class MegaDocBenchmarkTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "dataset"
        self.root.mkdir(parents=True)
        self.rid_valid = "axiom_scheme:sum:0:valid:none"
        self.rid_invalid = "axiom_scheme:sum:0:invalid:bad_effect"
        for rid in (self.rid_valid, self.rid_invalid):
            rel = "records/" + hashlib.sha256(rid.encode()).hexdigest()[:24] + ".json"
            write(self.root, rel, json.dumps({"record_id": rid, "canonical_source": "(+ 1 2)"}))
        doc = "megadocs/md_123.md"
        a = "attributes/md_123.json"
        v = "benchmark-views/pair_a.md"
        ds = "streams/train.jsonl"
        self.doc_sha = write(self.root, doc, "# MegaDoc\n" + "(+ 1 2)\n" * 100)
        write(self.root, a, '{"language":"scheme","construction":"latent_diagnostic"}\n')
        self.view_sha = write(self.root, v, "# unverified label, checker observation\n")
        write(self.root, ds, '{"id":"md_123","text":"..."}\n')
        self.manifest = {
            "schema": "axiom-megadoc-v012-manifest-v1",
            "source_bundle": "v0.11",
            "source_bundle_sha256": "a" * 64,
            "source_ledger_sha256": "b" * 64,
            "source_builder_sha256": "c" * 64,
            "generated_docs": 1, "benchmark_views": 1,
            "split_counts": {"train": 1},
            "documents": [{
                "id": "md_123", "path": doc, "attributes_path": a,
                "text_sha256": self.doc_sha, "record_ids": [
                    self.rid_valid, self.rid_invalid],
                "split": "train", "construction": "latent_diagnostic", "language": "scheme",
            }],
            "views": [{
                "path": v, "text_sha256": self.view_sha, "record_ids": [
                    self.rid_valid, self.rid_invalid], "split": "train",
                "language": "axiom_scheme", "family": "functional/list/sum"
            }],
        }
        self.save_manifest()

    def save_manifest(self):
        write(self.root, "manifest.json", json.dumps(self.manifest, sort_keys=True))

    def test_model_repeatable_and_v1_compatible(self):
        p1 = self.root.parent / "a.csv"
        p2 = self.root.parent / "b.csv"
        a = megadoc.trace(self.root, p1, pattern="mixed", passes=2)
        b = megadoc.trace(self.root, p2, pattern="mixed", passes=2)
        self.assertEqual(p1.read_bytes(), p2.read_bytes())
        self.assertEqual(a["events"], b["events"])
        self.assertGreater(a["events"], 30)
        self.assertEqual(a["observed_file_read_calls"], 0)
        with p1.open(newline="") as handle:
            rows = list(csv.DictReader(handle))
        self.assertEqual(list(rows[0]), megadoc.HEADER)
        self.assertTrue(all(row["speculative"] == "0" for row in rows))
        shapes = {}
        for row in rows:
            k = row["key"]
            shape = (int(row["charged_bytes"]), row["kind"])
            self.assertEqual(shapes.setdefault(k, shape), shape)
        self.assertFalse(any("megadocs/" in str(row) for row in rows))

    def test_assembly_reads_source_then_document(self):
        records, objects = megadoc.load_manifest(self.root)
        sched = megadoc.schedule(records, "assemble", "train", 1, 42)
        self.assertEqual(len(sched), 4)
        self.assertTrue(sched[0][1].startswith("records/"))
        self.assertTrue(sched[1][1].startswith("records/"))
        self.assertEqual(sched[2][1], "attributes/md_123.json")
        self.assertEqual(sched[3][1], "megadocs/md_123.md")
        self.assertIn("megadocs/md_123.md", objects)

    def test_real_reads_are_counted_not_misreported(self):
        p = self.root.parent / "read.csv"
        info = megadoc.trace(self.root, p, pattern="assemble",
                             mode="record", passes=2)
        self.assertGreater(info["observed_file_read_calls"], 0)
        self.assertGreater(info["observed_file_read_bytes"], 0)
        self.assertEqual(info["refill_cost_ns"], "model parameter, not measured device latency")
        self.assertIn("modeled logical", info["metadata_and_index_events"])

    def test_different_patterns_produce_different_traces(self):
        a = self.root.parent / "train.csv"
        b = self.root.parent / "search.csv"
        t = megadoc.trace(self.root, a, pattern="train")
        q = megadoc.trace(self.root, b, pattern="provenance")
        self.assertNotEqual(a.read_bytes(), b.read_bytes())
        self.assertGreater(q["events"], t["events"])
        self.assertIn("index", b.read_text())

    def test_tampering_and_unsafe_paths_rejected(self):
        (self.root / "megadocs/md_123.md").write_text("changed")
        with self.assertRaisesRegex(ValueError, "hash mismatch"):
            megadoc.load_manifest(self.root)
        self.doc_sha = write(self.root, "megadocs/md_123.md", "# MegaDoc\n" + "(+ 1 2)\n" * 100)
        self.manifest["documents"][0]["path"] = "../private"
        self.save_manifest()
        with self.assertRaisesRegex(ValueError, "invalid path"):
            megadoc.load_manifest(self.root)

    def test_reject_empty_split_and_invalid_modes(self):
        with self.assertRaisesRegex(ValueError, "no generated"):
            megadoc.trace(self.root, self.root.parent / "out.csv", split="test")
        with self.assertRaisesRegex(ValueError, "chunk"):
            megadoc.trace(self.root, self.root.parent / "out.csv", chunk=1000)
        with self.assertRaisesRegex(ValueError, "passes"):
            megadoc.trace(self.root, self.root.parent / "out.csv", passes=0)


if __name__ == "__main__":
    unittest.main()
