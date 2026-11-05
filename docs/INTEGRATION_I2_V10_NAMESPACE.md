# Integration I2 — coherent v10 namespace + extent/checksum publication

**Status:** Experimental, bounded **multi-file** v10 filesystem in [PR #26](https://github.com/HiroSakuraba/bfs-ng/pull/26). It is the first BFS-NG v10 image that stores multiple named files, nested directories, stable inode IDs, sparse per-file extents, historical snapshots and physical checksum metadata **in the same F0 transaction**. This does not switch the main \`FileSystem\` (v9) to v10, does not include searchable F4 attributes yet, and is not safe as an OS-mounted production filesystem.

## 1. Wire layout and namespace identity

The manifest's \`TreeRoots.fs\` now points to a namespace B+tree of sorted **absolute UTF-8 pathname keys**. Its 40-byte value is:

| Offset | Width | Description |
|---:|---:|---|
| 0 | 8 | Stable nonzero file/directory inode (\`u64\` LE), at least 2 |
| 8 | 1 | Kind: 1 directory, 2 regular file |
| 9 | 7 | Reserved zero |
| 16 | 24 | Optional authenticated \`BlockPointer\` to an immutable Fs extent **leaf** |

The root directory \`/\` is implicit with inode 1. Every other path has a live parent that is a directory. Inodes are unique in this first profile (hard links are not yet supported) and strictly lower than \`Manifest.next_inode\`. Namespace pages use standard v10 \`Page\` checksums and parent-child high-key B+tree separators; a complete validator checks sorted keys, duplicate page addresses, parent validity, path limits and stable inode uniqueness. Up to 2,048 namespace records are admitted by the prototype.

A regular-file entry's optional \`file_root\` points to the existing F5.1 \`FileExtents\` page; \`None\` is a zero-byte file. This keeps logical file metadata distinct from the directory index while sharing **one** v10 on-disk image and transaction boundary. Each file currently supports up to 64 logical blocks (256 KiB), sparse holes, physical CoW rewrite of touched blocks, and logical truncate.

## 2. Atomic, independent ownership certification

\`UnifiedV10\` supports \`format/open\`, \`mkdir\`, \`list_dir\`, \`stat\`, \`write_at\`, \`read_file\`, \`read_at\`, \`truncate\`, \`rename\` (including descendant paths), \`remove_file\`, snapshot pins and dropping-state transitions.

For every write, it independently audits **both** recoverable F0 manifests. For each it traverses:

1. The current and pinned snapshot **namespace trees**, including historical filenames and inode identities;
2. All referenced per-file extent-leaf roots, including those shared across snapshots;
3. The union of all immutable live/pinned physical data blocks, including stable births and partial EOF zero tails;
4. The physical checksum B+tree, requiring *exact* coverage of that block union;
5. The snapshot catalog and all metadata page identities.

Any invalid fallback, malformed branch separator, conflicting physical incarnation, orphaned checksum, missing data, or overlap of data/metadata addresses fails closed. Pages are allocated only from independently verified all-zero *virgin* physical addresses below the monotonic high-water boundary. Nonzero crash orphans are **never reused**, and actual reclamation is still disabled. This is stronger than trusting a lazy v10 reader or a CRC alone but remains expensive: full-graph audits are still performed.

The writer then stages changed data blocks, a new file extent leaf, a new namespace B+tree root, a page-local checksum B+tree delta, or a CoW snapshot catalog (depending on operation). It re-verifies the entire proposed replacement graph in an in-memory **read-only overlay** before F0's write/sync/alternate-superblock publication sequence. The old snapshot roots remain immutable. A renamed directory retains all descendant inode IDs, file extents, and metadata identities.

## 3. What is integrated, and what is not

Integrated: **directories + file identity + per-file extents + checksum tree + retained snapshots + atomic F0 publication.**

Not yet integrated: **F4 typed searchable attribute rows/postings/schema**. The v9 F4 engine still supports these. The next integration tranche must move the existing inode-keyed F4 index into the v10 root with complete snapshot historical attribute roots, then avoid rewriting the full catalog for a single metadata update.

Practical storage limitations: each file remains **256 KiB**, namespace CoW updates still rebuild the entire namespace B+tree, no arbitrary-size streaming/multilevel per-file extent tree, no hard links/symlinks/rename replacement, no reusable allocation, no online fsck, no mount and no hardware power-cut certification. The default v9 engine now allows 4 MiB whole-extent rewrites and reliable F4 attribute indexes; it is not silently replaced.

## 4. Tests and follow-up gates

\`cargo test -p bfs-ng-storage --test storage_v10_unified_namespace\` covers:

- Multiple named files with separate extents/checksums in one v10 image;
- Recursive namespace rename, stable inode numbers, pinned historical paths/bytes across F0 rotation and reopen;
- A deterministic multi-file byte oracle for unaligned sparse writes and truncation;
- B+tree branch splitting for hundreds of names and rejection of corrupted pages;
- Invalid rename/move operations without publication, physical data corruption checks;
- F0 publication crash injection and out-of-space atomicity;
- Side-by-side v9/v10 namespace and inode identity semantics for the same commands.

**Next:** F4 index integration with historical schema BPs, multi-level extent trees and bounded streaming, metadata-update path-copy rather than full namespace rebuild, exact F5.2/F5.3 reusable-space proof against an independent reference model, and a real standalone archive tool.

References: [F5 data-layer design](F5_DATA_LAYER.md), [F5.1 extents](F5_1_EXTENTS.md), [v9 rename bridge](INTEGRATION_I1_NAMESPACE_IO.md), [architecture](ARCHITECTURE.md).
