# BFS-NG

**Experimental Rust filesystem research, inspired by the Be File System.**

BFS-NG aims to combine searchable, typed file attributes with transactional metadata, crash safety, checksums, and cache-aware performance. It is **not currently a usable filesystem**: its implementation currently consists of research components, not a mountable filesystem.

## Status

- **Implemented (experimental):** Rust 2024 workspace, `crates/cache-lab`, and `crates/metadata-cache` with generation-checked immutable metadata reads, checksums and concurrent tests. The lab retains trace generation, CSV replay, byte-budgeted no-cache and LRU baselines, S3-FIFO-inspired and Window TinyLFU-inspired simulators, report generation, fixtures and tests.
- **Validated by GitHub Actions:** Rust compilation, 11 tests, Clippy, and the independent LRU golden trace. Formatting is enforced from this tranche onward.
- **C4 reference comparison:** independent libCacheSim 0.3.5 comparison runner and workflow, with verified LRU control. Research S3-FIFO/TinyLFU variants differ measurably; see results and limitations in the C4 report. S3-FIFO/TinyLFU remain research variants.
- **M0 searchable metadata (in-memory only):** typed attribute schemas, stable volume/inode IDs, ordered index postings, atomic in-memory update publication, immutable snapshots, equality/range/prefix/membership queries, and a full-scan differential oracle. See [M0 design and limits](docs/M0_SEARCHABLE_ATTRIBUTES.md).
- **F0.R1 rotating root-payload allocation (experimental v2/v9):** three reusable, CoW-safe F0 payload blocks (2–4) and two alternating superblocks (0–1), so root publication no longer burns another permanent low block each generation. F1/F2 always allocate above block 4. Includes rollover, torn-write, fallback and long-running (>image size) generation tests. [Design and limits](docs/F0_R1_ROTATING_ROOTS.md). **Breaking disposable-image format; no v1/v8 migration or hardware power-cut qualification.**
- **F0 historical v1 storage prototype:** regular-file block image with 4 KiB blocks, alternating checksummed roots, ordered two-barrier publication, and crash/torn-write simulation. See [original F0 protocol](docs/F0_STORAGE.md).
- **F2.R4 certified allocation and bounded reclaim sweep (experimental):** version-8 checked deferred/ready/pending journal with a durable physical sweep cursor, once-per-open two-root allocation certification, in-handle safe-pool advancement only after F0 publication, and explicit `reclaim_step()` calls. [Design, tests and limits](docs/F2_R4_CERTIFIED_RECLAMATION.md). **Per-commit reference-count reconstruction and full-run journal rewrites still remain; F0 monotonic payload allocation is replaced by F0.R1.**
- **F2.R3 crash-atomic reclamation journal (historical v7):** version-7 checksummed free-run checkpoints with ready and one-rotation-pending generation fences; journal metadata is protected under both recoverable F0 roots and malformed fallback payloads quarantine reuse. [Format and limitations](docs/F2_R3_RECLAMATION_JOURNAL.md). **Still runs a full reachability scan; the journal is a replayable witness, not yet an incremental allocator or background sweeper.**
- **F2.R2 incremental CoW reference tree (historical v6):** version-6 CRC-protected branch/leaf pages, page-sharing path-copy updates, independently checked reference counts, and F0 fallback-root delayed-free epoch reporting. [Design and limitations](docs/F2_R2_INCREMENTAL_REFERENCES.md). **Still full-scan allocation, no crash-resumable free-run journal and no F0 root-area redesign.**
- **F2.R1 durable reference-index witness (historical):** version-5 immutable sorted page chain, independent full-graph verification and conservative index-page protection. [Original scope](docs/F2_R1_DURABLE_REFERENCES.md). Version-5 test images are not accepted by current experimental writers.
- **F2/F3 backlog follow-up:** read-only reference/reclamation audit; balanced bulk-load nodes; localized CoW sibling merge/redistribution on deletion; direct checksummed disk-range pagination with snapshot cursors. See [closeout scope](docs/F2_F3_BACKLOG_CLOSEOUT.md). Durable free-space trees and fully incremental namespace maps are still missing.
- **F4 durable typed searchable attributes:** M0-normalized schema and value metadata plus sorted secondary postings are written as one private F3 CoW catalog, committed atomically with file changes and verified during recovery. Historical snapshot queries supported. See [F4 design](docs/F4_DURABLE_ATTRIBUTES.md). This is a **breaking version-4 disposable image format**, not an incremental on-disk attribute B+ tree.
- **F3 incremental metadata tree:** a path-copying B+tree-style updater reuses unchanged child nodes, splits full pages and collapses empty subtrees; tests assert reduced physical metadata writes. See [F3 design](docs/F3_INCREMENTAL_COW.md). Still not a fully occupancy-balanced B+ tree or a mounted filesystem.
- **F2 retention and allocation prototype:** persistent read-only snapshot pins, generation-stamped file extents and conservative multi-root reachability-based block reuse. See [F2 design](docs/F2_SNAPSHOTS_RECLAMATION.md). This is a breaking v2 test-image format.
- **F1 image-backed namespace prototype:** checksummed immutable file/data blocks, multi-level sorted metadata nodes, nested directories, transactional updates, append-only allocation and full-tree crash recovery. See [F1 scope and protocol](docs/F1_COW_NAMESPACE.md). Still not mountable or safe for real data.
- **Not implemented:** production disk format, mounts, persistent block-device driver, transactional allocator, **durable** indexes, persistent snapshot retention, authorization-aware queries, Windows/Linux adapters.

## Searchable metadata design

The [metadata query architecture](docs/SEARCHABLE_METADATA.md) explains how the experimental M0 typed in-memory query engine can eventually become a persistent, transactionally consistent CoW index. The durable query engine and live-query feed are **not implemented**.

## Real-world corpus and asset benchmarks

The [AxiomZig, DVE, and AxiomMegaDoc suite](benchmarks/real-world/README.md) collects source-backed deterministic modeled or benchmark-owned read traces, suitable for cache policy comparisons. It does **not** measure kernel cache misses or operate on a BFS-NG filesystem. The [AxiomMegaDoc walkthrough](benchmarks/real-world/MEGADOC.md) covers long-document assembly, training-style reads and provenance workloads. Bulk training data remains versioned in [AxiomMegaDoc](https://github.com/HiroSakuraba/AxiomMegadoc/tree/main/corpus/v0.12).

## First run

Install Rust 1.85 or later. From the repository root:

```sh
cargo test --workspace --all-targets
cargo test -p bfs-ng-attribute-index
cargo test -p bfs-ng-storage
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
crates/attribute-index/ # M0 typed in-memory query/index reference
crates/storage/         # F0 disposable block-image prototype
benchmarks/real-world/ # AxiomZig/DVE/AxiomMegaDoc access-pattern harness
docs/ARCHITECTURE.md  # Design decisions and roadmap
.github/workflows/    # Compilation, tests, lint and golden replay
```

The first crate is isolated so that cache experiments cannot corrupt future on-disk structures. Future crates will only be added when working code exists.

## Research and references

See [M0 searchable attributes](docs/M0_SEARCHABLE_ATTRIBUTES.md), [C4 independent comparison](docs/C4_REFERENCE_VALIDATION.md), [C5 cache contract](docs/C5_METADATA_CACHE.md), [the cache lab README](crates/cache-lab/README.md) and [architecture and milestones](docs/ARCHITECTURE.md). The foundational reference is Dominic Giampaolo's *Practical File System Design: The Be File System*, with modern directions from OpenZFS, XFS, Btrfs, S3-FIFO and TinyLFU research.

## Contribution and safety

See [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md). Do not put any real user data on future BFS-NG volumes until persistent storage is thoroughly tested and explicitly declared stable. Distributed under the [MIT License](LICENSE).
