"""Dependency-free tests for C4 comparison protocol."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("compare_libcachesim.py")
spec = importlib.util.spec_from_file_location("compare_libcachesim", SCRIPT)
compare_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(compare_module)


class AdapterTests(unittest.TestCase):
    def test_reported_delta_is_descriptive(self):
        names = ("lru", "s3fifo", "wtinylfu")
        rust = {name: {"misses": "4", "miss_bytes": "16384"} for name in names}
        reference = {
            name: {"misses": 3, "miss_bytes": 12288} for name in names
        }
        report = compare_module.compare(rust, reference, 7, 8192, "test")
        self.assertEqual(report["results"][0]["miss_delta"], 1)
        self.assertIn("non-equivalent", report["note"])

    def test_reject_prefetch_and_identity_change(self):
        header = ",".join(compare_module.HEADER) + "\n"
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "trace.csv"
            path.write_text(header + "0,0,3,4096,metadata,100,1\n")
            with self.assertRaisesRegex(ValueError, "speculative"):
                compare_module.load_events(path)
            path.write_text(
                header + "0,0,3,4096,metadata,100,0\n"
                "1,0,3,8192,metadata,100,0\n"
            )
            with self.assertRaisesRegex(ValueError, "changed"):
                compare_module.load_events(path)

    def test_legal_demand_trace(self):
        header = ",".join(compare_module.HEADER) + "\n"
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "trace.csv"
            path.write_text(
                header + "0,0,1,4096,metadata,100,0\n"
                "1,0,1,4096,metadata,100,0\n"
            )
            self.assertEqual(len(compare_module.load_events(path)), 2)


if __name__ == "__main__":
    unittest.main()
