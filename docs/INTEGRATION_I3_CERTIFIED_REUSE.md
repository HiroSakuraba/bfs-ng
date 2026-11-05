# Integration I3 — fully certified v10 reuse (conservative full-scan mode)

**Status:** Experimental implementation in [PR #28](https://github.com/HiroSakuraba/bfs-ng/pull/28). This is the first integrated v10 multi-file writer that can **recycle physical data and metadata pages** without consuming a new virgin block every generation. It does **not** implement the incremental F5.2 free-space B+tree, F5.3 dead-list/absolute eligibility epochs, multiwriter locking, or production durability.

## Source of reuse authority

Before *every* mutation, \`UnifiedV10\` runs the full independent F0 two-candidate audit already required by I2. This reconstructs all namespace paths, per-file extent roots, physical block births, exact checksum coverage, pinned or dropping snapshot roots, snapshot-catalog nodes and shared unchanged metadata subtrees. No physical allocation is attempted if either still-checksummed fallback candidate has an invalid or incompletely verified graph.

An address is **eligible** only if it is greater than F0's reserved range 0–4, inside the current image geometry and **absent from the full union of both recoverable F0 candidates' owned graphs**. This set can include uninitialized zeros, old superseded data/metadata and unreferenced crash-orphan pages. Rewriting nonzero orphans is safe *under this closed-world independent reachability model*, but is not authorized by a CRC check or by simply seeing a zero page.

Every chosen candidate address is unique and is checked against the old ownership set again at publication. Public generic \`stage_namespace_tree\`, \`stage_checksum_delta\` and \`plan_snapshot_edit\` retain **virgin-only** reservation constraints. Separate crate-private audited-reuse entrypoints are invoked only after the writer's independent both-F0 ownership scan.

The virgin high-water boundary cannot increase: after publication, \`high_water = min(old_high_water, lowest_staged_block)\`. Recycling a high-end block does not incorrectly authorize a lower address or discard the old boundary. Changed data, namespace nodes, checksum paths and snapshot nodes still go through the complete staged-graph overlay check followed by F0's write/sync/payload/sync/superblock/sync barriers.

## Tests and diagnostic

\`UnifiedV10::last_reused_blocks()\` reports how many physical pages from the last successful commit contained nonzero bytes **before** the verified overwrite. It is a diagnostic, not proof of safety; the underlying proof obligation is the exact two-F0-owner set.

\`cargo test -p bfs-ng-storage --test storage_v10_certified_reuse\` covers:

- 300+ consecutive file overwrites on a 56-block image with a pinned historical root retained and checked after repeated reopen;
- An unreferenced nonzero orphan that is rewritten *only after* a complete independent graph audit;
- Directory relocation, pinned historical paths and dropping-state retention across further reuse;
- A genuinely exhausted small image with numerous pinned versions: \`NoSpace\` must not damage the last successfully committed state;
- Torn staged-data/metadata/F0-root publications on recycled pages, and refusal to proceed if a fallback graph is corrupted.

This deliberately retains **O(number of volume blocks + all reachable metadata/data)** validation work per write and an in-memory ownership set. It is functionally useful for repeated edits but computationally expensive. Benchmarks must measure real read amplification, certification latency and compare it to v9 F2.R4 and ordinary file systems before claiming performance advantages.

## Remaining gates

The efficient alternative remains **F5.2**: a checksummed CoW free-range index with immutable absolute eligibility epochs and a bounded, nonrecursive metadata reservation proof. **F5.3** must prove snapshot-owned versus volume-global retirement and crash-resumable reclamation against an independent ownership oracle. Until then, do not describe this mode as scan-free, logarithmic, hardware-power-cut-certified or suitable for irreplaceable files.

The integrated engine still has per-file 256 KiB extent-leaf limits, non-incremental namespace rebuilds, and no F4 typed searchable attributes. The v9 engine remains the existing compatibility fallback. See [I2 architecture](INTEGRATION_I2_V10_NAMESPACE.md), [F5 design](F5_DATA_LAYER.md), and [v9 rename bridge](INTEGRATION_I1_NAMESPACE_IO.md).
