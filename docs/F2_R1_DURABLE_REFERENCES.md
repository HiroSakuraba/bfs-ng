# F2.R1 — durable block-reference index, independently verified

**Status:** experimental image-only reference-accounting milestone. NOT a production filesystem, mount, fsck, crash-consistent free-space database, or complete removal of allocation scans.

## What is implemented

F2.R1 adds immutable checksum-protected **sorted, chained reference-index pages** rooted in each F0 manifest. A reference record is \`(physical_block_address, reference_count)\`, where the count is the number of **distinct live or pinned historical namespace roots belonging to that F0 candidate** that reference the block. The index is used for durable accountability and cross-checks, and is never trusted for block reuse without a fresh independent scan.

Every changed filesystem generation now:

1. Uses F2's verified two-root reachable-set oracle to select blocks that are safe to allocate.
2. Builds F3 CoW file extents, metadata nodes and the F4 typed catalog, all in staged blocks. F3 discards superseded staged metadata pages.
3. Runs a **read-only staged/durable overlay** of the full new tree and each pinned snapshot, independently checking metadata and file CRCs, file/catalog references and unique block ownership. It computes sorted per-block counts.
4. Writes a new immutable reference-index page chain using the same allocator. These new index pages **are not recursively included in their own counts**; they are instead directly rooted by the F0 manifest and automatically protected by the allocator while *either* F0 slot can recover them.
5. Atomically publishes one F0 root containing the new namespace root, snapshot pins and index-chain pointer, page count and logical entry count. All staged writes and ordered sync barriers precede the checksummed F0 root publication.
6. On open/recovery, verifies both the entire namespace + pinned tree graph **and** the persisted reference pages, then requires the index to match the independently reconstructed counts byte-for-byte at the logical \`BTreeMap\` level. Mismatches, duplicate/out-of-order addresses, missing/looped pages, invalid page generations, impossible references and corrupted checksums all reject the candidate. A previous complete root may be selected instead.

The retained-root allocator still reconstructs physical reachability before reusing **any** block. It now also marks both candidate roots' index-chain pages as protected. Thus, dropping a snapshot does not cause immediate unsafe reuse: both F0 candidates must cease referencing the snapshot or its old index chain.

The F2 \`allocation_report()\` exposes two new diagnostics: \`reference_index_pages\` and \`reference_index_entries\`, in addition to shared/reclaimable block counts and \`reuse_blocked\` quarantine on damaged F0-valid candidates.

## Disk layout

**Breaking test-image format:** F0's superblock and CRC32C root-slot protocol remain unchanged, but the F2/F3/F4 manifest magic is now \`BFSNGM5\0\` with version **5** (formerly 4). This version is incompatible with previous test images; **there is no migration or mixed-version writer support**. Do not open newer images with old experimental writers.

The existing manifest's fixed 64-byte header and 40-byte sorted snapshot entries are retained. Appended immediately after the snapshots is a 16-byte checked trailer:

| Field | Type | Meaning |
| --- | --- | --- |
| first index page | u64 LE | Address of immutable first reference page |
| page count | u32 LE | Number of linked pages |
| entry count | u32 LE | Number of sorted physical block records |

All are covered by F0 root payload CRC and publication barriers.

Each reference page is one 4 KiB block: 8-byte \`BFSREF5\0\` magic, u64 birth/root generation, u64 next pointer (0 for final page), u32 CRC32C at offset 24, u16 entry count at offset 28, reserved zero bytes at offsets 30–31, then up to **406** sorted, fixed-size 10-byte records \`(address:u64, count:u16)\`. All unused bytes must be zero. The page chain is immutable and does not overwrite previous references; allocation can reuse retired pages only after both root candidates no longer reference them. A single all-zero-record page commits an explicitly verified empty graph.

The manifest references at most 2,048 index pages; entries may describe only physically present, high-region blocks. The implementation rejects zero counts, out-of-order or duplicate addresses, cycles and pages that appear among their own referenced blocks.

## Independent evidence / fault model

The new \`storage_f2_references.rs\` suite exercises a real multigeneration image, pinned historical data, crossing one full reference page, corrupted index checksum and page chain recovery, a **forged checksum-valid reference count** rejected by the independent tree oracle, crash faults around index staging and F0 publication, and F4 typed search + snapshot consistency. The older F0–F4 and F2/F3 backlog regression tests remain mandatory.

Run \`cargo test -p bfs-ng-storage --test storage_f2_references\` and \`cargo test --workspace --all-targets\`, plus Clippy and rustfmt.

## What F2.R1 deliberately does NOT solve

- **Not a B+ tree or production refcount tree:** the persisted index is a sorted immutable *page chain*, not yet an incrementally path-copied reference B+tree.
- **Not faster end-to-end yet:** we still validate the complete live/pinned graph before accepting an index and before reuse. Writes rebuild the full reference index. The main outcome is a durable, checkable allocation witness and a foundation for delayed-free epochs.
- **Not a persistent free-space allocator:** candidates remain calculated from reachability. There is no independently committed free-run tree, relocation/compaction or crash-resumable garbage-collection journal.
- F0 still consumes a new low-end root payload block per generation, so even with physical data reuse an image has a finite generation lifetime.
- No cross-process exclusive lock, ACLs, authenticated metadata, hardware power-failure validation, or online mount.
- Existing per-file 256 KiB maximum, 2,048 entry cap and at most 16 snapshot pins remain.

## Next F2.R2 milestone

Implement a compact **CoW extent/range reference tree** and explicit generation retirement epoch. Keep an independent full-scan verifier in CI, but use verified on-disk reference deltas for bounded incremental commits only after exhaustive differential tests, crash/mutation fuzzing and root-slot rotation prove that no pinned or fallback page can be reused early. A separate F0 redesign is required to eliminate the monotonically consumed low-end root payload blocks.
