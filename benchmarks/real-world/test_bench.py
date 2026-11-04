"""Tests for the corpus generator, measured file-access harness, and DVE dependency ordering."""
import csv
import io
import json
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bench


class RealWorldBenchmarkTests(unittest.TestCase):
    def setUp(self):
        self.tmp = TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def test_synthetic_axiomzig_is_deterministic_and_unvalidated(self):
        a = self.root / "a"
        b = self.root / "b"
        bench.generate_axiomzig(a, 12, 19)
        bench.generate_axiomzig(b, 12, 19)
        self.assertEqual((a / "documents/shard-000.jsonl").read_bytes(),
                         (b / "documents/shard-000.jsonl").read_bytes())
        docs = [json.loads(line) for line in
                (a / "documents/shard-000.jsonl").read_text().splitlines()]
        self.assertEqual(len(docs), 12)
        self.assertEqual(len(list((a / "src").glob("*.az"))), 12)
        self.assertEqual(len(list((a / "labels").glob("*.json"))), 12)
        for item in docs:
            self.assertEqual(item["metadata"]["label"]["validation_status"]["mode"],
                             "schema_only")
            self.assertIsNone(item["metadata"]["label"]["compiler_observed"])
            self.assertEqual(item["metadata"]["label"]["source_sha256"],
                             __import__("hashlib").sha256(
                                 item["text"].encode("utf-8")).hexdigest())
        with self.assertRaisesRegex(ValueError, "empty"):
            bench.generate_axiomzig(a, 1, 19)

    def test_model_trace_stable_and_accepted_by_strict_schema(self):
        root = self.root / "ax"
        bench.generate_axiomzig(root, 8, 7)
        items = bench.inventory(root, "axiomzig", False, 1000)
        first = self.root / "one.csv"
        second = self.root / "two.csv"
        one = bench.trace(root, items, "axiomzig", "compile", first,
                          None, 123, "fixture-commit-v1", "model", 3, 4096, 8192, 10000)
        two = bench.trace(root, items, "axiomzig", "compile", second,
                          None, 123, "fixture-commit-v1", "model", 3, 4096, 8192, 10000)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        self.assertEqual(one["events"], two["events"])
        self.assertEqual(one["observed_file_read_calls"], 0)
        with first.open(newline="") as handle:
            rows = list(csv.DictReader(handle))
        self.assertEqual(rows[0].keys(), dict.fromkeys(bench.HEADER).keys())
        self.assertEqual(len(rows), one["events"])
        shapes = {}
        for row in rows:
            key = int(row["key"])
            shape = (int(row["charged_bytes"]), row["kind"])
            self.assertEqual(shapes.setdefault(key, shape), shape)
            self.assertEqual(row["speculative"], "0")
            self.assertGreater(int(row["charged_bytes"]), 0)
        self.assertEqual(sorted(int(r["timestamp_ns"]) for r in rows),
                         [int(r["timestamp_ns"]) for r in rows])
        self.assertTrue(all("/" not in row["key"] for row in rows))

    def test_record_mode_performs_real_file_reads(self):
        root = self.root / "ax"
        bench.generate_axiomzig(root, 5, 8)
        items = bench.inventory(root, "axiomzig", False, 500)
        out = self.root / "observed.csv"
        summary = self.root / "observed.json"
        report = bench.trace(root, items, "axiomzig", "shuffle", out,
                             summary, 7, "rev-test", "record", 1, 4096, 4096, 10000)
        self.assertGreater(report["observed_file_read_calls"], 0)
        self.assertGreater(report["observed_file_read_bytes"], 0)
        self.assertEqual(report["mode"], "record")
        self.assertIsInstance(report["wall_duration_ns"], int)
        self.assertEqual(json.loads(summary.read_text())["events"], report["events"])

    def test_zip_members_are_safe_and_no_extraction(self):
        root = self.root / "ax"
        root.mkdir()
        with zipfile.ZipFile(root / "corpus.zip", "w") as arc:
            arc.writestr("docs/good.az", "fn f() -> I64 effects() { return 1; }\n")
            arc.writestr("../escape.az", "BAD")
            arc.writestr("labels/good.json", '{"id":"ok"}')
        items = bench.inventory(root, "axiomzig", True, 1000)
        self.assertEqual(len(items), 2)
        self.assertTrue(all("escape" not in item.name for item in items))
        self.assertFalse((root.parent / "escape.az").exists())
        self.assertTrue(all(item.member is not None for item in items))
        model = self.root / "zip-model.csv"
        obs = self.root / "zip-record.csv"
        bench.trace(root, items, "axiomzig", "compile", model,
                    None, 4, "zip-rev", "model", 1, 4096, 4096, 1000)
        result = bench.trace(root, items, "axiomzig", "compile", obs,
                             None, 4, "zip-rev", "record", 1, 4096, 4096, 1000)
        self.assertGreater(result["observed_file_read_bytes"], 0)

    def test_dve_real_dependency_paths(self):
        root = self.root / "dve"
        examples = root / "examples"
        examples.mkdir(parents=True)
        (examples / "house.dvoxscene.json").write_text(json.dumps({
            "format": "DVOXSCENE", "version": 1,
            "objects": [{"file": "house.dvox"}, {"file": "../outside.dvox"}],
        }))
        (examples / "house.dvox").write_bytes(b"dummy voxel block")
        (examples / "model.gltf").write_text(json.dumps({
            "asset": {"version": "2.0"}, "buffers": [{"uri": "model.bin"}]}))
        (examples / "model.bin").write_bytes(b"glTF buffer placeholder")
        (root / "game.dvegame").write_text(
            "DVE_GAME 1\nentryScene=examples/house.dvoxscene.json\n")
        (root / "shaders/common").mkdir(parents=True)
        (root / "shaders/example.hlsl").write_text('#include "common/light.hlsl"\n')
        (root / "shaders/common/light.hlsl").write_text("float x = 1;\n")
        items = bench.inventory(root, "dve", False, 100)
        by_name = {item.name: item for item in items}
        scene = by_name["examples/house.dvoxscene.json"]
        self.assertEqual(bench.dependencies(root, scene, by_name), ["examples/house.dvox"])
        self.assertEqual(bench.dependencies(root, by_name["examples/model.gltf"], by_name),
                         ["examples/model.bin"])
        self.assertEqual(bench.dependencies(root, by_name["shaders/example.hlsl"], by_name),
                         ["shaders/common/light.hlsl"])
        order = bench.schedule(root, "dve", "scene", items, 1, 7)
        names = [a.name for _, a in order]
        self.assertLess(names.index("examples/house.dvoxscene.json"),
                        names.index("examples/house.dvox"))
        self.assertLess(names.index("examples/model.gltf"),
                        names.index("examples/model.bin"))
        self.assertLess(names.index("game.dvegame"),
                        names.index("examples/house.dvoxscene.json"))
        self.assertNotIn("outside.dvox", names)

    def test_dve_hotreload_and_package_have_distinct_patterns(self):
        root = self.root / "dve"
        (root / "shaders").mkdir(parents=True)
        (root / "assets/materials").mkdir(parents=True)
        (root / "shaders/main.hlsl").write_text("float4 main() { return 1; }\n")
        (root / "assets/materials/rough.png").write_bytes(b"fake image")
        items = bench.inventory(root, "dve", False, 100)
        hot = bench.schedule(root, "dve", "hotreload", items, 3, 42)
        pack = bench.schedule(root, "dve", "package", items, 3, 42)
        self.assertEqual(len(hot), 6)
        self.assertEqual(len(pack), 6)
        self.assertTrue(all(a.suffix in (".hlsl", ".png") for _, a in hot))
        self.assertEqual([a.name for _, a in pack[:2]],
                         sorted(item.name for item in items))

    def test_refuses_symlink_escape_and_limits(self):
        root = self.root / "ax"
        root.mkdir()
        (root / "ordinary.az").write_text("safe")
        with self.assertRaisesRegex(ValueError, "exceeds"):
            bench.inventory(root, "axiomzig", False, 0)
        with self.assertRaisesRegex(ValueError, "power of two"):
            bench.trace(root, bench.inventory(root, "axiomzig", False, 20),
                        "axiomzig", "compile", self.root / "bad.csv", None,
                        1, "r", "model", 1, 1000, 4000, 100)
        if hasattr(__import__("os"), "symlink"):
            outside = self.root / "outside"
            outside.write_text("NOT IN ROOT")
            try:
                (root / "leak.az").symlink_to(outside)
            except (OSError, NotImplementedError):
                return
            items = bench.inventory(root, "axiomzig", False, 20)
            self.assertTrue(all(item.name != "leak.az" for item in items))

    def test_cli_inventory_revision_and_trace(self):
        root = self.root / "ax"
        bench.generate_axiomzig(root, 2, 5)
        inventory_path = self.root / "inventory.json"
        rc = bench.main(["inventory", "--project", "axiomzig",
                         "--root", str(root), "--revision", "known-rev",
                         "--output", str(inventory_path)])
        self.assertEqual(rc, 0)
        manifest = json.loads(inventory_path.read_text())
        self.assertEqual(manifest["revision"], "known-rev")
        self.assertGreater(len(manifest["items"]), 0)
        rc = bench.main(["trace", "--project", "axiomzig", "--root", str(root),
                         "--revision", "known-rev", "--pattern", "compile",
                         "--output", str(self.root / "trace.csv")])
        self.assertEqual(rc, 0)


if __name__ == "__main__":
    unittest.main()
