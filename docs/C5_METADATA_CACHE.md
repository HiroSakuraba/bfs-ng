# C5 — Generation-safe metadata cache

The new `bfs-ng-metadata-cache` crate is an experimental **read-only** in-memory component, not a mountable filesystem or persistent transaction engine.

## On-disk validation format (experimental)

Every block is fixed-sized and starts with a **48-byte little-endian** header: ASCII magic `BFNM` (4 bytes), format version (u16 = 1), kind (u16), volume (u128), physical block (u64), generation (u64), CRC32C (u32) and four zero-reserved bytes. CRC32C covers the whole block with bytes 40–43 treated as zero.

The cache rejects truncated blocks, bad headers, wrong generation/volume/address and checksum mismatches **before admission**. CRC32C only detects accidental corruption, not malicious modification. The metadata-block format is a prototype, **not** an approved BFS-NG production disk format.

## Read contract

- Entries are keyed by `(volume, physical_block, block_allocation_generation)`, and returned as immutable `Arc<MetadataBlock>`.
- Reference LRU has a resident-byte limit, but HashMap/Arc/queue bookkeeping is not charged against that budget.
- No cache lock is held over a device read. Concurrent misses may duplicate physical reads; insertion rechecks the key under the lock.
- Eviction drops the cache's ownership but does not revoke an already returned `Arc`. Zero budget performs read-through without retaining entries.
- No write-back, dirty entries, prefetch, physical block allocation or transaction publication.

### Critical allocator obligation

A snapshot carries its root generation; each CoW child reference separately carries the block's **allocation/birth generation**. Newer snapshots can therefore read unchanged older blocks, while future-generation references are rejected. Generation-tagged keys **prevent mistaken cache aliasing**, but do not prevent disk blocks from being reused prematurely. The CoW allocator and snapshot manager must retain physical blocks until **all** snapshots capable of referencing them have retired. If storage already contains a newer generation, the old-generation read must fail identity validation rather than silently accept newer contents.

## Tests

`cargo test --workspace --all-targets` exercises a known CRC32C vector, malformed blocks, checksum failure, generation separation, address reuse, transient read failures, zero budget, LRU eviction, live pinned handles and concurrent reads.

Next: wire snapshot pinning to a real allocator, develop a generation-aware storage device and inject crash/retirement faults. Only after that should optimized admission or prefetch replace the reference LRU.
