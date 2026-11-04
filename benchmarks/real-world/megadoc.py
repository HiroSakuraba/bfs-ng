#!/usr/bin/env python3
"""AxiomMegaDoc long-document/record-assembly benchmark for BFS-NG cache-lab.

Consumes AxiomMegaDoc v0.12 materialized corpus, not arbitrary source files.
Benchmark access events are workload models; record mode reads actual bytes.
Neither mode observes OS page-cache or physical-device misses.
"""
from __future__ import annotations

import argparse
import csv
from collections import defaultdict
import hashlib
import json
from pathlib import Path
import random
import sys
import time

from bench import Asset, Emitter, HEADER, MAX_READ, checked_path, safe_name


def _asset(root: Path, rel: str, sha256: str | None = None) -> Asset:
    if not isinstance(rel, str) or not safe_name(rel):
        raise ValueError("invalid path in AxiomMegaDoc manifest")
    path = checked_path(root, rel)
    if not path.is_file() or path.is_symlink():
        raise ValueError(f"missing or nonregular generated artifact: {rel}")
    size = path.stat().st_size
    if sha256 is not None:
        if hashlib.sha256(path.read_bytes()).hexdigest() != sha256:
            raise ValueError(f"content hash mismatch in generated artifact: {rel}")
    return Asset(rel, size, rel)


def load_manifest(root: Path) -> tuple[dict, dict[str, Asset]]:
    manifest = json.loads((root / "manifest.json").read_text(encoding="utf-8"))
    if manifest.get("schema") != "axiom-megadoc-v012-manifest-v1":
        raise ValueError("unsupported AxiomMegaDoc dataset manifest schema")
    if manifest.get("source_bundle") != "v0.11":
        raise ValueError("expected v0.11 upstream bundle")
    docs = manifest.get("documents")
    views = manifest.get("views")
    if not isinstance(docs, list) or not isinstance(views, list):
        raise ValueError("missing documents or views in AxiomMegaDoc manifest")
    if manifest.get("generated_docs") != len(docs) or manifest.get("benchmark_views") != len(views):
        raise ValueError("manifest count mismatch")
    objects: dict[str, Asset] = {}

    def add(rel: str, checksum: str | None = None) -> None:
        if rel not in objects:
            objects[rel] = _asset(root, rel, checksum)
        elif checksum is not None:
            _asset(root, rel, checksum)

    for doc in docs:
        if not isinstance(doc, dict) or not isinstance(doc.get("record_ids"), list):
            raise ValueError("invalid AxiomMegaDoc document entry")
        add(doc["path"], doc["text_sha256"])
        add(doc["attributes_path"])
        for rid in doc["record_ids"]:
            if not isinstance(rid, str) or not rid:
                raise ValueError("invalid source record ID")
            add("records/" + hashlib.sha256(rid.encode()).hexdigest()[:24] + ".json")
    for view in views:
        if not isinstance(view, dict):
            raise ValueError("invalid provenance view entry")
        add(view["path"], view["text_sha256"])
        for rid in view["record_ids"]:
            if not isinstance(rid, str) or not rid:
                raise ValueError("invalid view record ID")
            add("records/" + hashlib.sha256(rid.encode()).hexdigest()[:24] + ".json")
    for split in sorted(manifest.get("split_counts", {})):
        add(f"streams/{split}.jsonl")
    add("manifest.json")
    # All 520 source objects may be materialized; only verified paths are read.
    return manifest, objects


def selected_docs(manifest: dict, split: str) -> list[dict]:
    result = [item for item in manifest["documents"]
              if split == "all" or item["split"] == split]
    if not result:
        raise ValueError(f"no generated megadocs in requested split={split}")
    return sorted(result, key=lambda e: (e["split"], e["id"]))


def _source_record(rid: str) -> str:
    return "records/" + hashlib.sha256(rid.encode("utf-8")).hexdigest()[:24] + ".json"


def schedule(manifest: dict, pattern: str, split: str, passes: int, seed: int
             ) -> list[tuple[int, str, str]]:
    docs = selected_docs(manifest, split)
    views = sorted(
        [view for view in manifest["views"]
         if split == "all" or view["split"] == split],
        key=lambda x: x["path"])
    rng = random.Random(seed)
    requests: list[tuple[int, str, str]] = []

    def assemble(stream: int, order: list[dict]) -> None:
        for doc in order:
            # Read/parse the two source records, inspect attributes,
            # then read the assembled long-form megadoc.
            for rid in doc["record_ids"]:
                requests.append((stream, _source_record(rid), "read"))
            requests.append((stream, doc["attributes_path"], "index"))
            requests.append((stream, doc["path"], "read"))

    def provenance(stream: int, order: list[dict]) -> None:
        for view in order:
            # A deterministic logical index lookup, not an actual index read.
            requests.append((stream, "search:" + view["language"] + ":" + view["family"], "index"))
            for rid in view["record_ids"]:
                requests.append((stream, _source_record(rid), "read"))
            requests.append((stream, view["path"], "read"))

    def training(stream: int, order: list[dict]) -> None:
        relevant_splits = sorted({doc["split"] for doc in order})
        for split_name in relevant_splits:
            requests.append((stream, f"streams/{split_name}.jsonl", "read"))
        for doc in order:
            requests.append((stream, doc["path"], "read"))

    for phase in range(passes):
        order = list(docs)
        if pattern in ("train", "mixed"):
            rng.shuffle(order)
        if pattern in ("assemble", "mixed"):
            assemble(phase % 4, order)
        if pattern in ("train", "mixed"):
            training(1, order)
        if pattern in ("provenance", "mixed"):
            # Bounded, repeatable metadata lookup workload.
            # Rotate hot head of the view set across passes.
            take = min(128, len(views))
            offset = (phase * 19) % len(views) if views else 0
            lookup = (views[offset:] + views[:offset])[:take]
            provenance(2, lookup)
            provenance(2, lookup[:min(16, len(lookup))])
    return requests


def trace(root: Path, output: Path, pattern: str = "mixed",
          split: str = "train", mode: str = "model", passes: int = 2,
          seed: int = 42, chunk: int = 4096, max_read: int = 65536,
          max_events: int = 1_000_000, summary: Path | None = None) -> dict:
    if pattern not in ("assemble", "train", "provenance", "mixed"):
        raise ValueError("unsupported AxiomMegaDoc pattern")
    if mode not in ("model", "record"):
        raise ValueError("unsupported mode")
    if split not in ("train", "dev", "test", "all"):
        raise ValueError("unsupported split")
    if pattern in ("train", "mixed") and split == "all":
        raise ValueError("training-style workloads must select a single split")
    if not 1 <= passes <= 100:
        raise ValueError("passes must be 1..100")
    if seed < 0 or seed >= 2**128:
        raise ValueError("seed out of range")
    if chunk < 512 or chunk & (chunk - 1):
        raise ValueError("chunk must be power of two >=512")
    if max_read <= 0 or max_read > MAX_READ or max_read % chunk:
        raise ValueError("max_read must be a chunk multiple <=16MiB")
    if max_events < 1:
        raise ValueError("max_events must be positive")
    manifest, objects = load_manifest(root)
    revision = (manifest["source_ledger_sha256"] + ":"
                + manifest["source_builder_sha256"] + ":"
                + manifest["source_bundle_sha256"])
    requests = schedule(manifest, pattern, split, passes, seed)
    output.parent.mkdir(parents=True, exist_ok=True)
    started = time.perf_counter_ns()
    with output.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.writer(handle, lineterminator="\n")
        writer.writerow(HEADER)
        emitter = Emitter(writer, seed, revision, mode, chunk, max_events)
        read_ordinals: dict[str, int] = defaultdict(int)
        for stream, name, operation in requests:
            if operation == "index":
                # This synthetic query-result-page event has no corresponding file.
                emitter.emit(stream, "index", name)
            else:
                emitter.access(root, objects[name], stream, max_read,
                               read_ordinals[name])
                read_ordinals[name] += 1
    info = {
        "schema": "bfs-ng-megadoc-benchmark-summary-v1",
        "project": "AxiomMegaDoc",
        "pattern": pattern,
        "split": split,
        "mode": mode,
        "revision": revision,
        "seed": seed,
        "generated_docs": manifest["generated_docs"],
        "benchmark_views": manifest["benchmark_views"],
        "files_indexed": len(objects),
        "scheduled_accesses": len(requests),
        "events": emitter.events,
        "observed_file_read_calls": emitter.read_calls,
        "observed_file_read_bytes": emitter.bytes_read,
        "wall_duration_ns": time.perf_counter_ns() - started if mode == "record" else None,
        "metadata_and_index_events": "modeled logical events, not actual index reads",
        "refill_cost_ns": "model parameter, not measured device latency",
        "source_bundle_version": manifest["source_bundle"],
    }
    if summary:
        summary.parent.mkdir(parents=True, exist_ok=True)
        summary.write_text(json.dumps(info, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return info


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--summary", type=Path)
    parser.add_argument("--pattern", choices=("assemble", "train", "provenance", "mixed"),
                        default="mixed")
    parser.add_argument("--split", choices=("train", "dev", "test", "all"), default="train")
    parser.add_argument("--mode", choices=("model", "record"), default="model")
    parser.add_argument("--passes", type=int, default=2)
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--chunk", type=int, default=4096)
    parser.add_argument("--max-read", type=int, default=65536)
    parser.add_argument("--max-events", type=int, default=1_000_000)
    args = parser.parse_args(argv)
    try:
        result = trace(args.root.resolve(), args.output, args.pattern, args.split,
                       args.mode, args.passes, args.seed, args.chunk, args.max_read,
                       args.max_events, args.summary)
        print(json.dumps(result, indent=2, sort_keys=True))
        return 0
    except (OSError, KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
