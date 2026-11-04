# F2.R2 — incremental CoW reference B+tree and delayed-free epoch fence

**Status:** experimental disposable-image implementation on PR #13, not a finished free-space allocator, online filesystem, or substitute for a clean power-loss test. CI status must be checked before calling it tested.

## Compatibility

The F2/F3/F4 manifest advances from `BFSNGM5\0` (version 5) to `BFSNGM6\0` (version 6). No migration or mixed-version writer exists. Recreate experimental images; never use this code on production data.

The manifest trailer remains three little-endian fields: `reference_root:u64`, `page_count:u32`, `entry_count:u32`, all protected by the existing F0 root-slot checksums and ordering barriers. Its first address now points to a checked **B+tree root**, not a linked page chain.

## Physical reference tree

- Leaf pages: `BFSRLF6\0` magic, `birth_generation:u64`, reserved zeros, CRC32C at byte 24, `record_count:u16` at byte 28, `level:u8=0` at byte 30. Packed sorted `(physical_address:u64, roots:u16)` rows begin at byte 32; maximum 406 per 4 KiB leaf.
- Branch pages: `BFSRBR6\0` magic, the same header with level in 1–8, and packed `(child_max_physical_address:u64, child_page:u64)` separators; maximum 254 children. All unused bytes are zero and included in CRC.
- Old child pages remain immutable and keep their birth generation. New generations copy only modified leaves and ancestor branches. Separators are checked against **actual child subtree maxima**, not trusted on their own.
- Deletes can underfill nodes. Empty leaves are removed and single-child roots can collapse; this is a bounded research tree, not a proven production-grade occupancy-balanced B+tree.
- A new empty reference tree is one checked empty leaf. Index pages are *not* counted in their own postings. They are separately protected by both recoverable F0 manifests.

## Transaction and recovery invariants

1. Use the conservative F2 allocator to reserve all new F3 data and CoW namespace pages without touching **any** block held by either F0 root candidate, including snapshot trees and reference-index pages.
2. Reconstruct the new candidate reference map by reading staged plus persistent F3 pages. Distinct live and pinned roots each contribute at most one count per referenced physical block.
3. Independently read the previous reference B+tree and compute only changed reference records. Path-copy those records into the new index root. Untouched subtrees retain physical page identity, even across F0 generations.
4. Discard all intermediate reference-index path copies staged during the transaction, traverse the final staged-plus-persisted index, and require its logical map to match the freshly rebuilt expected map.
5. Sync all staged pages, then the new manifest payload, and finally the alternate checksummed F0 superblock, in the original two-barrier publication order.
6. On open/recovery, validate candidate page magic, CRC, birth generation, bounds, zero padding, level, shape, strict leaf order, branch separators, page uniqueness, and index-page ownership. **Additionally reconstruct the full live/pinned F3 graph and demand exact reference-map equality**. Reject the candidate on mismatch; use an independently valid prior slot if available.
7. When either F0-valid candidate fails the metadata verifier, **quarantine all previously used blocks from reuse** until a safe rebuild, rather than trusting the surviving candidate as proof of free space.

## Delayed-free epochs

The newly exposed `AllocationReport.delayed_free_blocks` counts blocks owned only by the **older recoverable F0 slot**: not reachable from the newest root but still needed for rollback. Its `delayed_free_eligible_generation` is the earliest generation whose allocator can consider these former fallback-only blocks, **after** a successful intervening root rotation (typically current generation + 2). After a crash, the count and epoch fence are reconstructed from the durable F0 candidates; no external cache or volatile in-memory retirement list is authoritative.

This is a conservative epoch fence **not yet an on-disk delayed-free queue**. The allocator still recomputes both roots' complete reachability, and its reclaimable set contains only physical blocks absent from both. Reclamation cannot run from index-only hints yet.

## Evidence to require before release

- Existing F2.R1 tests: forged checksum-valid reference count, multi-page index, snapshot drop, CRC and structural corruption, F4 catalog, and crash injection across ordered writes.
- New F2.R2 tests: a large reference tree must preserve at least one physically unchanged child after a small write; fallback-only retirement counts must persist across crash/reopen; a checksum-correct forged branch separator must fail recovery.
- Run `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --all-targets` and the existing reference-policy and real-world benchmark workflows.
- Extend future differential fuzzing: mixed insert/delete/overwrite/snapshot pin/drop sequences, root-slot rotation and fault injection at **every** write and sync boundary, B+tree separator/cycle mutation, free-run reallocation, and cross-version denial.

## Explicitly deferred

- **Full-scan allocation removal:** independent F3 graph verification is still performed at every commit and on recovery. Incremental reference-index *writes* do not yet imply incremental end-to-end commits.
- **Crash-resumable reclamation journal and persistent free-run tree:** these require a separately atomic checkpoint and reachability repair strategy, plus a fail-closed mode for torn or stale allocator metadata.
- **F0 root-block exhaustion:** F0 still consumes root payload block `generation + 1`. Redesign into a bounded root area with safe slot rotation, separate CoW payloads and wraparound checks; test it separately from F2.R2 to avoid changing two critical publication invariants at once.
- No online mount, cross-process writer lock, fsck/repair utility, migration, hardware flush validation or production use.

The independent verifier is intentionally retained as the correctness oracle while testing incremental index writes. The next speed milestone is an independently audited durable free-space/retirement ledger that avoids a full graph scan on the normal allocator path without weakening fallback-root safety.
