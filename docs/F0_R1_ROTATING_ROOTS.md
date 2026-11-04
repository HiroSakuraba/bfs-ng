# F0.R1 — rotating root payload allocator (experimental)

**Status:** implementation milestone, disposable single-writer images only. On-disk F0 root-superblock version **2** and F2/F3 root-manifest version **9**. No in-place migration from F0 v1 / F2 v8.

## What was wrong

F0 v1 chose each committed root-payload address by `block = generation + 1`. Each transaction consumed another permanently reserved low block. Eventually the low boundary collided with the F1/F2 descending data/metadata cursor; no amount of correct snapshot reclamation could make the filesystem continue. R4's in-memory certified allocator accelerated the free pool but could not fix this entirely separate F0 exhaustion path.

## Allocation rule and safety argument

F0 v2 permanently reserves **five** initial 4KiB blocks:

| Physical block | Role |
| ---: | --- |
| 0 | Superblock A, generations 0, 2, 4, ... |
| 1 | Superblock B, generations 1, 3, 5, ... |
| 2–4 | Three rotating CoW root-payload blocks |
| 5 onward | F1/F2 data, namespace trees, CoW reference index, and reclaim journal |

For `generation >= 1`, select

```text
payload_block(generation) = 2 + ((generation - 1) mod 3)
```

Generations 1, 2, 3, 4, 5, 6, ... therefore use blocks **2, 3, 4, 2, 3, 4, ...**. The selected root payload is not the payload block of either the latest or fallback recoverable F0 generation, provided those generations are consecutive. The superblock parser checks generation parity, the root-payload block dictated by the v2 rule, CRC32C, volume ID, and image size. If both F0 slots decode structurally, the mounter also requires consecutive generations; a checksum-valid but inconsistent pair fails.

**Two-root invariant:** Before publishing g+1, the recoverable superblock candidates are g and g−1, whose payloads occupy the other two ring blocks. The third block currently belongs to g−2, which was already rotated out of both root slots. Its contents can safely be replaced by g+1's new immutable payload. The new payload is synced before either superblock is overwritten. If publication fails, the g and g−1 payloads remain untouched, and recovery selects the newest complete root. If the new superblock is published, the pair becomes g+1 and g and both still have intact payloads.

The same argument holds after reopening the older root when the newer root slot or payload is damaged: the new generation uses its own deterministic block, never the selected older root's payload. As in F2.R4, **a checksum-valid F0 candidate with an invalid higher-level tree must not authorize high-region reuse**. Root-payload safety does not eliminate the independent F2 quarantine requirement.

## Commit ordering

1. Allocate new F1/F2 immutable data, namespace nodes, reference-index and journal blocks **only at addresses >=5**, from the certified free pool or the descending high cursor.
2. Write staged CoW data/metadata and **sync**.
3. Select the next unused root-payload ring position and write the complete 4KiB root payload with `BFSNGR0\0`, payload length, padding, and CRC pointer.
4. **Sync** the new root payload. Until its superblock appears, the prior root remains authoritative.
5. Write one alternating F0 superblock with the new generation, deterministic ring pointer, payload CRC, and superblock CRC32C.
6. **Sync** the superblock; only then report a successful commit and advance the F2.R4 in-handle allocation certificate.

F0-only `Volume::commit_root()` uses steps 3–6. F2/F3 `commit_root_with_blocks()` fences the staged blocks before the F0-only sequence. The existing `CrashDevice` model can inject torn data and interrupted sync at each operation; it is **not a hardware power-cut test**.

## Format transition and verification

- Superblock version at byte offset 8 changes **1 → 2**; the superblock still uses magic `BFSNGF0\0`. The root payload body retains the F0 `BFSNGR0\0` encoding.
- Filesystem manifest changes from `BFSNGM8\0`/format 8 to `BFSNGM9\0`/format 9, with the same F2.R4 reference and sweep fields. The reference index and journal still have their independently checked v6/v8 page formats.
- Every data extent, namespace node, tree-child pointer, reference page and journal page must be **strictly above block 4**, rather than strictly above `generation + 1`. Cursor bounds, birth generations, CRCs and cycle checks remain.
- `FileSystem::allocation_report().fresh_blocks_for_next_commit` now describes the physical virgin region above the fixed reservation, not how many low-end payload generations remain. `root_generations_remaining` counts remaining F1-compatible `u32` generations, **not** remaining reserved low-end disk blocks.
- Minimum image geometry is five blocks for F0 alone. F1/F2 needs additional blocks above 4 and may still run out of capacity based on workload and free space.
- **Old v1/v8 test images are rejected, not silently remapped.** Recreate a disposable image to use v2/v9.

## Additional F2.R4 staging-accounting correction

The new >image-blocks endurance workload revealed a separate, pre-existing
F2.R4 leak: CoW namespace and physical-reference index updates may create
several intermediate pages before retaining only the final reachable path.
The final staged write set correctly discarded those pages, but the certified
allocator failed to return their *reserved addresses* to the durable ready
pool. Independent full-tree scrub saw abundant unowned space even as the
fast allocator exhausted its virgin cursor.

F0.R1 therefore also returns the addresses of discarded, **never-published**
staged nodes to `Builder.reusable` in both `discard_superseded_nodes` and
`discard_intermediate`, *before* the journal is built. Those addresses were
allocated from a previously certified ready set or virgin space and were
not referenced by either recoverable F0 candidate. The staged reachable
namespace and reference trees are verified before publication, and the new
journal is independently decoded and later full-scrub-checked. Discarded
pages can safely appear as ready immediately; unlike pages retired from an
**already published** F0 candidate, they need no fallback-rotation epoch.

This means a v9 journal may contain ready blocks even at generation 2,
while g1's fallback-owned physical blocks remain pending until rotation.
Tests now identify forged *pending* entries rather than assuming the first
journal entry must be pending, and distinguish newly promoted sweep blocks
from previously proven, never-published staged blocks.

## Tests

- `storage_f0`: 240 root-payload publications on a **five-block** image; both root candidates remain recoverable across repeated ring rollover. Corrupt latest superblocks and latest payloads after rollover and verify clean fallback. Inject torn writes/sync failures at all four F0 publication steps after the first wrap.
- `storage_f0_rotation`: explicit checksum-correct old-F0-format rejection; **440 F2.R4 filesystem generations on a 384-block image** with repeated power-loss reopen, independent scrubs, CRC-protected reference counts and journal checks; deterministic interrupted/torn F2 root publication after multiple wraparounds.
- Existing F1/F2/F3 tests continue to cover snapshot retention, epoch fences, reference-index ownership, document/asset benchmark corpora, and one-generation fallback.

Verify with `cargo test --workspace --all-targets`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo fmt --all -- --check`, plus the three external benchmark/reference CI workflows.

## What remains

This milestone **does not** turn F2.R4 into a fully incremental end-to-end filesystem. Each commit still reconstructs the new live/snapshot reference counts; a sorted free-run journal is rewritten each generation; unjournaled crash orphans are not automatically harvested. The safe single-writer in-handle certificate remains scoped to its opened image and must be rebuilt after recovery.

Generation overflow remains relevant: F0 uses a `u64` generation, but F1/F2 stores an extent birth generation in `u32` and deliberately stops at its upper limit. The prototype has no image migration, concurrent process writer lock, mounted filesystem, verified sector-atomic writes, or physical-device power-failure qualification. CRC32C is accidental-error detection, **not cryptographic authentication**.

## References

- Rust `std::fs::File::sync_all`: https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all
- SQLite atomic commit / write ordering: https://www.sqlite.org/atomiccommit.html
- Btrfs copy-on-write design: https://btrfs.readthedocs.io/en/stable/dev/dev-btrfs-design.html
