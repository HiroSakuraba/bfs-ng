# F5.2/F5.3 — Persistent free runs and delayed retirement

**Implementation status:** First code tranche in [PR #30](https://github.com/HiroSakuraba/bfs-ng/pull/30). The authenticated on-disk `Free` and `DeadList` B+trees, localized CoW update planner, absolute eligibility epochs and a bounded retirement transition exist. **This PR is a data-structure and safety-oracle foundation; the integrated writer still uses its full two-root scan allocator until the next integration/recovery gate passes.** Do not claim scan-free or crash-resumable reclamation solely from a pure planner.

## Physical wire format

All nodes use the existing v10 4 KiB `Page` / 24-byte authenticated `BlockPointer` contract, with a canonical sorted high-key branch tree and immutable address/birth identity.

| Tree | Leaf key (BE) | Leaf value (LE) |
| --- | --- | --- |
| `TreeId::Free` | physical start `u64` | length `u32`, reserved `u32=0`, **absolute** `eligible_after_generation:u64` |
| `TreeId::DeadList` | death generation `u64` + physical start `u64` | length `u32`, domain `u8`, reserved 3 bytes, **absolute** `eligible_after_generation:u64` |

Domains: `Snapshot` indicates logical namespace/extent/data that a pinned generation may own; `Global` indicates a volume-wide checksum, free-tree or retirement-index page. Domain is **not** an authorization to reclaim without independent ownership verification. Dead entries require `eligible_after >= dead_generation + 2`, conservatively protecting the old F0 fallback through a further publication. Adjacent free runs may be coalesced **only if they have identical absolute eligibility epochs**. The index does not restamp old leaves on each generation.

The authenticated decoder rejects duplicate pages and entries, out-of-bounds intervals, overlapping runs, noncanonical equal-epoch neighbors, malformed domain flags, invalid absolute epochs, unsorted keys, nonmatching child high keys, excessive depth and unsupported page layouts.

## Fast lookup / CoW path copy

`reserve_free` performs a run-index lookup for an exact number of available blocks, choosing from eligible runs and checking each against the independently certified both-F0-owner set. It produces the **exact residual run map** and a descending list of physical addresses for metadata/data staging. An incomplete allocation returns `NoSpace` without publishing a partial list.

`stage_space_delta` updates the canonical Free or DeadList tree by path copying only the affected leaves and branch ancestors. Untouched physical nodes are shared. The public planner is nonpublishing: the caller must verify all reservations, independently validate the newly staged result using a read-only overlay, and publish both roots in the same F0 transaction. Until the reservation-bootstrap proof is implemented, neither the free index nor its staged pages are allowed to overwrite any block reachable from *either* recoverable candidate.

## Retirement

`retire_step(free, dead, protected, generation, budget)` checks at most **32 eligible retirement records** per call. It never releases a retired span if even one physical block remains in the externally supplied current/fallback/snapshot owner set. Successful releases move their exact physical interval into the available tree with unchanged absolute epoch and remove the old dead entry. The resulting changes must be published atomically with the F0 manifest and the new free/dead roots; an interrupted commit must replay entirely from the prior root. A persisted sweep cursor and page reservation policy are still required for efficient progress under blocked early records.

**Important:** ownership-domain assignment cannot be inferred solely from block birth or global page generation. Snapshot-owned trees and volume-global metadata have different lifetime rules. No snapshot may be charged every retired global free-tree page merely because it existed while the snapshot was pinned.

## Proof gates before activating integrated allocation

1. Independently certify the selected and fallback F0 root graphs, all pinned namespace/attribute/data roots and exact checksum coverage.
2. Decode both persisted space trees and verify no run overlaps protected owners when eligible, or contradicts the pending retirement list.
3. Reserve a bounded, nonrecursive metadata staging arena so that the free-space tree cannot allocate the very pages it is staging.
4. Produce exact old/new ledger deltas, stage CoW paths, traverse the complete staged graph, and use F0 write/data/payload/root sync barriers.
5. Add restart/crash-cut tests for pending/eligible transitions, every allocator-page split, sparse fragmentation, independent oracle agreement, snapshot middle drops and delayed global-page retirement.
6. Keep `Full` authoritative until the optional `Accounted` mode passes the model-checking and fuzz gates from [F5 design](F5_DATA_LAYER.md). Do not eliminate the full ownership scan merely because the persistent free index is valid.

Tests: `crates/storage/tests/storage_v10_space_index.rs`. See [I3 full-scan safe reuse](INTEGRATION_I3_CERTIFIED_REUSE.md).