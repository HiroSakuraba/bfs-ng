# BFS-NG

**Experimental Rust filesystem research, inspired by the Be File System.**

BFS-NG aims to combine searchable, typed file attributes with transactional metadata, crash safety, checksums, and cache-aware performance. It is **not currently a usable filesystem**: the first implementation tranche is a deterministic cache-policy laboratory.

## Status

- **Implemented (experimental):** Rust 2024 workspace and `crates/cache-lab`, with trace generation, CSV replay, byte-budgeted no-cache and LRU baselines, S3-FIFO-inspired and Window TinyLFU-inspired simulators, report generation, fixtures and tests.
- **Validated by GitHub Actions:** Rust compilation, 11 tests, Clippy, and the independent LRU golden trace. Formatting is enforced from this tranche onward.
- **Not implemented:** disk format, filesystem mounts, block-device driver, transactional engine, real metadata cache, indexed attributes, snapshots, Windows/Linux filesystem adapters.

## Real-world workload benchmarks

The [AxiomZig and DVE benchmark harness](benchmarks/real-world/README.md) generates compiler-unvalidated AxiomZig fixtures, inventories checked-out corpus archives, and produces recorded or modeled page-level traces from DVE scenes and assets. It feeds the existing cache-policy simulator; **it does not mount BFS-NG or measure kernel page-cache misses**.

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
benchmarks/real-world/ # AxiomZig/DVE corpus and trace workload harness
docs/ARCHITECTURE.md  # Design decisions and roadmap
.github/workflows/    # Compilation, tests, lint and golden replay
```

The first crate is isolated so that cache experiments cannot corrupt future on-disk structures. Future crates will only be added when working code exists.

## Research and references

See [the cache lab README](crates/cache-lab/README.md) and [architecture and milestones](docs/ARCHITECTURE.md). The foundational reference is Dominic Giampaolo's *Practical File System Design: The Be File System*, with modern directions from OpenZFS, XFS, Btrfs, S3-FIFO and TinyLFU research.

## Contribution and safety

See [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md). Do not put any real user data on future BFS-NG volumes until persistent storage is thoroughly tested and explicitly declared stable. Distributed under the [MIT License](LICENSE).
