# F1 — checksummed copy-on-write namespace prototype

**Status: experimental test-image implementation. Not mountable; do not store irreplaceable data.**

F1 extends the F0 durable-root model with actual files, directories, stable inode numbers, immutable file-content blocks, and a checksummed multi-level sorted metadata tree. It remains a single-writer, append-only, bounded research filesystem.

## Physical image layout

| Region | Content |
| --- | --- |
| Blocks 0–1 | F0 alternating CRC32C root slots |
| Low-end, increasing | F0 root payload block for generation g is at g+1 |
| High-end, decreasing | F1 immutable metadata nodes and file-content extents |
| F0 root payload | 56-byte F1 manifest containing generation, allocator cursor, tree root, next inode and record count |

The high-end allocator cursor only decreases. No physical space is reclaimed after deletes. A staged allocation must stay strictly beyond the next reserved F0 payload address, preventing root/data overlap.

F1 tree nodes are 4096-byte checksummed blocks with magic, level, generation and at most twelve sorted children or leaf entries. Leaf keys are canonical UTF-8 absolute paths; leaf records hold the stable inode, file/directory type, file extent, length, and full-file CRC32C. Internal entries contain the highest key in each child and that child's physical block address. File-content blocks store up to 4064 bytes each plus inode, chunk index, length and checksum.

Each mutation rebuilds the **entire immutable metadata tree**, bottom-up. This is not an optimized path-copying B+ tree: metadata write amplification is proportional to the number of entries. Existing unchanged data extents can be shared across generations. That makes the prototype simpler to reason about but unsuitable for high-volume workloads.

## API and limits

The Rust FileSystem type offers format, open, begin transaction, commit transaction, read_file, list_dir, stat, generation and into_device. FsTransaction offers mkdir, write_file, remove_file and rmdir. The root directory is implicit and has inode 1.

Paths must be absolute, UTF-8, canonical, no dot/dotdot components, no duplicate separators, no NUL, no trailing separator; parents must be directories. Replacing file contents preserves its inode. Each transaction publishes a complete set of namespace changes or none.

Prototype safety budgets: **2,048 entries maximum**, **240 bytes maximum path**, **256 KiB maximum file contents**, **twelve child pointers per tree node**, and **no free-space reuse**. No rename, hardlinks, symlinks, permissions, snapshots, huge files, compression, encryption or multiwriter coordination.

## Commit durability contract

1. Validate the requested namespace edits, allocate fresh high-region blocks for updated file data, and rebuild checksummed sorted metadata nodes in unused blocks.
2. Write all new blocks, then sync them to stable storage.
3. Ask F0 to write and sync the manifest payload, and then publish and sync the alternate checksummed superblock.
4. Only after all sync barriers succeed, install the new namespace in the caller's process.

Failure at any stage poisons the writer handle. Recovery reads the latest valid F0 root candidates and fully verifies each candidate's F1 tree and file contents. If the newest tree or file content is corrupt, it can select the preceding complete generation. A failed final sync can leave a wholly valid new generation already committed; callers must reopen and inspect it.

This design assumes the host storage stack honors successful sync barriers, old durable blocks are not corrupted by new writes, and only one process writes an image. The crash simulator is a deterministic model of partial writes and flush ordering, **not a proof of actual SSD power-failure behavior**. FileDevice has no cross-process image lock. Treat generated image files as disposable test artifacts.

## Validation and benchmark boundaries

Integration tests check file-backed write/close/open, nested directories, multiblock contents, multi-level sorted metadata trees, file updates/deletes, stable inodes, invalid paths, stale transactions, exhaustion, all seven staged-commit fault points, torn-write boundaries and corruption fallback for both newest tree nodes and data extents.

Run:

    cargo test -p bfs-ng-storage
    cargo test --workspace --all-targets
    cargo clippy --workspace --all-targets -- -D warnings
    cargo fmt --all -- --check

This F1 tree is separate from the M0 typed searchable-attribute index. It has no native indexed queries yet. The AxiomZig, DVE and AxiomMegaDoc source-backed benchmark harness can now become a candidate small-file import fixture, but importing those corpora directly and query indexing are future tasks.

## Following milestones

- F2: explicit allocation birth generations, retention of more than the current two roots, orphan/free-space lifecycle, and crash-resumable reclamation.
- F3: incremental CoW B+ tree path copying, efficient range scans, larger files, bounded write amplification.
- F4: durably index typed attributes and stable asset dependencies, committing file attributes and posting changes with the same root.
- F5: independent crash/model checking, permission/ACL semantics, scrub and repair, OS integration and mounting.

See F0_STORAGE.md and M0_SEARCHABLE_ATTRIBUTES.md. F1 remains **experimental and not a production filesystem**.
