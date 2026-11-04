# BFS-NG architecture and decision log

**Status:** early research implementation; not yet a filesystem.

## Core principles

1. **Correctness before optimization:** crash consistency, explicit durability contracts, bounded parsing, checksums and independently tested recovery precede filesystem integration.
2. **Rust 2024 by default:** forbid unsafe code in research crates; reserve narrow, documented unsafe/FFI boundaries for eventual kernel or OS adapters.
3. **Portable core:** future filesystem logic must be independent of Linux FUSE and Windows WinFsp adapters.
4. **Searchable metadata:** typed attributes and transactionally consistent indexes are core goals, not sidecars.
5. **Cache optimization is optional:** any cache miss policy must preserve validity of generation IDs, transactional publication, and eviction of pinned/dirty blocks.
6. **Measure actual costs:** cache hit rate is not equivalent to I/O avoidance. Simulated refill penalties are not measured wall-clock latency.

## Storage direction under evaluation

A checksummed copy-on-write metadata design with atomic root publication is preferred for a *new* filesystem. A metadata write-ahead-log variant is reserved as a documented alternative, not mixed into CoW by default.

A disk format will need explicit little-endian encodings, checksummed root slots, generation IDs, free-space reservations, ordered durable writes, recovery idempotence, and an unambiguous fsync contract.

## Cache architecture

The trace simulator models residency decisions only. Physical caching may involve application buffers, the OS page cache, filesystem metadata caching, CPU caches and NVMe device queues. A future production cache must allocate *one accountable memory budget*, integrate OS-cache behavior, distinguish prefetch from demand traffic, and keep dirty/pinned pages out of eviction candidates.

The research sequence is:
- C1: deterministic trace schema, reproducible generators, replay baseline.
- C2: compare LRU, S3-FIFO, W-TinyLFU policies under identical byte capacity.
- C3: cost-weighted reports and golden reference invariants.
- C4: validate policy behavior against independently implemented references; fix deviations.
- C5: add an opt-in read-only generation-safe metadata cache with explicit invalidation.
- C6: measure real query p50/p99, CPU L1/LLC/TLB misses, page-cache overlap and write amplification.

## First filesystem milestones

- F0: disk-format specification, power-failure model, block device abstraction.
- F1: image-backed storage, validated block/inode parsing, directory structure.
- F2: choose and implement a *single* transaction durability mechanism.
- F3: checksums, scrub and recoverability tests under injected failures.
- F4: typed attributes, stable index keys and transactional query consistency.
- F5: optional snapshots, reflinks, live-query stream and mature caching.

No cache result is a proof of filesystem safety. No filesystem semantics are implemented yet.

## Key references

- BeFS: https://www.haiku-os.org/legacy-docs/practical-file-system-design.pdf
- Haiku BFS: https://github.com/haiku/haiku/tree/master/src/add-ons/kernel/file_systems/bfs
- OpenZFS: https://openzfs.github.io/openzfs-docs/
- XFS logging: https://docs.kernel.org/filesystems/xfs/xfs-delayed-logging-design.html
- S3-FIFO: https://s3fifo.com/
- W-TinyLFU: https://github.com/ben-manes/caffeine/wiki/Efficiency
- libCacheSim: https://github.com/1a1a11a/libCacheSim
