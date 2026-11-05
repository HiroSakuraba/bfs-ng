# F5.3 — Indexed v10 allocation and crash-resumable retirement

**Status:** Opt-in experimental v10 integration in [PR #32](https://github.com/HiroSakuraba/bfs-ng/pull/32). The existing UnifiedV10::format retains the established full two-root-scan allocator. This milestone wires the authenticated Free and DeadList B+trees from [PR #30](F5_2_F5_3_PERSISTENT_SPACE.md) into the actual multi-file, typed-attribute, checksum and snapshot publication path. It is **not** a replacement for production filesystems and does **not** yet eliminate full reference-graph certification.

## Disk profile and safety contract

UnifiedV10::format_indexed(device, volume_id) initializes a previously blank image with at least 256 blocks. A separate manifest generation records:

- metadata_reserve=64, indicating a private **64-block index metadata arena** at physical addresses 5–68 inclusive, separate from the ordinary v10 data and metadata address range beginning at 69.
- TreeRoots.free and TreeRoots.dead_list: authenticated persistent CoW B+trees storing physical free extents and delayed retirements with immutable eligible_after generations and snapshot/global ownership domains.
- free_blocks, pending_blocks and a persistent (sweep_snapshot,sweep_key) *retirement cursor*; the cursor's fields encode the last scanned (dead_generation, physical_start), not a user-visible snapshot ID.

The Free and DeadList trees **never allocate their own pages from the Free index**. Their CoW metadata pages are drawn from the fixed arena only after independently confirming each address is unowned in either recoverable F0 candidate. An arena-full transaction fails with NoSpace without publishing a partial update. Existing v10 non-indexed images are not silently migrated or reinterpreted.

The root-manifest validator treats the two index root addresses differently from ordinary data roots: Free/DeadList roots must live inside the reserved arena, while all normal roots remain outside it. Every page still carries its role, physical address, birth generation, volume ID and authenticated parent pointer. Invalid role/arena boundaries, malformed free ranges, inconsistent counters, overlaps or a corrupt fallback root cause a fail-closed error.

## Publication sequence

Each indexed mutation retains **independent two-F0-root ownership certification** before allocation. It then:

1. Reads the current authenticated Free and DeadList trees and validates ledger contents against all recoverable roots, referenced snapshots, namespace/data/attribute metadata and checksummed data blocks.
2. Visits at most MAX_RETIRE_STEP=32 pending retirement entries from the persisted cursor, wrapping once. Too-young or still-protected entries remain pending. An eligible, unowned run can move into Free with its original absolute epoch.
3. Reserves data and ordinary CoW metadata pages from eligible Free runs, recording their exact removal from the persistent index. An unexpected owner overlap is an immediate corruption error.
4. Records pages exclusive to the older F0 fallback as delayed retirements, separating snapshot-owned file/namespace/attribute metadata from volume-global checksum and snapshot-catalog metadata. The eligibility fence is at least two generations later; actual release still requires an independent ownership check.
5. Stages ordinary data, per-file extents, directory tree, checksum delta, F4 typed attributes/postings and snapshots as needed. It then stages **localized CoW Free and DeadList tree paths** using only independently unowned index-arena addresses.
6. Re-traverses and verifies the complete staged replacement using an in-memory read-only overlay, including exact ledger contents and manifest counters. F0 then syncs staged blocks before publishing the rotating payload and alternate superblock.

No block is reused merely because its checksum is valid, an epoch has elapsed, a snapshot was marked Dropping, or a free-list entry exists. Pinned and Dropping snapshots retain all historical roots until explicit safely verified deletion is implemented.

reclaim_step() is an optional separate mutation that publishes Free/DeadList and cursor changes without modifying file contents. If there is nothing to do it is a no-op.

## Demonstrated scope and limitations

Regression suite: crates/storage/tests/storage_v10_indexed_allocation.rs. It exercises opt-in format/reopen, durable counters/roots, repeated partial CoW writes on the same image, historical pinned content, atomic directory rename and inode-keyed F4 postings, bounded reclaim, two-root fallback corruption, and interrupted data/index/root publication.

This mode improves allocation discovery by using the ordered persistent Free runs rather than scanning the entire image for candidate addresses. **However, every operation still reconstructs the complete selected/fallback ownership graphs**, and the pure Free/DeadList planners materialize their maps in memory before path-copying changed pages. Neither end-to-end logarithmic commits nor fully incremental reference retirement is established. The initial fixed 64-block index arena is a deliberate simplifying constraint, not a scalable metadata-reservation solution.

Next performance/proof gates: incremental reference-count deltas with independent full-oracle cross-checks; a bounded nonrecursive but expandable metadata reservation mechanism; crash-model validation of every free/dead/root split and cursor transition; separate hardware power-cut qualification; performance comparisons on AxiomZig, DVE and AxiomMegaDoc actual read/write traces; and larger streaming extent trees.

See [Architecture](ARCHITECTURE.md), [F5 data-layer design](F5_DATA_LAYER.md), [I2 multi-file namespace](INTEGRATION_I2_V10_NAMESPACE.md), and [I3 full-scan reuse](INTEGRATION_I3_CERTIFIED_REUSE.md).
