# F2/F3 backlog closeout — allocation audit, balanced CoW pages, disk cursors

**Status:** experimental single-writer test-image filesystem. This batch is compatible with the F4 \`BFSNGM4\` root and F3 node format; **no disk-format change** is required. Not safe for valuable user data.

## F3: occupancy-aware deletion and balanced bulk load

The F3 metadata nodes have fanout 12. New bulk builds distribute leaves and internal children evenly, so a non-root page starts with **6–12** entries (except an unbalanced pre-existing tree opened from an older image). A modified leaf is copied. If a deletion leaves it below 6 entries, the changed child is rebalanced with a neighboring sibling through *immutable* CoW copies. If both fit in one 12-entry page, merge; otherwise redistribute entries as evenly as possible. The same procedure applies to internal child-pointer pages and the root collapses a single-child level. Unrelated subtrees retain their old physical blocks. No in-place writes are made to a retained tree or a pinned snapshot.

This is **local occupancy repair**, not a global validator demanding minimum occupancy of every historical page. Old F3 test images may contain underfilled pages that remain valid; they are healed on affected paths. A full in-memory namespace map still exists, and a multi-path transaction still computes a logical diff. We do **not** claim B+tree end-to-end \`O(log n)\` update cost.

The standalone \`storage_f2_f3_backlog.rs\` tests independently parse physical tree nodes and assert occupancy after initial bulk builds and large deletion batches, verify all survivor records, and recover from interrupted merge/redistribution writes.

## F3: direct on-disk range pages

New APIs:

\`\`\`rust
fs.scan_files_disk(expected_generation, prefix, after, limit)
fs.scan_snapshot_files_disk(snapshot_generation, prefix, after, limit)
\`\`\`

Both scan checksummed **on-disk tree pages**, not \`FileSystem\`'s materialized \`BTreeMap\`. Keys are sorted UTF-8 absolute paths; \`prefix\` matches the beginning of the key, and \`after\` is an exclusive continuation key. \`NamespacePage\` returns \`generation\`, \`entries\` (path + stable inode/type/size), \`next_after\`, and \`metadata_pages_read\`. A live cursor must supply the expected root generation on every page; stale generations are rejected. A pinned snapshot cursor reads the pinned historical tree. The F4 private catalog is omitted from all public pages.

The walker uses high-key pruning to skip whole subtrees whose maximum key precedes the prefix or continuation key. It stops on leaving the lexicographic prefix interval. Limit 1–512 per page. Prefixes may end in slash, enabling \`"/assets/"\` directory-style enumeration. This is an **on-disk range cursor** but not yet a leaf-linked iterator, and it does not eliminate full tree validation at open or F2 full-scan allocation marking.

## F2: reconstructed allocation and retention audit

\`FileSystem::allocation_report()\` independently walks both recoverable F0 root candidates and all *uniquely* pinned F2 snapshot roots. It checks F4 catalogs and all checksum-verified data/metadata before computing:

- Physical referenced high-region block count.
- Blocks shared by multiple retained roots.
- Free/reclaimable blocks absent from every retained root.
- Next-commit fresh space below the historical high-watermark.
- Remaining root-generation slots before the low-end root region collides with the high watermark.
- Root candidate and historical snapshot counts.
- \`reuse_blocked\` when any checksummed root candidate has an invalid higher-level tree, preserving the existing **fail-closed allocator** behavior.

This is a read-only integrity/accounting tool and a common oracle for F2's conservative allocator. It does **not** persist a reference-count tree, provide live online fsck, repair corrupt blocks, release unused high-region space to the host OS, or remove the root-generation limit. A malformed retained root prevents reusable-block allocation; the commit may still succeed by allocating *fresh* blocks if enough remain.

## Verification

\`\`\`sh
cargo test -p bfs-ng-storage --test storage_f2_f3_backlog
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
\`\`\`

Eight additional tests inspect tree occupancy, compare paginated disk walks to the independent namespace oracle, pin snapshots through large deletes, exercise F4 attribute/index consistency after tree merges, check fail-closed F2 space reporting, and simulate torn writes/crashes around deletion publication.

## Remaining actual backlog

F2 still needs a durable, independently verifiable allocation/reference-count tree with delayed-free epochs, write interruption journaling, crash-resumable reclamation, data relocation/compaction, a root-payload addressing redesign to avoid monotonically consuming low-end blocks, and device-level fault testing. F3 still clones the in-memory namespace per transaction, lacks leaf links and query-plan-aware streaming cursors, and cannot guarantee logarithmic **end-to-end** cost while F2 rescans all retained roots. These require separate format/design milestones. Do not mark them implemented on the strength of the above audit and cursor work.
