# F0 — experimental block image and root publication

**Status: committed implementation prototype. Not a usable filesystem, not safe for irreplaceable data.**

This milestone adds dependency-free \`bfs-ng-storage\` with a fixed 4096-byte block interface, a preallocated **regular-file** device, CRC32C-protected alternating root slots, an append-only single-root-block payload format, and an explicit deterministic crash simulator.

## Version 1 image layout

| Block | Function |
| --- | --- |
| 0 | root-slot A; generations 0, 2, 4, ... |
| 1 | root-slot B; generations 1, 3, 5, ... |
| 2 | immutable root payload for generation 1 |
| 3 | immutable root payload for generation 2 |
| g+1 | immutable root payload for generation g, g >= 1 |

There is **no free-list, inode tree, directory tree, data extent map, snapshots, reusable allocation or production metadata parser**. Each successful commit appends one root payload block; an image of N blocks can commit at most N-2 generations. Root payloads may contain 0..4080 bytes of opaque data. All updates should use *disposable test images only*.

Root slot (4096-byte block) v1 fields, **little-endian**:

| Offset | Size | Meaning |
| ---: | ---: | --- |
| 0 | 8 | \`BFSNGF0\\0\` magic |
| 8 | 4 | format version: 1 |
| 12 | 4 | logical block size: 4096 |
| 16 | 16 | explicit nonzero volume ID |
| 32 | 8 | committed root generation |
| 40 | 8 | root payload block (all-ones means absent at generation 0) |
| 48 | 4 | CRC32C over complete 4096-byte root payload; 0 if absent |
| 52 | 4 | reserved zero |
| 56 | 8 | image block count |
| 64 | 56 | reserved zero |
| 120 | 4 | CRC32C of header bytes 0..119 |
| 124 | 3972 | reserved zero |

Root payload blocks have an 8-byte \`BFSNGR0\\0\` magic, 4-byte length, 4 reserved zero bytes, at most 4080 payload bytes, and zero-filled tail. A slot is accepted only if it passes format, header checksum, generation/slot parity, volume identity, image geometry, root-block bounds, root-payload CRC, magic, length and zero-padding validation.

**CRC32C is error detection, not authentication**. Disk format v1 is a deliberately narrow F0 prototype and is subject to a breaking version bump before introducing a real CoW tree. A generation counter is distinct from a block allocation birth generation (C5); the later allocator needs explicit retention and cannot infer safety from these simple roots.

## Single-writer commit protocol

For every new root generation \`g\`:

1. Create a completely new, immutable payload in block \`g+1\`. Do not modify a block referenced by the currently published root.
2. **Sync the payload file** using \`File::sync_all()\`. If this fails, poison the handle and require reopening.
3. Write the alternate superblock slot \`g % 2\`, including the payload pointer, payload CRC, generation and header CRC32C.
4. **Sync the published slot** using \`File::sync_all()\`. Only after success is \`commit_root\` reported successful.

Recovery reads both candidate slots and validates the referenced payload before selecting the highest generation. If the newest slot is torn or its payload is corrupted, recovery can fall back to the previously committed slot. If both roots are unusable, it fails closed. A failed final sync can leave **either** previous or new root visible after crash; a caller must query recovery and not infer commit success merely from the error.

The root update sequence follows the classical write-new-data-before-new-root principle (copy-on-write), but **is not a universal guarantee across arbitrary hardware**: it requires the underlying storage stack to honor sync barriers and not corrupt unrelated previously synced blocks on power loss. A real filesystem must test actual OS/device behavior, flush ordering, torn sectors, write caches, directory entry durability, controller faults and host filesystem failure modes.

## Fault injection

\`CrashDevice\` maintains two images:
- \`pending\`: visible to a running process, not yet guaranteed durable.
- \`stable\`: changes made durable by \`sync\`, plus explicitly injected partial writes.

A fault may fire after any of the four commit operations, and a write may partially reach stable storage (0..4096 leading bytes) even when the final sync did not occur. After \`power_loss()\`, pending writes vanish and recovery reads stable data. Tests enumerate all four failure points and a set of torn-write boundaries; assert that recovery produces an entire old or entire new root, never a partially accepted payload.

This is a **model-level crash test**; it does not prove atomic sectors, physical SSD behavior or host filesystem durability. File-based integration tests additionally close and reopen real image files, validate checksums and deliberately corrupt the newest payload.

## Non-goals and safety constraints

- **Single writer only.** The device does not acquire an OS file lock; opening the same image for concurrent writers is unsupported and dangerous.
- **Regular files only.** Never point the prototype at raw devices, mounted partitions or real user data. \`create_new\` refuses to overwrite an existing image.
- **No grow/shrink, truncation tolerance or repacking.** All image blocks must be present and geometry must be fixed.
- **Volume creation durability:** \`FileDevice::create_new\` syncs the image contents and metadata, but does **not** fsync the parent directory. A new pathname may be lost following a system crash; use an external image-creation discipline in future mount code.
- **No verification of malicious content:** the checksum protects accidental corruption, not adversarial rewrites.
- **No encryption, ACLs, multi-writer coordination or snapshot pinning.**
- **No claims of strong isolation under hostile path races** (symlink check is best effort).

## Next integration tranches

F1 will connect a checksummed CoW node tree, then metadata attributes and index postings, using stable file IDs and explicit allocation birth generations. F2 must specify allocation commit rules, snapshot roots, orphan reclamation and crash-resumable consistency. The F0 root payload API is a limited test harness, not final filesystem storage.

### References

- Rust \`std::fs::File::sync_all\`: https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all
- SQLite atomic commit and torn writes: https://www.sqlite.org/atomiccommit.html
- Btrfs CoW design: https://btrfs.readthedocs.io/en/stable/dev/dev-btrfs-design.html

## Verify

\`\`\`sh
cargo test -p bfs-ng-storage
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
\`\`\`
