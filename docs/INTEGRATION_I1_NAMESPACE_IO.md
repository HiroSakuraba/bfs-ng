# Integration I1 — stable namespace identities and practical v9 file operations

**Implemented in experimental storage:** The existing v9 `FileSystem` now supports atomic file/directory-subtree renames, byte-range I/O and truncation. The v9 maximum logical file size increases from 256 KiB to **4 MiB**. These are compatibility and usability improvements to the *working image-backed engine*, not a v10 migration or a full streaming implementation.

## Rename semantics

`FsTransaction::rename(source, destination)` retains file and directory inode numbers, existing immutable data extents and inode-keyed F4 attribute rows/postings. A moved directory atomically rekeys every descendant before `FileSystem::commit` publishes the new namespace root. Path syntax, all renamed descendant lengths, private F4 catalog protection, destination collisions, missing destination parents and a move into the source's own subtree are checked **before** changing the transaction. Pending uncommitted `new_data` keys move together with their records.

Snapshots retain their earlier root and namespace spelling: a pinned `/assets/shader` remains accessible there even if the live file subsequently becomes `/project/shader`. A pure rename does **not** change or rebuild F4 attribute postings because their identities are stable inode IDs; those rows already share the F0 transaction boundary with the namespace.

## Transitional partial I/O

`FileSystem::read_at(path, offset, len)` checks an existing file and returns a bounded byte range. `FileSystem::write_at(path, offset, bytes)` and `truncate(path, size)` use the already audited v9 CoW transaction path. Writes beyond EOF zero-fill the gap; overflow, invalid parents, directory writes and the 4 MiB limit fail without publishing. Empty writes and unchanged-size truncations are no-ops.

**Important performance limitation:** v9 still reads and **rewrites an entire changed file** even for a tiny `write_at`. Its single file extent, full-reference reconstruction and snapshot retention mean it is not a production partial-write or streaming solution. No new v9 physical reuse algorithm is introduced; F2.R4's certified, delayed-safe allocation remains in effect.

## Integration contract to v10

The next stage should move the **same identity and namespace semantics** to v10's `TreeId::Fs` records, including a persistent `(parent_inode, name) -> inode` index and inode-keyed file extent roots. A v10 commit must publish namespace, checksum tree, complete snapshot historical roots, and indexed attribute rows/postings/schema together through F0, not as two independently committed images.

A separate full-graph checker must certify both candidate F0 roots before any retired block is physically reused. The current v10 `ExtentDataWriter` is **one logical file bounded to 256 KiB**; do not claim it already supports v9's multiple names/attributes, and do not silently mount v9 as v10. The working v9 engine remains the reliable experimental fallback while this shared v10 tree format is built.

## Acceptance tests

`cargo test -p bfs-ng-storage --test storage_unification_v9` checks:

- Recursive directory relocation, stable inode identities, persistent F4 typed attributes/postings and snapshot historical paths after reopen.
- Rename of a file with an uncommitted payload, rename-into-descendant rejection, unavailable parents, collision, private catalog and overlong-descendant rejection.
- Partial overwrite across data blocks in a 512 KiB file, snapshot isolation, truncated/extended zero tails, holes on new files, no-op and overflow semantics.

**Remaining high-priority work:** v10 durable multi-inode namespace and F4-compatible attribute index, true multi-megabyte streaming/coalesced extents with bounded memory, efficient partial CoW rather than full-file staging, end-to-end benchmarks, independent salvage/fsck, and F5.2/F5.3 safe reusable-space proofs.
