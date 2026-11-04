#!/usr/bin/env python3
"""BFS-NG real-world workload *harness*, not an OS page-cache profiler.

Requires Python 3.10+, standard library only. Repository data stays external.
"""
from __future__ import annotations

import argparse
import csv
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import posixpath
import random
import re
import subprocess
import sys
import time
import zipfile

HEADER = "timestamp_ns,stream_id,key,charged_bytes,kind,refill_cost_ns,speculative".split(",")
MODEL_COST = {"directory": 100_000, "metadata": 110_000, "index": 130_000, "data": 200_000}
AX_EXT = {".az", ".json", ".jsonl", ".md", ".py", ".zip"}
DVE_EXT = {".dvox", ".dmesh", ".dvepak", ".gltf", ".glb", ".bin", ".hlsl",
           ".glsl", ".lua", ".json", ".dvegame", ".dds", ".ktx", ".ktx2",
           ".png", ".jpg", ".jpeg", ".wav", ".ogg", ".obj", ".mtl", ".txt"}
DVE_PREFIX = ("assets/", "examples/", "shaders/", "content/", "projects/", "tests/fixtures/")
SKIP_DIRS = {".git", ".svn", "__pycache__", "target", "build", "node_modules",
             ".venv", "venv", "third_party", ".cache"}
MAX_READ = 16 * 1024 * 1024


@dataclass(frozen=True)
class Asset:
    name: str  # relative path, or archive!member (never emitted to trace)
    bytes: int
    container: str
    member: str | None = None

    @property
    def suffix(self) -> str:
        return PurePosixPath(self.member or self.container).suffix.lower()


def safe_name(name: str) -> bool:
    if not name or chr(92) in name or "\x00" in name:
        return False
    path = PurePosixPath(name)
    return (not path.is_absolute() and all(part not in ("..", ".", "") for part in path.parts)
            and ":" not in path.parts[0])


def source_revision(root: Path, revision: str | None) -> str:
    if revision:
        return revision
    try:
        result = subprocess.run(
            ["git", "-C", str(root), "rev-parse", "HEAD"],
            capture_output=True, text=True, check=True, timeout=5,
        )
        if not result.stdout.strip():
            raise ValueError("empty git revision")
        root_check = subprocess.run(
            ["git", "-C", str(root), "rev-parse", "--show-toplevel"],
            capture_output=True, text=True, check=True, timeout=5,
        )
        if Path(root_check.stdout.strip()).resolve() != root.resolve():
            raise ValueError("root is not repository toplevel; pass --revision")
        return result.stdout.strip()
    except (OSError, subprocess.SubprocessError):
        raise ValueError("specify --revision for a non-git source directory") from None


def allowed(project: str, rel: str) -> bool:
    suffix = Path(rel).suffix.lower()
    if project == "axiomzig":
        return suffix in AX_EXT
    return (rel.startswith(DVE_PREFIX) or rel == "game.dvegame") and (
        suffix in DVE_EXT or rel.endswith((".dvoxscene.json", ".dve-import.json"))
    )


def inventory(root: Path, project: str, zip_members: bool, max_items: int) -> list[Asset]:
    if not root.is_dir():
        raise ValueError(f"root is not a directory: {root}")
    assets: list[Asset] = []
    for parent, dirs, names in os.walk(root, followlinks=False):
        dirs[:] = sorted(d for d in dirs if d not in SKIP_DIRS and
                         not (Path(parent) / d).is_symlink())
        for filename in sorted(names):
            path = Path(parent) / filename
            if path.is_symlink() or not path.is_file():
                continue
            rel = path.relative_to(root).as_posix()
            if not allowed(project, rel):
                continue
            if zip_members and project == "axiomzig" and path.suffix.lower() == ".zip":
                try:
                    with zipfile.ZipFile(path) as archive:
                        for info in sorted(archive.infolist(), key=lambda z: z.filename):
                            member = info.filename
                            if (info.is_dir() or not safe_name(member)
                                    or PurePosixPath(member).suffix.lower()
                                    not in {".az", ".json", ".jsonl", ".md"}
                                    or info.flag_bits & 1
                                    or (info.external_attr >> 16) & 0o170000 == 0o120000):
                                continue
                            assets.append(Asset(f"{rel}!{member}", info.file_size, rel, member))
                            if len(assets) > max_items:
                                raise ValueError(f"inventory exceeds --max-items={max_items}")
                except (zipfile.BadZipFile, OSError) as error:
                    # Real repositories sometimes contain mislabeled or incomplete
                    # ZIPs. Preserve their opaque-file access pattern, but do not
                    # invent any archive member entries.
                    print(f"warning: not a readable ZIP: {rel} ({error}); indexing as opaque file",
                          file=sys.stderr)
                    assets.append(Asset(rel, path.stat().st_size, rel))
                    if len(assets) > max_items:
                        raise ValueError(f"inventory exceeds --max-items={max_items}")
            else:
                assets.append(Asset(rel, path.stat().st_size, rel))
                if len(assets) > max_items:
                    raise ValueError(f"inventory exceeds --max-items={max_items}")
    names_seen: set[str] = set()
    for asset in assets:
        if asset.name in names_seen:
            raise ValueError(f"duplicate asset identity in inventory: {asset.name}")
        names_seen.add(asset.name)
    return sorted(assets, key=lambda a: a.name)


def is_metadata(asset: Asset) -> bool:
    name = asset.name.lower()
    return ("/labels/" in "/" + name or "/attributes/" in "/" + name
            or name.startswith(("labels/", "attributes/")))


def dependencies(root: Path, asset: Asset, by_name: dict[str, Asset]) -> list[str]:
    if asset.member or asset.bytes > 2_000_000:
        return []
    name = asset.name
    path = root / asset.container
    results: list[str] = []
    def add(candidate: str) -> None:
        if (not candidate or candidate.startswith(("data:", "http:", "https:"))
                or not safe_name(candidate)):
            return
        target = posixpath.normpath(posixpath.join(posixpath.dirname(name), candidate))
        if target in by_name and target != name:
            results.append(target)

    try:
        if name.endswith(".dvoxscene.json"):
            obj = json.loads(path.read_text(encoding="utf-8"))
            for item in obj.get("objects", []):
                if isinstance(item, dict) and isinstance(item.get("file"), str):
                    add(item["file"])
        elif name.endswith(".gltf"):
            obj = json.loads(path.read_text(encoding="utf-8"))
            for section in ("buffers", "images"):
                for entry in obj.get(section, []):
                    if isinstance(entry, dict) and isinstance(entry.get("uri"), str):
                        add(entry["uri"])
        elif name.endswith(".dvegame"):
            for line in path.read_text(encoding="utf-8").splitlines():
                item = line.split("#", 1)[0].strip()
                if "=" in item:
                    key, value = (part.strip() for part in item.split("=", 1))
                    if key in ("entryScene", "startupScript", "settings"):
                        add(value)
        elif name.endswith((".hlsl", ".glsl")):
            for match in re.findall(r'^[ \t]*#include[ \t]+["<]([^">]+)[">]',
                                    path.read_text(encoding="utf-8"),
                                    flags=re.MULTILINE):
                add(match)
    except (UnicodeError, OSError, json.JSONDecodeError, TypeError, ValueError):
        # Malformed dependencies are excluded; benchmark never guesses paths.
        return []
    return sorted(set(results))


def schedule(root: Path, project: str, pattern: str, items: list[Asset],
             passes: int, seed: int) -> list[tuple[int, Asset]]:
    by_name = {item.name: item for item in items}
    rng = random.Random(seed)
    out: list[tuple[int, Asset]] = []
    if project == "axiomzig":
        sources = [x for x in items if x.suffix in (".az", ".jsonl")]
        related = [x for x in items if x not in sources]
        if not sources:
            sources = items
            related = []
        for phase in range(passes):
            order = list(sources)
            if pattern == "shuffle":
                rng.shuffle(order)
            for i, item in enumerate(order):
                out.append((phase % 4, item))
                if related and i % 4 == 0:
                    out.append((phase % 4, related[(i // 4 + phase) % len(related)]))
            # A short hotset repeated between passes models repeated compiler queries.
            if pattern == "compile":
                for hot in sources[:min(16, len(sources))]:
                    out.append((3, hot))
    else:
        scenes = sorted(
            (x for x in items if x.name.endswith((".dvoxscene.json", ".dvegame", ".gltf"))),
            key=lambda x: (0 if x.name.endswith(".dvegame") else 1, x.name),
        )
        hotspots = [x for x in items if x.suffix in (
            ".hlsl", ".glsl", ".png", ".dds", ".ktx", ".ktx2")]
        deps = {a.name: dependencies(root, a, by_name) for a in items}
        def visit(name: str, stream: int, seen: set[str]) -> None:
            if name in seen or name not in by_name:
                return
            seen.add(name)
            out.append((stream, by_name[name]))
            for dependent in deps.get(name, []):
                visit(dependent, stream, seen)
        for phase in range(passes):
            if pattern in ("scene", "mixed"):
                for scene in scenes:
                    visit(scene.name, phase % 4, set())
            if pattern in ("hotreload", "mixed"):
                for hot in hotspots[:min(len(hotspots), 32)]:
                    visit(hot.name, 2, set())
            if pattern == "package":
                for item in items:
                    out.append((1, item))
            if pattern == "mixed":
                pool = list(items)
                rng.shuffle(pool)
                out.extend((1, item) for item in pool[:min(128, len(pool))])
        if not out:
            out = [(0, item) for _ in range(passes) for item in items]
    return out


def generate_axiomzig(out: Path, count: int, seed: int) -> None:
    if not 1 <= count <= 5000:
        raise ValueError("--count must be 1..5000")
    if out.exists() and any(out.iterdir()):
        raise ValueError("output directory must be empty (no overwriting)")
    rng = random.Random(seed)
    (out / "src").mkdir(parents=True, exist_ok=True)
    (out / "documents").mkdir(exist_ok=True)
    (out / "labels").mkdir(exist_ok=True)
    with (out / "documents" / "shard-000.jsonl").open("w", encoding="utf-8", newline="\n") as stream:
        for i in range(count):
            amount = rng.randrange(1, 20_000)
            text = (f"fn sample_{i:05d}(x: I64) -> I64 effects() {{\n"
                    f"    return x + {amount};\n}}\n")
            digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
            example = f"bfsng_{i:05d}__{digest[:16]}"
            (out / "src" / f"{example}.az").write_text(text, encoding="utf-8")
            label = {"schema_version": "bfs-ng-synthetic-label-v1",
                     "example_id": example, "source_sha256": digest,
                     "generator": "bfs-ng-real-world-benchmark",
                     "template": "arithmetic_function",
                     "validation_status": {"mode": "schema_only"},
                     "compiler_observed": None}
            (out / "labels" / f"{example}.json").write_text(
                json.dumps(label, sort_keys=True) + "\n", encoding="utf-8")
            document = {"id": example, "text": text, "source": "bfsng_synthetic_v1",
                        "metadata": {"stream": "syntax_arithmetic", "label": label}}
            stream.write(json.dumps(document, sort_keys=True) + "\n")


def checked_path(root: Path, rel: str) -> Path:
    path = root / rel
    if not path.resolve().is_relative_to(root.resolve()):
        raise ValueError("asset escaped root")
    return path


def read_asset(root: Path, asset: Asset, limit: int, offset: int) -> bytes:
    path = checked_path(root, asset.container)
    before = path.stat()
    if asset.member is None:
        with path.open("rb") as handle:
            handle.seek(offset)
            data = handle.read(limit)
    else:
        with zipfile.ZipFile(path) as archive:
            with archive.open(asset.member) as handle:
                # ZipExtFile.seek can decompress earlier contents. Model runs do not.
                if offset:
                    handle.seek(offset)
                data = handle.read(limit)
    after = path.stat()
    if before.st_size != after.st_size or before.st_mtime_ns != after.st_mtime_ns:
        raise ValueError("source changed during trace capture; rerun with a new revision")
    return data


class Emitter:
    def __init__(self, writer: csv.writer, seed: int, revision: str, mode: str,
                 chunk: int, max_events: int):
        self.writer = writer
        self.salt = seed.to_bytes(16, "little", signed=False)
        self.revision = revision
        self.mode = mode
        self.chunk = chunk
        self.max_events = max_events
        self.events = 0
        self.read_calls = 0
        self.bytes_read = 0
        self.cost_model_only = True

    def key(self, kind: str, name: str, index: int = 0) -> int:
        h = hashlib.blake2b(digest_size=8, key=self.salt)
        h.update(json.dumps([self.revision, kind, name, index],
                            separators=(",", ":")).encode("utf-8"))
        return int.from_bytes(h.digest(), "little")

    def emit(self, stream: int, kind: str, name: str, index: int = 0,
             charged: int = 4096) -> None:
        if self.events >= self.max_events:
            raise ValueError(f"trace exceeded --max-events={self.max_events}")
        t = self.events * 1_000 if self.mode == "model" else time.monotonic_ns()
        self.writer.writerow([t, stream, self.key(kind, name, index), charged,
                              kind, MODEL_COST[kind], 0])
        self.events += 1

    def access(self, root: Path, asset: Asset, stream: int, max_read: int,
               pass_index: int = 0) -> None:
        parent = posixpath.dirname(asset.name)
        self.emit(stream, "directory", parent)
        self.emit(stream, "metadata", asset.name)
        if is_metadata(asset) or asset.name.endswith((".dvoxscene.json", ".gltf", ".dvegame")):
            self.emit(stream, "index", "index:" + asset.name)
        if asset.bytes == 0:
            return
        start = (pass_index * max_read) % asset.bytes if max_read else 0
        start = (start // self.chunk) * self.chunk
        length = min(max_read, asset.bytes - start)
        if self.mode == "record":
            payload = read_asset(root, asset, length, start)
            self.read_calls += 1
            self.bytes_read += len(payload)
            length = len(payload)
            if not length:
                raise ValueError(f"unexpected empty read of {asset.name}")
        for pos in range(start, start + length, self.chunk):
            self.emit(stream, "data", asset.name, pos // self.chunk, self.chunk)


def trace(root: Path, items: list[Asset], project: str, pattern: str,
          output: Path, summary: Path | None, seed: int, revision: str,
          mode: str, passes: int, chunk: int, max_read: int, max_events: int) -> dict:
    if chunk < 512 or chunk & (chunk - 1):
        raise ValueError("--chunk must be a power of two >= 512")
    if max_read <= 0 or max_read > MAX_READ or max_read % chunk:
        raise ValueError("--max-read must be a multiple of chunk, <= 16 MiB")
    scheduled = schedule(root, project, pattern, items, passes, seed)
    output.parent.mkdir(parents=True, exist_ok=True)
    start = time.perf_counter_ns()
    with output.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.writer(handle, lineterminator="\n")
        writer.writerow(HEADER)
        emit = Emitter(writer, seed, revision, mode, chunk, max_events)
        for i, (stream, asset) in enumerate(scheduled):
            emit.access(root, asset, stream, max_read, i // max(len(items), 1))
    info = {"schema": "bfs-ng-realworld-summary-v1", "source": project,
            "pattern": pattern, "mode": mode, "revision": revision,
            "seed": seed, "files_indexed": len(items),
            "scheduled_accesses": len(scheduled), "events": emit.events,
            "observed_file_read_calls": emit.read_calls,
            "observed_file_read_bytes": emit.bytes_read,
            "wall_duration_ns": time.perf_counter_ns() - start if mode == "record" else None,
            "trace_refill_costs": "hypothetical, not measured I/O latency",
            "metadata_and_directory_events": "modeled logical accesses"}
    if summary:
        summary.parent.mkdir(parents=True, exist_ok=True)
        summary.write_text(json.dumps(info, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return info


def positive_int(value: str) -> int:
    n = int(value)
    if n < 0 or n > (2**64 - 1):
        raise argparse.ArgumentTypeError("expected unsigned 64-bit integer")
    return n


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    gen = sub.add_parser("generate-axiomzig", help="create a small synthetic, compiler-unvalidated corpus")
    gen.add_argument("--output", type=Path, required=True)
    gen.add_argument("--count", type=int, default=64)
    gen.add_argument("--seed", type=positive_int, default=42)
    for name in ("inventory", "trace"):
        p = sub.add_parser(name)
        p.add_argument("--project", choices=("axiomzig", "dve"), required=True)
        p.add_argument("--root", type=Path, required=True)
        p.add_argument("--output", type=Path, required=True)
        p.add_argument("--zip-members", action="store_true", help="enumerate AxiomZig ZIP members without extraction")
        p.add_argument("--max-items", type=int, default=20_000)
        p.add_argument("--revision", help="source commit or immutable dataset revision")
        if name == "trace":
            p.add_argument("--summary", type=Path)
            p.add_argument("--pattern", help="axiomzig: compile|shuffle; dve: scene|hotreload|package|mixed")
            p.add_argument("--mode", choices=("model", "record"), default="model")
            p.add_argument("--passes", type=int, default=2)
            p.add_argument("--seed", type=positive_int, default=42)
            p.add_argument("--chunk", type=int, default=4096)
            p.add_argument("--max-read", type=int, default=65536)
            p.add_argument("--max-events", type=int, default=1_000_000)
    args = parser.parse_args(argv)
    try:
        if args.command == "generate-axiomzig":
            generate_axiomzig(args.output, args.count, args.seed)
            print(f"generated {args.count} unvalidated AxiomZig samples in {args.output}")
            return 0
        if args.max_items < 1:
            raise ValueError("--max-items must be positive")
        root = args.root.resolve()
        items = inventory(root, args.project, args.zip_members, args.max_items)
        revision = source_revision(root, args.revision)
        if args.command == "inventory":
            # Local-only manifest contains paths; never publish unreviewed.
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps({
                "schema": "bfs-ng-realworld-inventory-v1",
                "revision": revision, "project": args.project,
                "items": [a.__dict__ for a in items]},
                sort_keys=True, indent=2) + "\n", encoding="utf-8")
            print(f"indexed {len(items)} objects into {args.output}")
            return 0
        patterns = ("compile", "shuffle") if args.project == "axiomzig" else (
            "scene", "hotreload", "package", "mixed")
        pattern = args.pattern or patterns[0]
        if pattern not in patterns:
            raise ValueError(f"unsupported pattern: {pattern}; choose {', '.join(patterns)}")
        if not 1 <= args.passes <= 100:
            raise ValueError("--passes must be 1..100")
        result = trace(root, items, args.project, pattern, args.output, args.summary,
                       args.seed, revision, args.mode, args.passes, args.chunk,
                       args.max_read, args.max_events)
        print(json.dumps(result, sort_keys=True))
        return 0
    except (ValueError, OSError, zipfile.BadZipFile, RuntimeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
