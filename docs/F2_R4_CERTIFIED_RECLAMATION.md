# F2.R4 — certified allocation fast path and crash-resumable bounded reclamation

**Historical v8 description:** F0.R1 subsequently replaced monotonic root-payload allocation with a fixed three-block CoW ring and moved the root manifest to version 9. See [F0.R1](F0_R1_ROTATING_ROOTS.md). References below to F0's monotonic root-payload exhaustion describe the v8 baseline.
**F0.R1 correction:** The v9 filesystem also reclaims *never-published* intermediate namespace/reference CoW pages into the durable ready set before journal publication. They do not require fallback retirement; this prevents virgin-cursor exhaustion from discarded staged allocations. See [F0.R1](F0_R1_ROTATING_ROOTS.md).


**Status:** experimental implementation in PR #15. Version 8 is a breaking disposable-image format; recreate v7 images. The journal is still a sorted page chain rewritten each generation. F0 still spends the low-end root-payload block `generation + 1`; this is not a production filesystem.

## Why the change

F2.R3 correctly protected both recoverable F0 candidates, but each `commit()` called `allocation_state()`, reconstructing *both* F0 roots and all snapshot trees before choosing a single reusable block. Its journal also promoted all matured fallback-only blocks at once.

F2.R4 splits **certification** from **ordinary allocation**:

1. On the first mutating commit after format/open, independently traverse both F0 candidate roots, snapshot catalogs, reference indexes and immutable journal pages. Recheck each journal's integrity. A damaged/unknown F0 candidate prevents reuse.
2. Build an in-memory certificate scoped to the exact selected F0 generation: ownership sets for latest and fallback, checked `ready`, `pending`, `deferred` runs, and the durable sweep cursor. `ready` must be a subset of the independently verified two-root free set; `deferred` must also be free. The persisted `pending` set must **exactly match** fallback-owned minus newest-owned blocks.
3. Subsequent commits in the **same single-writer handle** allocate only from the certified `ready` set (or virgin blocks below the descending high cursor). They do **not rerun the independent two-root allocation scrub** on every commit.
4. For each prospective transaction, reconstruct the new live/snapshot reference counts and check the CoW reference B+tree as before. This is still a **full new-generation reference traversal**; F2.R4 eliminates the extra *two-root allocator* traversal, not every metadata read.
5. Compute newly pending blocks from `old_latest_owned - new_candidate_owned`. Mature the previous pending set only when the old fallback F0 candidate is rotated out; those matured blocks go into the untrusted-for-allocation **deferred** queue.
6. Promote at most **32 deferred blocks** to the new journal's `ready` class. Check each selected address against both ownership sets that will survive the publication. Remaining deferred blocks and the physical continuation cursor are serialized. Newly promoted blocks are **not** available to the current `Builder`; they become usable only after F0 publishes their journal.
7. Stage namespace, reference and journal pages; independently decode the staged journal; then publish them through F0's ordered write/sync/root-slot/sync transaction. Advance the in-handle certificate **only after successful final publication**. A failed or poisoned write requires reopening; never carry an uncommitted certificate across recovery.
8. On reopen, run independent verification again before trusting the journal for block reuse. Checksummed but semantically forged ready/deferred/pending entries are quarantined by the full-scan oracle.

The explicit `FileSystem::reclaim_step()` publishes a journal-only generation even without namespace changes. Each call sweeps one bounded budget and persists the cursor; interruption returns to the last complete root, not a partial retirement decision. This operation still consumes journal blocks and an F0 payload generation and should be scheduled deliberately.

## Disk layout v8

Manifest magic `BFSNGM8\0`, version 8. Snapshot entries and the 16-byte physical-reference trailer retain their v6/v7 meaning. Journal trailer is now 24 bytes, so total trailer size is **40 bytes**.

| Offset from snapshot trailer | Width | Meaning |
| --- | --- | --- |
| 0 | u64 | CoW reference B+tree root |
| 8 | u32 | Reference pages |
| 12 | u32 | Reference entries |
| 16 | u64 | First free-run journal page |
| 24 | u32 | Journal pages |
| 28 | u32 | Journal runs |
| 32 | u64 | Persistent physical `sweep_cursor` (0 means start) |

Journal page magic `BFSFRJ8\0`, same 4KiB checked-page format as v7, with 20-byte sorted `(start:u64, length:u32, eligible_generation:u64)` records:

| Eligible value | Meaning |
| --- | --- |
| current manifest generation | **Ready:** certified across both roots on next open, available on a subsequent allocation |
| current generation + 1 | **Pending:** still referenced by the latest root's fallback, never allocatable before that candidate rotates away |
| 0 | **Deferred:** released by a previous rotation but held until the bounded sweep independently checks it |

All runs remain sorted, disjoint, bounds-checked and CRC32C-protected. The cursor is checked against the addressable region; rolling back the F0 root automatically rolls back its journal and cursor.

## Public diagnostics

- `FileSystem::certification_scans()`: independent two-root allocation scans performed to bootstrap commit-time certificates on this handle (does not count explicitly requested allocation reports).
- `FileSystem::last_commit_used_certificate()`: true when the last successful commit used an already installed generation-matched certificate.
- `FileSystem::reclaim_step()`: one explicit transaction to advance at most 32 queued physical retirements.
- `AllocationReport::journal_deferred_blocks`: persisted safe-but-not-swept blocks; other F2.R3 ready/pending and reference metrics remain available.
- `allocation_report()` continues to perform an **independent full two-root scan** and is the read-only audit/checkpoint oracle. It does not accept cached ownership as proof.

## Regression suite

`cargo test -p bfs-ng-storage --test storage_f2_r4` covers:

- No extra full two-root allocator scans on repeated mutations within a certified handle; mandatory re-certification after power-loss/reopen.
- Delayed retirement of more than 32 blocks; persisted progress through `reclaim_step()` across a fresh open.
- Checksum-correct forged premature quarantine of a block still held by fallback (must disable all reuse).
- Torn writes at journal and F0 publication boundaries; selected root and cursor always describe the same complete generation.

F2.R2/R3 tests still cover snapshot drops, reference B+tree physical sharing, independent reference recomputation, corrupted-journal fallback and damaged F0 root payloads. All CI workflows remain mandatory before merge.

## What is **not** finished

- Per-commit `count_roots` and staged index-map checks still read complete live/snapshot graphs. True **incremental end-to-end reference accounting** is a separate proof and optimization milestone.
- The sorted full-run journal is still **rewritten in each published generation**. Persistent CoW run-tree/bitmap pages, localized updates and run coalescing would reduce I/O.
- A bounded sweep is driven by `reclaim_step()`/ordinary commits, not a background worker. The v8 cursor records progress across *completed generations*, not crash-resumption in the middle of one uncommitted operation.
- The in-handle cache assumes a single writer, no uncoordinated raw-device writes, and stable media until I/O errors are reported. After reopen the complete two-root audit is required.
- Free blocks missing from the prior journal (e.g. orphaned, unpublished CoW intermediates) are intentionally not harvested into the fast allocator by this tranche. A full scrub can detect them; reclaiming them needs its own explicitly verified on-disk reconciliation protocol.
- F0's monotonic low-end root payload allocation still limits generations. Implement its reusable two-root protected payload allocator separately, with power-fail fault tests.
- No production-grade exclusive image locks, online mount, hardware crash qualification, or format migration.
