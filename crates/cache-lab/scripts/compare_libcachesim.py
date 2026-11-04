#!/usr/bin/env python3
"""Independent differential comparison with libCacheSim, not policy equivalence."""
import argparse
import csv
import importlib.metadata
import json
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory

HEADER = [
    "timestamp_ns", "stream_id", "key", "charged_bytes",
    "kind", "refill_cost_ns", "speculative",
]


def load_events(path):
    with open(path, newline="", encoding="utf-8") as source:
        reader = csv.DictReader(source)
        if reader.fieldnames != HEADER:
            raise ValueError("unsupported CSV trace header")
        shape = {}
        previous_time = None
        events = []
        for line, record in enumerate(reader, 2):
            if record["speculative"] != "0":
                raise ValueError(f"line {line}: speculative access is not comparable")
            event = {
                column: int(record[column])
                for column in ("timestamp_ns", "stream_id", "key", "charged_bytes")
            }
            event["kind"] = record["kind"]
            if event["charged_bytes"] <= 0 or (
                previous_time is not None and event["timestamp_ns"] < previous_time
            ):
                raise ValueError(f"line {line}: invalid object size or timestamp")
            previous_time = event["timestamp_ns"]
            signature = (event["charged_bytes"], event["kind"])
            old = shape.setdefault(event["key"], signature)
            if old != signature:
                raise ValueError(f"line {line}: key changed size or kind")
            events.append(event)
        return events


def rust_reports(trace, capacity, cargo):
    with TemporaryDirectory(prefix="bfs-ng-reference-") as directory:
        output = Path(directory) / "rust.csv"
        subprocess.run(
            [
                cargo, "run", "--quiet", "--release", "-p", "bfs-ng-cache-lab",
                "--", "replay", "--input", str(trace),
                "--capacity-bytes", str(capacity), "--policies",
                "lru,s3fifo,wtinylfu", "--output", str(output),
            ],
            check=True,
        )
        with output.open(newline="", encoding="utf-8") as handle:
            rows = list(csv.DictReader(handle))
    return {row["policy"]: row for row in rows if row["class"] == "all"}


def reference_reports(events, capacity, libcachesim):
    result = {}
    constructors = (
        ("lru", libcachesim.LRU),
        ("s3fifo", libcachesim.S3FIFO),
        ("wtinylfu", libcachesim.WTinyLFU),
    )
    for name, constructor in constructors:
        if name == "wtinylfu":
            # Match the BFS-NG variant's minimum 4 KiB window in bytes.
            # The upstream default 1% window can be smaller than a 4 KiB block,
            # making even basic admission behavior incomparable.
            window_bytes = min(capacity, max(capacity // 100, 4096))
            cache = constructor(
                cache_size=capacity, window_size=window_bytes / capacity
            )
        else:
            cache = constructor(cache_size=capacity)
        hits = 0
        miss_bytes = 0
        for event in events:
            request = libcachesim.Request(
                obj_id=event["key"],
                obj_size=event["charged_bytes"],
                clock_time=event["timestamp_ns"],
            )
            if cache.get(request):
                hits += 1
            else:
                miss_bytes += event["charged_bytes"]
        result[name] = {
            "misses": len(events) - hits,
            "miss_bytes": miss_bytes,
        }
    return result


def compare(rust, reference, events, capacity, version):
    results = []
    for name in ("lru", "s3fifo", "wtinylfu"):
        model = rust[name]
        other = reference[name]
        misses = int(model["misses"])
        missed_bytes = int(model["miss_bytes"])
        results.append(
            {
                "policy": name,
                "rust_misses": misses,
                "reference_misses": other["misses"],
                "miss_delta": misses - other["misses"],
                "rust_miss_bytes": missed_bytes,
                "reference_miss_bytes": other["miss_bytes"],
                "miss_byte_delta": missed_bytes - other["miss_bytes"],
                "rust_miss_ratio": misses / events if events else 0,
                "reference_miss_ratio": other["misses"] / events if events else 0,
            }
        )
    return {
        "schema": "bfs-ng-c4-comparison-v1",
        "libcachesim_version": version,
        "events": events,
        "capacity_bytes": capacity,
        "note": "S3-FIFO and W-TinyLFU are non-equivalent research variants.",
        "results": results,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--trace", required=True, type=Path)
    parser.add_argument("--capacity", required=True, type=int)
    parser.add_argument("--cargo", default="cargo")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--require-lru-match", action="store_true")
    options = parser.parse_args(argv)
    if options.capacity <= 0:
        parser.error("capacity must be positive")
    try:
        import libcachesim
    except ImportError as error:
        parser.error(f"install libcachesim to run independent comparison: {error}")
    events = load_events(options.trace)
    rust = rust_reports(options.trace, options.capacity, options.cargo)
    reference = reference_reports(events, options.capacity, libcachesim)
    try:
        version = importlib.metadata.version("libcachesim")
    except importlib.metadata.PackageNotFoundError:
        version = "unknown"
    report = compare(rust, reference, len(events), options.capacity, version)
    data = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if options.output:
        options.output.write_text(data, encoding="utf-8")
    else:
        print(data)
    control = report["results"][0]
    if options.require_lru_match and (
        control["miss_delta"] or control["miss_byte_delta"]
    ):
        print("ERROR: independent LRU control mismatch", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"C4 comparison failed: {error}", file=sys.stderr)
        sys.exit(2)
