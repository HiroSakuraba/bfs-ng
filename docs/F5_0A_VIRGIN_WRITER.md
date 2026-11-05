# F5.0a — strictly audited virgin-only v10 snapshot metadata writer

**Status:** experimental implementation, proposed in PR #22. The default `FileSystem` remains **v9**. This is an intentionally limited **v10 snapshot-catalog test writer**, not F5.0b file data, an allocator for existing/free blocks, a mount, or a complete v10 filesystem.

## Supported profile

The v10 manifest has one immutable, empty `TreeId::Fs` leaf and an optional multilevel `TreeId::Snapshots` catalog. Snapshot descriptors may reference only the same static filesystem root, without separate attribute trees. Checksum, free, dead-list, schema and attribute roots are **not permitted** in this mode. The counters, sweep state and reserve values must match this restricted profile. Every write goes through the existing F0 v2 three-payload ring.

`experimental_v10::VirginSnapshotWriter` exposes:

- `format(blank_device, volume_id)`: F0 checks the **entire image is blank**, then publishes a v10 manifest with an empty fs tree using F0's ordered staged-block root commit.
- `open(device)`: select an F0 root and **independently validate both recoverable F0 candidates**, all snapshot-catalog pages, all descriptors and the static fs root before accepting a writer handle.
- `pin_current()`: pin precisely the currently published generation and fs root. Caller cannot provide arbitrary historical addresses or forge attribute roots.
- `mark_dropping(generation)`: change a pin's state to `Dropping` **without removing its descriptor or releasing any block**. True deletion/reclamation belongs to F5.3.
- `snapshots()`, `manifest()`, `into_device()`: bounded inspection, test-image geometry and explicit device ownership.

Read-only `ReadOnlyV10::open` still allows **lazy** verification of the direct manifest roots. That success alone does **not** imply a writer can open. A corrupted fallback-owned snapshot-catalog leaf is a hard error for the strict writer, even if the newest root remains readable.

## Exact allocation policy

**Never allocate a previously published block.** On every mutation, the writer independently traverses both recoverable F0 candidate manifests in their entirety **within its restricted graph**. The verified physical snapshot-catalog page sets and filesystem root are combined; all rooted addresses must be at or above their manifest's published descending high-water mark.

Working from the current root's high-water boundary, the writer scans lower addresses in descending order. Each candidate must be:

1. Block `>=5` and strictly *below* the current published high-water boundary.
2. Absent from the independently reconstructed physical ownership set.
3. All-zero on read. A nonzero block (including an orphan from a failed transaction) is **never reused**; after a successful lower allocation, the high-water mark moves past it permanently.

This third condition does not prove an address is free in an arbitrary filesystem. It is an additional conservative check **after complete graph verification for the narrow snapshot-only layout**. Generic data extents, shared ranges, initialized-zero user files and attribute trees remain explicitly unsupported.

The planner receives up to 35 sorted descending zero-verified addresses, but stages only the pages needed for the changed leaf/branch paths. Unused addresses remain untouched; the published high-water boundary is the lowest **actually used** page. Reaching the bottom boundary returns `NoSpace`, not premature reuse or a cursor rewind. Formatting small experimental images is possible, but they may not have enough room for snapshot edits.

## Publication and crash-safety proof obligations

1. Reconstruct both F0 candidate graphs. A checksummed F0 fallback whose payload or deeper metadata cannot be validated makes the writer refuse mutation.
2. Determine the proposed snapshot edit from a canonical, complete old catalog: duplicate pin rejects; marking an absent/already dropping pin rejects.
3. Select uniquely owned-or-virgin **zero** staging pages below the current high-water mark.
4. Run the CoW path-copy snapshot planner, without any device writes.
5. Build a **read-only in-memory overlay** of all candidate staged pages and independently traverse the complete proposed multilevel snapshot tree; require it equals the exact expected snapshot descriptor list and remains within the restricted graph.
6. Use F0's existing `commit_root_with_blocks`: write all staged pages, sync; write new payload in the rotating root ring, sync; publish alternating superblock, sync. An ambiguous write/sync failure poisons the writer's embedded F0 handle until reopen. Only on successful completion is the new manifest installed.

This intentionally sacrifices space efficiency, and may report `NoSpace` with many old, now-unreferenced blocks on disk. F5.2/F5.3 must independently prove reusable free-space eligibility before that can change. No background reclamation, copy-on-write data extents, or persistent global free tree is implemented here.

## Regression evidence and limits

The `storage_f5_0a_virgin_writer` integration tests cover 100+ genuine root publications with reopening and snapshot splits; immutable and dropping snapshot retention; a nonzero orphan below the watermark; fallback-only catalog corruption; near-full image exhaustion without partial publication; root and staged-page write/sync crash cuts; v9 image rejection; a zeroed historical root; and read-only versus writer validation boundaries.

This is an experimental `CrashDevice` model, **not** physical-media power-cut testing, multiwriter locking, or production crash durability. The strict writer only recognizes a single static empty namespace root. It must not become the default v10 writable filesystem until F5.0b checksum/data retention, general tree ownership, incremental allocation proofs and broader hardening are complete.

Related: [F5.0a on-disk wire format](F5_0A_WIRE_FORMAT.md), [F5 architecture](F5_DATA_LAYER.md), [F0.R1 root publication](F0_R1_ROTATING_ROOTS.md).
