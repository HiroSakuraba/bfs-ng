# BFS-NG

**Experimental Rust filesystem research, inspired by the Be File System.**

BFS-NG aims to combine searchable, typed file attributes with transactional metadata, crash safety, checksums, and cache-aware performance. It is **not currently a usable filesystem**: it currently contains a trace laboratory and an experimental read-only metadata cache, not a mountable filesystem.

## Status

- **Implemented (experimental):** Rust 2024 workspace, `crates/cache-lab`, and `crates/metadata-cache` with generation-checked immutable metadata reads, checksums and concurrent tests. The lab retains trace generation, CSV replay, byte-budgeted no-cache and LRU baselines, S3-FIFO-inspired and Window TinyLFU-inspired simulators, report generation, fixtures and tests.
- **Validated by GitHub Actions:** Rust compilation, 11 tests, Clippy, and the independent LRU golden trace. Formatting is enforced from this tranche onward.
- **C4 reference comparison:** independent libCacheSim 0.3.5 comparison runner and workflow, with verified LRU control. Research S3-FIFO/TinyLFU variants differ measurably; see results and limitations in the C4 report. S3-FIFO/TinyLFU remain research variants.
- **Not implemented:** production disk format, mounts, persistent block-device driver, transactional allocator, indexes, snapshot retention, Windows/Linux adapters.

## First run

Install Rust 1.85 or later. From the repository root:

```sh
cargo test --workspace --all-targets
cargo run --release -p bfs-ng-cache-lab -- demo --events 50000 --seed 42 --capacity-bytes 1048576
cargo run --release -p bfs-ng-cache-lab -- sweep --input crates/cache-lab/fixtures/golden.csv --capacities 4096,8192,16384
python3 crates/cache-lab/scripts/lru_oracle.py crates/cache-lab/fixtures/golden.csv 8192
```

Cache miss costs are **trace-supplied model parameters**, not observed SSD or application latencies. Speculative requests are modeled as immediately resident on admission; real asynchronous prefetch completion is not modeled. The simulation does not measure OS page-cache overlap, CPU cache misses, physical I/O or tail latency.

## Layout

```text
Cargo.toml
crates/cache-lab/     # Standalone safe-Rust trace simulator
crates/metadata-cache/ # Generation-safe read-only metadata cache
docs/ARCHITECTURE.md  # Design decisions and roadmap
.github/workflows/    # Compilation, tests, lint and golden replay
```

The first crate is isolated so that cache experiments cannot corrupt future on-disk structures. Future crates will only be added when working code exists.

## Research and references

See [C4 independent comparison](docs/C4_REFERENCE_VALIDATION.md), [C5 cache contract](docs/C5_METADATA_CACHE.md), [the cache lab README](crates/cache-lab/README.md) and [architecture and milestones](docs/ARCHITECTURE.md). The foundational reference is Dominic Giampaolo's *Practical File System Design: The Be File System*, with modern directions from OpenZFS, XFS, Btrfs, S3-FIFO and TinyLFU research.

## Contribution and safety

See [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md). Do not put any real user data on future BFS-NG volumes until persistent storage is thoroughly tested and explicitly declared stable. Distributed under the [MIT License](LICENSE).
