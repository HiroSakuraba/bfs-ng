# F2.R3 — durable epoch-tagged reclamation journal (experimental)

**Status:** PR #14. Prototype only, not a mountable production filesystem or independently audited free-space allocator. Version-7 images must be recreated; there is no v6 migration.

## Rationale

F2.R2's immutable reference B+tree records the physical pages referenced by the live namespace and pinned snapshots **within one recoverable F0 root**. The previous F0 superblock is also recoverable; a block held only by that fallback must not be recycled until the slot containing it is atomically replaced.

F2.R3 encodes that rotation explicitly in a durable, CRC-checked free-run journal. Each manifest contains an immutable checkpoint with two disjoint classes:

- **Ready**: physical block IDs independently proven absent from both F0 candidates **after** publication of the owner generation; safe to consider on a future commit.
- **Pending**: physical IDs held by the previous F0 candidate but absent from this new root, eligible only after publication of the **next generation**. They are never allocatable during the commit that retires them.

An interrupted commit can restart from the last complete F0 manifest and its journal. Any staged, unpublished pages are ignored or quarantined by the full allocator scan. This supplies a **crash-replayable checkpoint**, not yet a bounded-progress background GC sweeper with a persistent scan cursor.

## Breaking on-disk v7 format

F2/F3/F4 manifest magic is `BFSNGM7\0` and version 7. The existing 64-byte header and 40-byte snapshot entries are unchanged. After snapshot entries come 32 bytes:

| Offset from trailer | Width | Meaning |
| --- | --- | --- |
| 0 | u64 LE | CoW physical-reference root |
| 8 | u32 LE | Reference-tree page count |
| 12 | u32 LE | Reference record count |
| 16 | u64 LE | First immutable free-run journal page |
| 24 | u32 LE | Journal page count |
| 28 | u32 LE | Total free-run records |

Each 4096-byte journal page contains:

| Offset | Width | Meaning |
| --- | --- | --- |
| 0 | 8 | `BFSFRJ7\0` magic |
| 8 | 8 | Birth/owner generation |
| 16 | 8 | Next journal page (0 terminates) |
| 24 | 4 | CRC32C with checksum field zeroed |
| 28 | 2 | Entry count |
| 30 | 2 | Reserved zero |
| 32 onward | 20 per run | `(start:u64, length:u32, eligible_generation:u64)` |

At most 203 runs per page and 2048 pages per manifest. All pages and rows are checked for bounds, strict non-overlap, generation, monotonic physical order, CRC, reserved zeros, zero padding, and cycles. A checked empty journal occupies one page. Eligible-generation values are exactly the owner generation (ready) or owner generation + 1 (pending).

## Publication order and safety

1. Independently reconstruct all physical reachability for **both** recoverable F0 candidates, including pinned snapshots, the reference B+trees, and the journal pages themselves.
2. Fail closed if any F0-valid candidate has an unverified manifest or journal; a checksum-valid F0 superblock with a **corrupt root payload** is no longer silently omitted.
3. Allocate new F3 data, CoW namespace and index pages only from already-verified free blocks or virgin high-end space. The previous fallback's exclusive pages are **not** added to the allocator pool.
4. Compute new reference-tree ownership. Calculate the new journal's ready blocks from remaining old safe free space plus older-fallback-exclusive pages that will cease being recoverable **only after** this publication. Calculate pending blocks from physical pages belonging to the prior head but not the new head.
5. Reserve journal pages from the **preexisting** safe pool, never from newly retired blocks. Recalculate free runs after reservations; reserve another journal page if splitting a free run increases run count. This breaks the self-reference/space-accounting cycle.
6. Verify the staged free-run journal against the new live namespace and reference-tree pages, stage immutable journal pages, then use F0's ordered writes and sync barriers to publish a single manifest containing namespace, snapshots, reference root, and journal root.
7. On reopen, verify journal page integrity, non-overlap with current F3 and reference pages, and the independently reconstructed tree counts. During the two-candidate allocation scrub, require the newest journal's **ready** set to be a subset of actual free blocks across both roots. A checksum-valid forged early promotion is quarantined, not recycled.

The journal can conservatively under-report actually free blocks if temporary, never-published CoW path copies were reserved. An independent scrub can recover them, and this is intentional until a complete allocator-transaction log exists.

## Observable diagnostics

`AllocationReport` now exposes `reclaim_journal_pages`, `reclaim_journal_runs`, `journal_ready_blocks`, and `journal_pending_blocks` alongside F2.R2's `delayed_free_blocks` and `delayed_free_eligible_generation`.

## Tests

`cargo test -p bfs-ng-storage --test storage_f2_r3` exercises manifest restart, pending-to-ready rotation, checksum-tampered journal rollback, checksum-valid forged early promotion (quarantined), CRC-corrupt older F0 root payload (quarantined), snapshot pin/drop with fallback, and torn journal/root publication. The entire workspace and benchmark workflows remain required.

## Remaining work

1. **Full allocation scans still happen.** The journal is a durable, independently checked witness, not a trusted fast allocator or proof that end-to-end commits are incremental. On each normal commit BFS-NG still verifies both F0 graphs.
2. **Crash-resumable multi-batch sweeping is not yet present.** No persistent sweep cursor/frontier, checkpointed mark bitmap or bounded-resume GC worker; interruption atomically replays the last completed journal instead.
3. The free-run journal itself is rewritten per generation. A future incremental range B+tree/bitmap representation should eliminate that cost.
4. F0 still spends root payload block `generation+1`. A circular/root-region allocator with generation/lease validation and its own power-fail proof is a separate milestone.
5. No raw-device flush qualification, cross-process exclusive lock, online mount, defragmentation or migration.

The next optimization should use **verified generation deltas** plus a replayable transactional reservation journal to make the common allocation path scan-free, with full-graph checks retained for scrub/CI and fail-closed recovery.
