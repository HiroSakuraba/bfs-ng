# F2 — retained snapshots and generation-safe block reclamation

**Experimental format: disposable images only. No mount, production durability, or multiwriter locking.**

F2 extends F1 with persistent read-only snapshot pins, birth-tagged extents and conservative physical block recycling. The manifest changes incompatibly to format version 2 (magic BFSNGM2); existing F1 test images require recreation.

## Snapshot catalog

FsTransaction.pin_snapshot() pins the generation on which the transaction began; FsTransaction.drop_snapshot(generation) removes a pin. Both changes publish atomically with any file updates on commit. FileSystem.snapshots() enumerates pinned generations; read_snapshot_file(generation, path) traverses and checksums the historical tree and file contents.

A snapshot is an immutable tree-root reference, not a duplicate copy of its file contents. The F2 manifest extends the 56-byte F1 layout with an 8-byte catalog header (u32 count plus reserved zero) and at most 16 sorted, fixed 40-byte snapshot entries. Each entry carries generation, root block, high-watermark cursor, next inode and record count as little-endian 64-bit fields. The manifest is covered by F0 checksummed root publication.

Each leaf record now includes an 8-byte file-content birth generation. File data chunks carry a 32-bit birth-generation tag at header offset 28. F2 rejects new commits beyond generation 2^32−1. Metadata nodes already have eight-byte birth generations and CRC32C checksums.

## Conservative allocation and crash safety

Before each commit, BFS-NG examines **both F0 root-slot candidates**, their complete F2 metadata trees, all their retained snapshot trees and referenced file contents, validating checksums. Their reachable block-address union forms a protected set.

Only previously used high-region blocks absent from that protected set are eligible for reuse, and file extents must occupy contiguous free runs. Blocks allocated from new high-to-low space remain monotonic. If any F0-valid candidate contains malformed F2 state, reuse is disabled for that commit rather than guessing about ownership.

A dropped snapshot remains physically protected while it is still referenced by the older fallback F0 root. Only after root rotation makes it unreachable from *both* slots can blocks be reused. This is delayed reclamation with a rebuilt reachability map, not a trusting serialized free list.

New data and metadata blocks are written and synced first; then F0 writes and syncs the manifest payload, publishes the alternate checksummed superblock, and syncs again. If any operation fails, the handle requires reopen. A failed final sync may leave either the previous or a complete new generation recoverable. Torn writes are tested during actual block reuse.

**Critical boundary:** even when recycled blocks are available, F2 refuses a commit if advancing F0's low-end root-payload location would cross the high-watermark allocation cursor. This avoids root/data overlap, at the cost of a finite image lifetime.

## Testing and practical limits

The storage_f2.rs suite covers historical reads after overwrite/deletion, real file-image close/reopen, multiblock snapshots, 16-pin bounds, idempotent pins, absent-pin errors, safe reuse after fallback retirement, pinned-root protection, repeated recovery, and faults/torn writes at each commit stage.

F2 still rewrites the entire sorted metadata tree for each modified namespace and traverses live plus historical trees to reconstruct allocation safety. It does **not** implement a durable reference-count tree, compaction, continuous snapshots, write-concurrent mounts, encryption, ACLs, large files, hardware-backed power-loss validation, or a stable on-disk format. 256 KiB file limit, 2048 entries, 240-byte paths, and the F0 generation root-payload allocation limit remain.

Next: F3 incremental CoW B+tree updates and a durable, independently checked reference/reclamation tree, followed by F4 persistent searchable-attribute indexes. Use cargo test -p bfs-ng-storage and the whole-workspace CI.

## Reference designs

- [Btrfs CoW and reference-counted extents](https://btrfs.readthedocs.io/en/stable/dev/dev-btrfs-design.html)
- [OpenZFS copy-on-write and block birth transactions](https://github.com/openzfs/openzfs-docs/blob/master/docs/Basic%20Concepts/Copy-on-write.rst)
