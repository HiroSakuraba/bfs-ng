# F5.3 — Indexed allocator integration, experimental opt-in

**Status:** Implementation and validation in [PR #31](https://github.com/HiroSakuraba/bfs-ng/pull/31). The default BFS-NG v9 filesystem and existing v10 \`UnifiedV10::format\` full-scan allocator are unchanged. **Do not use this indexed profile for irreplaceable files.** The index is an allocation accelerator, not a replacement for independent reachability proofs.

## Format and recovery gate

\`UnifiedV10::format_indexed(device, volume_id)\` requires an otherwise blank device of at least 256 blocks. It first establishes the ordinary v10 F0 generation-1 empty filesystem, then atomically publishes generation 2 with:

- A checksummed, authenticated \`TreeId::Free\` root tracking physical runs and immutable absolute \`eligible_after\` generations.
- A dedicated **64-block index metadata arena** immediately following F0's reserved blocks 0–4; the Free and DeadList B+tree pages must live inside this arena and are **never themselves allocatable from the Free tree**.
- The remaining data region outside the arena, with the generation-1 filesystem root excluded, recorded as the initial free range. No existing image silently upgrades or changes its layout.

A decoded indexed manifest's \`metadata_reserve\` must match the supported fixed arena. Both candidate F0 roots remain independently checked, including exact live, snapshot, attribute, checksum and index metadata ownership, and the Free/DeadList interval/counter invariants.

## Commit ordering

For an indexed write:

1. Independently reconstruct the protected ownership union from both recoverable F0 candidate roots. Every reachable data or metadata address is protected; a corrupt fallback fails closed.
2. Read and verify the authenticated Free and DeadList indexes. Reject overlapping ranges and stale Free claims, **including future-eligible entries**. A page reachable now is never Free. Validate the pending/available counters.
3. Take a bounded amount of retirement work using \`retire_step_cursor\`. The persisted cursor is \`(sweep_snapshot, sweep_key)\`, reinterpreted for this experimental profile as the last visited \`(death_generation, physical_start)\`. A future-ineligible or still-owned record also consumes the visit budget, preventing unbounded scans.
4. Reserve the next data / namespace / checksum / attribute pages from eligible Free runs. Stage them into the existing complete-overlay verifier, without overwriting anything owned by either old F0 candidate.
5. Record newly obsolete **fallback-only** physical pages as delayed retirements, classifying global metadata separately from snapshot-owned blocks. Every retired run has a conservative \`eligible_after >= death_generation + 2\` bound, but this epoch alone never authorizes release.
6. Stage exact localized CoW changes to the Free and DeadList trees, reserving only separately-certified unowned blocks inside the fixed index arena. Reject root or descendant pages outside the arena. The index cannot allocate its own staging pages recursively.
7. Independently traverse the entire staged candidate graph. Check file contents/checksums, snapshot/attribute roots, Free/DeadList entries and counters, metadata/data disjointness, and the expected root state. Finally let F0 publish staged data/index pages, sync, root payload, sync, then alternate superblock, sync.

The older fully audited mode remains available. **This stage does not remove the full ownership audit**, so it reduces allocation discovery work without yet proving logarithmic end-to-end commit cost.

## Reclamation

\`UnifiedV10::reclaim_step()\` is available **only on indexed images**. It visits at most 32 pending records per transition and stores the cursor in the same F0 root as the resulting index roots. It is safe to resume after reopen because a torn or unsuccessful F0 publication cannot independently publish either the new cursor or the new Free/DeadList roots. A Pinned or Dropping snapshot is still a data owner and prevents its blocks from being released.

The index arena itself remains governed by the full two-root owner audit and is not represented by DeadList. Its fixed size and the preliminary in-memory ledger snapshots are deliberate experimental limits.

## Validation and remaining work

\`storage_v10_indexed_allocator.rs\` exercises blank indexed format/reopen, persistent free-root counters, repeated partial overwrites under a retained snapshot, inode stability, bounded retirement, index reopen and crash-cut F0 publication. Regression tests are necessary but not equivalent to real power-loss validation.

Further gates before default enablement: realistic DVE/AxiomMegaDoc workload measurements, sustained fragmentation and index-arena exhaustion studies, independently implemented recovery/repair, epoch wrap/overflow invariants, online large streaming files, durable concurrency/locking and **the eventual elimination of full reference-graph reconstruction** only after a separate accounted-vs-oracle proof.

Related: [F5.2/F5.3 pure tree and retire planners](F5_2_F5_3_PERSISTENT_SPACE.md), [I3 full-scan ownership oracle](INTEGRATION_I3_CERTIFIED_REUSE.md), [v10 unified namespace](INTEGRATION_I2_V10_NAMESPACE.md).
