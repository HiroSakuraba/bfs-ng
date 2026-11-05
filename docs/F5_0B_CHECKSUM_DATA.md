# F5.0b — headerless data and snapshot-retained physical checksums

**Status:** Experimental one-blob proof harness in [PR #23](https://github.com/HiroSakuraba/bfs-ng/pull/23). This is NOT F5.1's extent-based file layer and does NOT replace the functioning v9 filesystem. F0 v2 root publication and F2.R4 certified v9 allocation remain unchanged.

## 1. Implemented components

- **Global physical checksum B+tree:** immutable v10 \`TreeId::Checksum\` pages, with parent-held address/birth/CRC32C block pointers, exact maximum-descendant high-key separators, ordered disjoint physical keys, a complete bounded read-time checker, and self-identifying page CRCs.
- **Checksum leaf item:** \`key=physical:u64 big-endian\`; \`value=count:u16 little-endian (=1), reserved:u16 (=0), crc32c:u32 little-endian\`. Larger contiguous checksum runs are specified by F5, but NOT encoded in this tranche. Unknown counts fail closed rather than being silently skipped.
- **CoW checksum-tree staging:** build new immutable pages from the expected exact physical checksum map, stage without publishing, verify the entire staged tree against independently reconstructed file/snapshot ownership, then use F0's ordered barriers. This first implementation **bulk rebuilds** checksum pages; it is *not* the localized CoW per-key updater intended for later milestones.
- **Restricted \`HeaderlessDataWriter\`:** a single logical blob of 0–4,096 bytes. Its physical data blocks are exactly 4,096 bytes of user bytes plus zero padding, with *no* in-band header or metadata. The fs root is a single v10 \`Fs\` leaf containing either nothing (empty blob) or the fixed key \`blob\` and a 20-byte immutable reference: \`physical:u64\`, \`birth:u64\`, \`logical_length:u16\`, \`reserved:u16=0\` (all little-endian).
- **Historical retention:** snapshot descriptors keep the old fs root. The **selected** F0 manifest's checksum tree covers the exact physical union of its live blob and ALL pinned/dropping snapshot blobs. It never removes an old checksum merely because live data was overwritten. An older F0 candidate retains its own historical checksum-tree root during fallback rotation. Marking a snapshot Dropping is **not** physical reclamation.

## 2. Read and reuse authority are distinct

\`ReadOnlyV10::open\` still validates only the direct manifest root pages. A missing or corrupt *data* block can fail on a later read; fast read-open does not validate every historical byte. By contrast, \`HeaderlessDataWriter::open\` and every mutation reconstruct **both** recoverable F0 candidates, traverse their full supported live/pinned fs and checksum graphs, compare exact checksum-key coverage, verify data CRC32C and zero tails, and reject damaged fallback metadata. Never infer a free physical block from a valid CRC or a lazy root read.

This writer admits **only** pages below the published virgin high-water boundary that are (a) absent from the independent two-root ownership map and (b) still zero when read. Nonzero unpublished crash-orphan blocks are skipped by the descending cursor and are never reused. It does not use a free-range index, delayed-free journal, snapshots for reclaim, in-place rewrite, or online compaction.

## 3. Atomic commit sequence

1. Verify current and fallback F0 ownership completely, including snapshot descriptors, checksum metadata, physical data checksums, and birth identities.
2. Plan the next immutable data page (if any), an fs root page, and a CoW checksum-tree rebuild. For pin/drop-mark only, path-copy the snapshot catalog without rewriting data checksums.
3. Compute the **exact new union** of physical blocks reached by the new live root and all new retained snapshot descriptors; retain their checksums and no others. Do NOT transfer current-manifest checksum authority to the previous fallback candidate, which already has its own root.
4. In a read-only in-memory overlay, re-traverse the complete proposed metadata/data graph. Require valid data checksums, canonical tail padding, and equality between expected and actual snapshot descriptors, with no staging address overlapping existing owners.
5. Write immutable data and metadata pages; **sync**; write next F0 rotating root payload; **sync**; publish alternating F0 superblock; **sync**. Install in-handle state only after complete success; an ambiguous failure poisons the handle until reopen.

Because each commit allocates only virgin blocks and retains no reclaimable pool, this prototype can reach NoSpace despite orphaned or superseded pages. No F5.3 accounting proof or disk-format migration is implied.

## 4. Required evidence

\`cargo test -p bfs-ng-storage --test storage_f5_0b_headerless\` exercises snapshot pin → overwrite → repeated F0 rotation → historical read; unpinned overwrite/fallback checksum retention; empty and exactly 4 KiB blob; nonzero crash-orphan quarantine; corruption in live data and fallback-only checksum metadata; checksum-tree branch splits; staged-write/payload/root crashes; and an ENOSPC case that must leave the previous generation intact.

**Explicit limitations:** exactly one logical 4 KiB blob, a non-general fs leaf, one-block checksum run items, a full checksum-tree rebuild, no partial file writes, no multi-block files, no F5.1 extents or inode table, no incremental metadata reserve, no clone, no real-device power-cut proof, and no physical snapshot deletion. These are mandatory follow-up milestones rather than features inferred from the prototype.

See also [F5 data-layer design](F5_DATA_LAYER.md), [F5.0a format](F5_0A_WIRE_FORMAT.md), [F5.0a virgin writer](F5_0A_VIRGIN_WRITER.md), and [architecture/backlog](ARCHITECTURE.md).
