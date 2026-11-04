# F3 — Incremental copy-on-write B+tree-style metadata updates

**Status:** experimental single-writer image implementation. Requires the F2 version-2 image format; still not mountable or safe for valuable data.

## Implementation

The F2 namespace tree is a sorted, CRC32C-protected multi-level tree with leaves containing path keys and file records and internal nodes storing each child subtree's greatest path. F3 updates this existing on-disk format **without a format bump**.

Instead of rewriting every leaf and internal node, F3 traverses the path to each changed key, stages a replacement leaf and new ancestors, and reuses the physical addresses of all untouched siblings. Parent maximum-key separators are recomputed when necessary. A full leaf (12 entries) is split into two leaves; an oversized internal node is split similarly. Deletes remove empty leaves and internal child references; a one-child root collapses to that child. Non-root nodes may be underfilled after deletion in the original F3 milestone. The later [F2/F3 backlog batch](F2_F3_BACKLOG_CLOSEOUT.md) adds localized sibling redistribution/merge and balanced bulk-load page grouping, maintaining 6–12 entries on newly built/affected non-root pages while preserving older underfilled pages for compatibility.

Initial bulk import into an empty root still uses a compact bottom-up tree builder. A multi-file transaction computes a sorted record diff and applies paths in order. Intermediate newly staged nodes that are superseded by later changes in the same transaction are discarded *before the write barrier*, leaving only the newly reachable final tree nodes and changed file data. The high-watermark cursor may still advance for discarded reservations; F2 can recycle them after recovery-root retirement.

The public diagnostic FileSystem.last_metadata_blocks_written() reports actual metadata nodes written in the last successful transaction (zero after reopen). It enables deterministic write amplification tests. A single update in a 150-file tree should rewrite at most three metadata nodes, compared with roughly 18 nodes from a full rebuild.

## Safety guarantees inherited from F2

- All newly written pages are checksummed and carry node birth generations; unchanged pages retain their original birth generations.
- F2's full-tree verified recovery validates internal separator ordering, node addresses, file contents and snapshots before accepting a root.
- The allocator protects the union of reachable blocks across both F0 root slots and every pinned snapshot. Shared unchanged pages remain pinned by every root referencing them.
- New blocks are synced before F0 publishes the new root; interrupted writes must recover either a whole previous generation or a whole new generation. The tests deliberately fault writes during path copying and check old/new file values.
- A transaction is bound to the volume identity and original root generation; stale or cross-volume commits are rejected.

## Deliberate limits

F3 continues to materialize the full in-memory BTreeMap namespace per transaction and verifies a whole candidate tree on open. The allocator still rebuilds its reachable-set oracle across retained roots before each commit. Actual work to modify a single file is smaller, but end-to-end commit cost is **not logarithmic** until those components become incremental.

Node occupancy after deletes is not rebalanced except by removing empty children/collapsing the root. The follow-up adds checksummed **on-disk range pagination** with snapshot and generation-bound continuation; it is not yet a leaf-linked iterator, and other APIs still use the fully loaded namespace. No stable page cache, durable free-space tree, streaming file I/O, compaction, concurrent writers, snapshots beyond F2's 16 pins, ACLs, mount driver or hardware-level crash validation is implemented.

## Tests

Run cargo test -p bfs-ng-storage --test storage_f3, then cargo test --workspace --all-targets, cargo clippy --workspace --all-targets -- -D warnings, and cargo fmt --all -- --check. Integration tests inspect shared physical tree addresses independently, assert reduced metadata write counts, compare randomized updates/deletes to a BTreeMap oracle across repeated reopen, test split/deletion/root-collapse, preserve pinned snapshots during recycling, and inject failures at every path-copy/commit boundary.

**Next:** implement occupancy-aware deletion balancing and durable free-space/reference tracking, replace full in-memory namespace cloning with direct indexed traversal/cursors, then connect M0 typed searchable attributes in the same transaction root.
