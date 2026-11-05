# F5.3 — Indexed allocator integration, experimental opt-in

**Status:** Merged experimental indexed writer in [PR #31](https://github.com/HiroSakuraba/bfs-ng/pull/31). The default BFS-NG v9 filesystem and existing v10 `UnifiedV10::format` full-scan allocator are unchanged. **Do not use this indexed profile for irreplaceable files.** The index is an allocation accelerator, not a replacement for independent reachability proofs.

## Format and recovery gate

`UnifiedV10::format_indexed(device, volume_id)` requires an otherwise blank device of at least 256 blocks. It first establishes the ordinary v10 F0 generation-1 empty filesystem, then atomically publishes generation 2 with:

- A checksummed, authenticated `TreeId::Free` root tracking physical runs and immutable absolute `eligible_after` generations.
- A dedicated **64-block index metadata arena** immediately following F0's reserved blocks 0–4; the Free and DeadList B+tree pages must live inside this arena and are **never themselves allocatable from the Free tree**.
- The remaining data region outside the arena, with the generation-1 filesystem root excluded, recorded as the initial free range. No existing image silently upgrades or changes its layout.

A decoded indexed manifest's `metadata_reserve` must match the supported fixed arena. Both candidate F0 roots remain independently checked, including exact live, snapshot, attribute, checksum and index metadata ownership, and the Free/DeadList interval/counter invariants.

## Commit ordering

For an indexed write:

1. Independently reconstruct the protected ownership union from both recoverable F0 candidate roots. Every reachable data or metadata address is protected; a corrupt fallback fails closed.
2. Read and verify the authenticated Free and DeadList indexes. Reject overlapping ranges and stale Free claims, **including future-eligible entries**. A page reachable now is never Free. Validate the pending/available counters.
3. Take a bounded amount of retirement work using `retire_step_cursor`. The persisted cursor is `(sweep_snapshot, sweep_key)`, reinterpreted for this experimental profile as the last visited `(death_generation, physical_start)`. A future-ineligible or still-owned record also consumes the visit budget, preventing unbounded scans.
4. Reserve the next data / namespace / checksum / attribute pages from eligible Free runs. Stage them into the existing complete-overlay verifier, without overwriting anything owned by either old F0 candidate.
5. Record newly obsolete **fallback-only** physical pages as delayed retirements, classifying global metadata separately from snapshot-owned blocks. Snapshot-catalog pages and checksum-index pages are **global**; namespace, file-data and historical attribute roots are **snapshot-owned**. Both domains remain subject to independent reachability checks. Every retired run has a conservative `eligible_after >= death_generation + 2` bound, but this epoch alone never authorizes release.
6. Stage exact localized CoW changes to the Free and DeadList trees, reserving only separately-certified unowned blocks inside the fixed index arena. Reject root or descendant pages outside the arena. The index cannot allocate its own staging pages recursively.
7. Independently traverse the entire staged candidate graph. Check file contents/checksums, snapshot/attribute roots, Free/DeadList entries and counters, metadata/data disjointness, and the expected root state. Finally let F0 publish staged data/index pages, sync, root payload, sync, then alternate superblock, sync.

The older fully audited mode remains available. **This stage does not remove the full ownership audit**, so it reduces allocation discovery work without yet proving logarithmic end-to-end commit cost.

## Reclamation

`UnifiedV10::reclaim_step()` is available **only on indexed images**. It visits at most 32 pending records per transition and stores the cursor in the same F0 root as the resulting index roots. It is safe to resume after reopen because a torn or unsuccessful F0 publication cannot independently publish either the new cursor or the new Free/DeadList roots. A Pinned or Dropping snapshot is still a data owner and prevents its blocks from being released.

The index arena itself remains governed by the full two-root owner audit and is not represented by DeadList. Its fixed size and the preliminary in-memory ledger snapshots are deliberate experimental limits.

## #34 next gate: incremental allocation certification (shadow and memory-only lease)

**Default remains fully audited.** The indexed writer now constructs a `TwoRootOwners` allocation certificate from the two independently verified F0 owner graphs. The certificate represents the currently selected graph and the physical pages reachable **only** from the fallback graph. Its transition models the next F0 rotation: new candidate as head, old head as fallback, and old fallback released only after a matching DeadList retirement witness. All staged block addresses are checked against the *old* two-root union; every newly reachable address must be staged; volume-global metadata cannot silently change into snapshot-scoped ownership; and the new Free tree must remain disjoint from the next two-root union. Every transition is cross-checked against the full-graph oracle before root publication.

The certificate also carries the last audited F0 generation. Any skipped or repeated F0 generation fails closed before retirement or allocation. Its bounded retirement-witness test validates the entire Free/Dead interval set, then uses physical-start ordering for indexed witness lookup rather than re-scanning every pending interval for every retired block. A rejected edit does not advance the in-handle certificate.

A **restricted in-handle experiment**, `UnifiedV10<CrashDevice>::enable_in_handle_allocation_certificate()`, uses a cached, previously fully audited two-root certificate to skip re-traversing the *old* graphs on successive writes **inside one exclusively owned deterministic memory-backed device handle**. Each new candidate graph is still independently verified in the staged CoW overlay, and the cache advances only after successful F0 publication. There is no durable lease: consuming the handle or reopening after any power failure drops the cache, requiring full independent F0 verification again. This method is intentionally **not available on FileDevice**; ordinary `UnifiedV10::open` and all file-backed commits retain the full audit.

Validation includes a slow-vs-cached identical-workload comparison with physical-read counters, persistent snapshot and historical attribute equality after power-loss reopen, and deterministic torn-write recovery cuts. This is a demonstration that **old-root allocation certification** can be incremental under the test device's exclusive-ownership assumption, not proof that physical disks cannot change outside BFS-NG. The complete new candidate-graph walk remains, as do unresolved on-disk owner-ledger persistence, exact kill-site completeness, corruption detection without rescanning the old graph, lock/exclusive-writer enforcement and a full Accounted-vs-oracle state-machine proof. Do not extrapolate CrashDevice read counts to hardware throughput, and do not enable the memory-only optimization for normal images.

## Checkpoint recovery experiment (not yet on-disk authority)

A canonical `BFSNGAC1` ownership-checkpoint sidecar serializes the latest audited F0 generation, volume ID, canonical manifest checksum, and four sorted physical-address sets: selected owners, selected-global owners, fallback-only owners, and fallback-global-only owners. A length cap and whole-record CRC32C reject malformed, truncated, reordered, or tampered records. `CrashDevice` exposes `export_allocation_checkpoint()` and `open_with_verified_allocation_checkpoint()` for testing. **Reopening still performs a complete fresh two-F0-root audit before comparing a supplied checkpoint against actual disk ownership.** A valid CRC32C alone never authorizes a block release. Tests cover matching recovery, stale checkpoints after F0 advancement, different images with the same volume ID, damaged sidecars, snapshot retention, and continued writes.

This is an encoding and recovery-validation prototype, not an atomically F0-published persistent ledger: there is no on-disk checkpoint root, no durable kill-event log, no safe `FileDevice` fast path, and no elimination of the required re-audit after a crash. A future format revision must persist both the owner's graph identity and atomic delta history, and must prove restart and partial-publication safety before considering a faster reopen.

## F0-published owner/delta receipt — bounded opt-in

The `CrashDevice`-only `enable_f0_published_allocation_receipts()` opt-in appends a versioned `BFSNGAR1` owner receipt to the 292-byte v10 manifest in the same root payload. The record contains a canonical four-set owner checkpoint, parent-manifest CRC, and ordered physical allocation and retirement deltas. CoW data, root payload and F0 slot share the usual durable publication barriers. `reopen_with_verified_f0_receipts()` checks the entire current and fallback graph against the receipt **before** re-enabling cached allocation; a root CRC or receipt alone is not authorization. Incorrect, out-of-range, oversized or damaged receipts fail closed; the fixed F0 payload cap causes `TooLarge` rather than silently truncating.

This is an explicit experimental extension to v10 root-payload length, not a new production format: older strict decoders reject the extension. It does not provide a persistent Merkle-verified ownership tree, a complete durable kill-event log, or fast reopening without independent traversal. Normal `FileDevice` writes remain on the full audit. Moving beyond the bounded prototype requires an on-disk CoW ledger root and a separately tested, versioned format migration.

## Validation and remaining work

**Post-#33 boundary hardening (experimental):** The DeadList codec and bounded in-memory retirement planner reject death generations that cannot represent a full two-generation absolute eligibility fence (checked addition, never saturation). A separate deterministic small-state oracle exhausts cursor positions, scan budgets, owner masks and eligibility generations. A staged-tree test also asserts that insufficient index-page reservations return `NoSpace` without touching device blocks. These checks constrain the existing implementation; they **do not** establish full F5.3 Accounted ownership correctness, unlock the 64-page reserve, or eliminate the complete two-F0-root graph audit.

`storage_v10_indexed_hardening.rs` adds fallback Free-root corruption, pinned-snapshot endurance on smaller volumes, historical F4 typed-attribute searches after rename, exact retirement-counter/cursor persistence, and repeated reclaim/reopen regression coverage. The original `storage_v10_indexed_allocator.rs` exercises blank indexed format/reopen, persistent free-root counters, repeated partial overwrites under a retained snapshot, inode stability, bounded retirement, index reopen and crash-cut F0 publication. Regression tests are necessary but not equivalent to real power-loss validation.

**Two-root oracle follow-up:** `storage_v10_two_root_oracle.rs` independently parses both F0 superblock/payload slots, traverses all live and pinned namespace/file/extents, checks data versus the physical checksum index, follows historical attribute/schema trees, includes Free/DeadList and snapshot-catalog metadata pages, and rejects any Free interval intersecting either root's physical ownership union. A deterministic 96-step mixed workload exercises sparse writes, truncation, subtree/name changes, three retained snapshots (including a Dropping middle snapshot), attribute edits, reclaim, and power-loss reopen. A second per-transition test compares the physical ownership union before and after each F0 publication; every data-region block that loses its last F0 graph owner must still have a persisted delayed-retirement witness in DeadList (the separately managed index arena is excluded). The test intentionally uses the public canonical page readers; it is an independent **graph assembly** and Free-index comparison, not an independent on-disk codec implementation or a proof of incremental kill completeness. Both F0 manifest sets remain audited on the production path.

Further gates before default enablement: realistic DVE/AxiomMegaDoc workload measurements, sustained fragmentation and index-arena exhaustion studies, independently implemented recovery/repair, epoch wrap/overflow invariants, online large streaming files, durable concurrency/locking and **the eventual elimination of full reference-graph reconstruction** only after a separate accounted-vs-oracle proof.

Related: [F5.2/F5.3 pure tree and retire planners](F5_2_F5_3_PERSISTENT_SPACE.md), [I3 full-scan ownership oracle](INTEGRATION_I3_CERTIFIED_REUSE.md), [v10 unified namespace](INTEGRATION_I2_V10_NAMESPACE.md).
