# F5.1 — bounded sparse extent map and incremental checksum CoW

**Status:** Experimental v10 implementation in [PR #24](https://github.com/HiroSakuraba/bfs-ng/pull/24). This is a **bounded, single-logical-file** tranche, not the complete F5.1 inode/extent/inline/streaming subsystem. The regular BFS-NG FileSystem remains **v9**. No F5 block reuse, background reclaim, clone, or real-device durability claim is enabled.

## 1. Implemented wire representation

The v10 fs root remains one parent-authenticated 4 KiB \`TreeId::Fs\` **leaf**. Its items represent a sparse logical file, with at most **64 logical blocks = 256 KiB**:

| Item | Key | Value |
| --- | --- | --- |
| Extent | \`logical_block_start: u64\` in big-endian order | \`physical_start: u64\` LE, \`blocks: u32\` LE, \`reserved: u32 = 0\`, \`birth: u64\` LE (24 bytes) |
| File size | Eight \`0xff\` bytes (sorted last) | Logical byte size as \`u64\` LE (8 bytes) |

A fresh empty fs leaf is allowed; subsequent updates store an explicit file-size entry even for zero-length files. The physical range of an extent is \`[physical_start, physical_start + blocks)\`, monotonically increasing with logical block index. Adjacent logical blocks sharing a contiguous physical run and birth generation are serialized as one extent. Missing logical blocks are **holes**, and read as zero. A hole has no data checksum. The last allocated partial block's unused tail is zero-filled and independently verified.

\`FileExtents::decode\` checks bounds, birth, distinct physical addresses within one version, extent overlap, size, reserved bytes, and the fs leaf role. \`encode_items\` emits canonical, coalesced runs. There are no inode table, directory names, compression, inline-data items, preallocation items or reflinks in this tranche; those are still covered by the larger [F5 design](F5_DATA_LAYER.md).

## 2. Byte-addressed operations

\`experimental_v10::ExtentDataWriter\` supports \`format\`, \`open\`, \`file_size\`, \`write_at(offset, data)\`, \`truncate(new_size)\`, \`read_current\`, \`read_at\`, \`read_snapshot_at\`, \`pin_current\`, and \`mark_dropping\`.

- Every \`write_at\` touching unaligned blocks performs read-modify-write **only on affected 4 KiB logical blocks**. All other extent references retain their physical address/birth unchanged in the new fs leaf.
- Appending or writing beyond EOF grows logical size; unwritten holes remain zero, without allocating data blocks. Pure truncate growth similarly leaves new ranges sparse.
- Truncation drops extents completely past the new EOF and, if necessary, copies and zeroes the shortened final block's bytes past EOF. The prior version remains retained if a snapshot is pinned.
- Repeated writes serialize newly allocated touched runs in **ascending logical/physical order**, coalescing contiguous blocks. Actual free-space allocation is not yet fragmentation-aware; only descending virgin pages are offered.
- A zero-byte \`write_at\` and same-size \`truncate\` are no-ops. An overflowing/too-large write is rejected without publication.

**Explicit bounds:** one logical file, at most 64 logical blocks (256 KiB), a single fs extent leaf, and a bounded 160-slot private staging candidate scan. This is neither a general arbitrary-sized file API nor a streaming/spill implementation.

## 3. Incremental CoW checksum index

The previous F5.0b \`HeaderlessDataWriter\` remains unchanged and still bulk-rebuilds its checksum tree. The **new F5.1 writer** uses \`stage_checksum_delta\`:

1. Independently reconstruct its selected F0 manifest's complete live/pinned data reachability, and the exact physical checksum map.
2. Construct the exact next manifest's union from its new live file extents and all pinned or \`Dropping\` snapshot roots.
3. Compute a physical-address delta: delete checksum entries no longer needed by the next manifest; insert checksums for new immutable data blocks; reject a changed checksum at any still-live physical address.
4. CoW-copy **only modified checksum leaves and their ancestor paths**, potentially splitting branches/roots. Unchanged subtrees keep their old physical block pointer and CRC. When the final checksum disappears, the new root is absent.
5. Verify the complete staged graph against a fresh read-only overlay and exact ownership oracle before F0 publication.

The checksum-item codec remains \`physical-address BE -> count=1, reserved=0, CRC32C\`. Multi-block runs within one checksum item are still deferred. A single ordinary update no longer needs to serialize every unchanged checksum leaf, but **full per-commit two-root graph verification remains deliberately expensive**. Do not claim an overall logarithmic commit bound or a reclaiming allocator.

## 4. Publication and allocation safety

Every mutating operation reconstructs ownership from **both** recoverable F0 candidates, including snapshot catalog/roots, all referenced fs blocks, checksum-tree metadata and data blocks. Conflicting physical incarnations, malformed roots, missing checksums, bad zero tails or ambiguous fallback metadata fail closed. The writer chooses only zero-verified virgin addresses below the published high-water mark; nonzero crash orphans are skipped, not recycled.

Staged data/fs/checksum pages are independently checked using an in-memory read-only overlay before publication. F0 then performs staged-page writes → sync → rotating root payload → sync → alternate superblock → sync. Ambiguous failure poisons the in-handle writer. Snapshot \`Dropping\` is only a retained state, not final deletion.

These checks are **necessary experimental safety gates**, not evidence of general hardware crash durability or a production-ready v10 filesystem. The v9 storage engine and its F2.R4 allocation rules are unchanged.

## 5. Regression and performance acceptance

\`cargo test -p bfs-ng-storage --test storage_f5_1_extents\` covers cross-block unaligned overwrites, sparse holes, truncate shrink/growth, trailing padding, snapshot retention across reopen, extent coalescing, physical alias rejection, exact size bounds, a deterministic randomized byte-oracle test, torn publication and small-image ENOSPC.

\`cargo test -p bfs-ng-storage --test storage_f5_1_checksum_delta\` checks an independently reconstructed checksum oracle; edits on separated leaves, retained physical subtree identity, root deletion, nonexistent removals, in-place physical rechecksumming refusal and insufficient reservations.

Measure old/new checksum-page counts for the same sparse edits before declaring performance improvement. The checksum planner's page-copy reduction is a **structural** property; it is not a measured filesystem benchmark. Next required steps include multilevel fs extent-tree path copying, inline data, \`punch_hole\`, \`fallocate\`, streaming/spill with bounded memory, per-inode namespaces, and F5.2/F5.3 safe block reuse.

Related: [F5 design](F5_DATA_LAYER.md), [F5.0b checksums](F5_0B_CHECKSUM_DATA.md), [F5.0a v10 format](F5_0A_WIRE_FORMAT.md), [F5.0a strict writer](F5_0A_VIRGIN_WRITER.md), [architecture](ARCHITECTURE.md).
