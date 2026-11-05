# F5 — extent data layer, birth-time reclamation and inode namespace (proposed)

**Status:** design proposal, **safety-corrected 2026-11-04**. F5.0a's checked v10 codecs, historical snapshot-root reader, staged CoW catalog and restricted virgin-only metadata publisher are implemented experimentally. F5.0b added the [one-blob checksum/data proof](F5_0B_CHECKSUM_DATA.md). The [bounded F5.1 extent experiment](F5_1_EXTENTS.md) now adds one sparse logical file (max 64 × 4 KiB blocks), byte-addressed CoW writes, truncation and localized checksum-leaf/ancestor updates. The experimental [I2 integrated v10 namespace](INTEGRATION_I2_V10_NAMESPACE.md) now unifies directories, stable inodes, separate per-file extent roots, snapshots and the global data checksum index in one F0 root. **The F4 typed searchable attribute index, normalized parent-inode directory keys, general multi-level per-file extents, inline data, `punch_hole`, `fallocate`, large streaming, reusable allocation, safe snapshot retirement and production mount remain unimplemented.** Target format: F0 superblock **v2** (unchanged) and experimental root manifest **v10**. Disposable single-writer test images only; no v9 migration. Code and tests are authoritative.

**Baseline:** `main` after F0.R1 (manifest v9), as described in [ARCHITECTURE.md](ARCHITECTURE.md).

Every design choice below is followed by a **Why** paragraph. The [decision log](#14-decision-log) collects them in one table with the rejected alternatives.

---

## 1. Summary

v9 has a careful commit path (one publication point, two-root reuse safety, independent oracles) sitting on top of a file-data layer that cannot grow into a real filesystem. Files are write-once values of at most 256 KiB, stored as one contiguous run of blocks, with a 32-byte header inside every 4 KiB data block. The search catalog rides on the same path.

F5 replaces that layer with:

1. **Headerless, page-aligned data blocks** whose integrity is vouched for by their parent (a checksum tree plus 64-bit birth generations in pointers).
2. **A per-inode extent map** with inline small files, holes, preallocation and partial overwrites.
3. **An extent-based durable free-space tree** carrying the existing ready / pending / deferred epochs.
4. **Birth-time reclamation with per-snapshot dead lists**, which removes the per-commit `count_roots` walk.
5. **An inode table plus `(parent, name)` directory entries**, replacing full-path keys.
6. **The F4 attribute catalog in its own CoW trees**, off the file-data path.
7. **Optional shared extents (clone/reflink)**, off by default and gated by an oracle.

The F0 publication protocol, the two-root reuse invariant and the "oracle referees the fast path" methodology are kept unchanged. **Safety corrections:** snapshot descriptors retain attribute/schema roots; the checksum tree retains historical checksums while snapshots survive; snapshot-owned and volume-global metadata use different retirement rules; free-space eligibility is encoded as a stable absolute generation; fast opening never authorizes unverified reuse; and tree-allocation reservations must be bounded before publication. F5.3's birth/dead-list algorithm remains a hypothesis requiring an executable proof model, **not an approved allocator**.

## 2. What is wrong with the v9 data layer

These come from `crates/storage/src/fs.rs` at the baseline.

| # | Problem | Where | Real-world consequence |
| ---: | --- | --- | --- |
| 1 | `write_file` replaces the whole file; no offset write, append, truncate or hole punch. The whole file is held in a `Vec` in the transaction. | `FsTransaction::write_file`, `new_data` | No POSIX file semantics are possible on top. Memory use scales with file size. |
| 2 | `MAX_DATA = 256 KiB`. | `fs.rs` constants | Ordinary files (assets, binaries, documents) do not fit. |
| 3 | Each file is exactly one contiguous physical run. `Builder::reserve` refuses to stitch holes. | `Builder::reserve` | Once free space fragments, writes fail with `NoSpace` while plenty of space is free. |
| 4 | `reserve` walks the entire reusable set linearly per allocation, and the set holds one `u64` per free block. | `Builder::reserve`, `BTreeSet<u64>` | O(free blocks) per allocation. A 1 TiB image has about 268 million 4 KiB blocks. |
| 5 | Every data block carries a 32-byte in-band header (magic, inode, index, length, CRC, generation), leaving 4,064 payload bytes. | `encode_data`, `CHUNK` | File offsets never align with pages. Rules out mmap, O_DIRECT, zero-copy reads, compression framing and device atomic-write alignment. |
| 6 | The inode number is stamped into each data block. | `encode_data` | Two files can never share a block, so clone/reflink and dedupe are impossible by format. |
| 7 | Data blocks store the birth generation as `u32`, and `commit` refuses generations above `u32::MAX`. | `encode_data`, `commit` | About 4.29 billion commits for the life of a volume. At 1,000 commits per second that is roughly 50 days. |
| 8 | The record keeps a whole-file CRC on top of the per-block CRC. | `Record::checksum`, `data_read` | A partial read cannot be verified without reading the whole file. |
| 9 | The F4 catalog is a single file, `/.bfs_f4_attrs`, bounded to 256 KiB and rewritten on every change. | `attributes.rs` `MAX_BYTES`, `commit` | Search capacity is capped by the blob limit; every attribute edit rewrites the index. |
| 10 | Namespace records are keyed by full path, and `begin()` clones the whole record map. | `State::records`, `begin` | Directory rename would rewrite every descendant; hard links are impossible; 240-byte keys force `FANOUT = 12`; every transaction is O(files). |
| 11 | Open verifies every candidate's tree and file contents. | F1 recovery contract, `data_read` | Mount time grows with total data size. |

## 3. Goals and non-goals

**Goals**

- Byte-addressable files of any size up to `u64`, with sparse holes, preallocation and partial overwrite.
- Allocation that succeeds whenever enough free space exists, regardless of fragmentation, minus an explicit metadata reserve.
- Commit cost proportional to the change set times tree height, not to the number of files or snapshots.
- Mount cost proportional to tree height, not to data size.
- Keep every v9 safety property: single publication point, two-root reuse rule, fail-closed on unknown ownership, independent full-scan oracles.

**Non-goals for F5**

- Compression, encryption, content-hash dedupe, multi-device redundancy, concurrent writers, back-references, and the mount adapters themselves. Fields are reserved where a later milestone would need them, and those fields must be zero in v10.

## 4. Principles

Carried over unchanged from [ARCHITECTURE.md §1](ARCHITECTURE.md): recoverability before reuse, one publication point, stable typed identity, independent verification, portable core, measured performance claims, explicit formats.

One new principle:

> **The parent vouches for the child.** A block's validity is established by the pointer that reached it (address, birth generation, checksum), not only by what the block says about itself.

**Why:** a self-describing block can be internally valid but stale (a lost write) or valid but in the wrong place (a misdirected write). A checksum held by the parent catches both. This is the ZFS block-pointer model, and it lets data blocks drop their header entirely. Metadata pages keep a small self-describing header as well (see §5.2) because orphan reconciliation and offline repair need to recognize a page with no parent.

## 5. Milestone breakdown

| Milestone | Delivers | Depends on |
| --- | --- | --- |
| **F5.0a** | v10 manifest and complete snapshot descriptors, `u64` generations, generic CoW metadata-page codec, typed block pointers and compatibility rejection | — |
| **F5.0b** | Headerless data, physical checksum tree with historical-retention rules, read-time verification and safe lazy open | F5.0a |
| **F5.1** | Extents, inline small files, partial writes and bounded streaming/spill | F5.0b |
| **F5.2** | CoW free-space tree with **immutable eligibility epochs**, proven metadata-page reservation and verified full-graph reuse mode | F5.0a; existing F2 ownership oracle |
| **F5.3** | Birth/dead-list accounting, snapshot-ownership-domain model and optional accounted reuse **only after proof gates** | F5.1, F5.2 |
| **F5.4** | Inode table, `(parent, name)` directory entries, hard links and rename | F5.0a |
| **F5.5** | Incremental attribute rows/postings with historical schema roots | F5.0a, F5.4 |
| **F5.6** | Optional shared extents; no default enablement | F5.3 plus interval-sharing oracle |

**Release ordering:** F5.0a and F5.0b may be tested as separate changes behind a feature/test-only format gate. Do not advertise a stable v10 image writer until its committed root, snapshot descriptor, checksums and safe default allocator are coherent. F5.0 through F5.4 are still **one public format break**; no series of incompatible v10/v11/v12 test images is intended.

**Why:** isolating the generation/pointer format from checksum/recovery semantics makes failures attributable. The new format cannot make the allocation fast path less conservative merely because mount verification becomes lazy.

## 6. F5.0 — format v10 foundation

### 6.1 Generations are `u64` everywhere

Every stored generation (birth generations in pointers and extents, snapshot generations, journal eligibility, dead-list keys) is `u64`. The `generation > u32::MAX` check in `commit` is removed.

**Why:** F0 already uses `u64`. The `u32` limit exists only because the data block header had 4 spare bytes. A volume-lifetime commit ceiling reachable in weeks is a format bug, not a tunable. Eight bytes per pointer is negligible.

### 6.2 Generic CoW B+tree page

All F5 trees (fs tree, checksum tree, free-space tree, dead-list tree, attribute trees) share one page format. Pages are 4,096 bytes. The first F5.0a implementation adds tree IDs **7 = snapshot catalog** and **8 = attribute schema**, alongside IDs 1–6, and supports canonical empty leaf roots. The codec and its exact offsets are described in [F5.0a](F5_0A_WIRE_FORMAT.md).

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | Magic `BFSNGT10` |
| 8 | 1 | Tree id (1 = fs, 2 = checksum, 3 = free, 4 = dead list, 5 = attr rows, 6 = attr postings) |
| 9 | 1 | Level (0 = leaf) |
| 10 | 2 | Item count |
| 12 | 4 | CRC32C of the page with this field zeroed |
| 16 | 8 | Birth generation of this page |
| 24 | 8 | Own physical address |
| 32 | 16 | Volume id |
| 48 | … | Slotted item directory, then keys and values packed from the end |

Items are variable-length. A slotted directory entry is `(key_off u16, key_len u16, val_off u16, val_len u16)`. Fanout is set by bytes, not by a constant.

Branch values are block pointers (§6.3).

**Why a shared format:** one encoder, one decoder, one path-copy routine, one fuzz target. The v9 code has separate page formats for namespace nodes, reference leaves, reference branches and journal pages, each with its own checks.

**Why keep a header on metadata pages:** own address plus volume id makes a misdirected or foreign page detectable even before the parent's checksum is consulted, and lets an orphan scan (backlog P0) recognize F5 pages. Metadata is a small fraction of space, so the 48 bytes are cheap there.

**Why 4 KiB pages:** smaller pages keep CoW amplification low for small updates and align with common logical sector/page conventions; **4 KiB physical write atomicity is not assumed or guaranteed**. A `node_size` field is reserved in the manifest; larger nodes are a measured follow-up, not a default.

**Why variable-length items:** with short binary keys (§7.1) a branch holds roughly 80 children instead of 12. That makes trees about three levels shallower at 100,000 entries and cuts path-copy writes accordingly.

### 6.3 Block pointer

A block pointer (BP) is 24 bytes:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | Physical block address |
| 8 | 8 | Birth generation |
| 16 | 4 | CRC32C of the target page |
| 20 | 1 | Target level |
| 21 | 3 | Reserved, zero |

Every branch entry and every tree root in the manifest is a BP. A reader rejects the target page unless address, birth generation, level and CRC all match the BP, and the page's own header agrees.

**Why:** this is what makes "the parent vouches for the child" true for metadata. It replaces the v9 pattern of trusting a page's own CRC plus a separately checked generation.

### 6.4 Headerless data blocks and the checksum tree

Data blocks contain only file bytes, with deterministic zero-filled tails beyond the logical end of an allocated final block. Each referenced aligned data block has a CRC32C in the **CoW checksum tree** keyed by its physical address:

```text
key   = csum_run_start: u64 (big-endian)
value = count: u16, reserved: u16, crc32c[count]: u32...
```

A contiguous checksum item holds a bounded run; branch/root BPs authenticate its metadata path. The checksum tree is a **volume-global manifest tree**, not a tree root stored in each snapshot descriptor.

**Historical-data obligation:** the checksum tree at generation `g` must contain exactly the checksums required to verify all `REG` data blocks reachable through the current live root **and every pinned snapshot descriptor at `g`**. A checksum for a snapshot-retained block is never removed when the live file is overwritten. When a snapshot is dropped, checksum deletion must wait until neither the newly committed manifest nor its F0 fallback candidate can require the corresponding data block. The previous F0 manifest already retains its own checksum-tree root during fallback rotation. No snapshot may rely on checksums that were removed from its selected manifest.

**Birth/reuse obligation:** a checksum is keyed by physical address, but it is also *bound to the matching data block's immutable allocation birth*. Before admitting a reused address, every old reference to that address, including snapshots and fallback candidates, must have retired. The new checksum and newly written data must be committed together; stale checksum entries may never verify a different block incarnation by accident. Incorporate birth in the extent reference and checked ownership model even if the checksum value stores only CRC32C.

**Failure path:** if a data write is acknowledged but lost, the parent-authenticated checksum tree must reject the stale bytes on read. A misdirected write must be detected on **every affected, subsequently checked read**, not assumed to be detected immediately at open; a CRC collision is still possible. Before an affected block can be freed/reused, accounting must identify the correct referenced incarnation. Torn, missing or corrupt checksum-tree metadata disables the claimed read or reuse authority, never authorizes a free block.

**Why headerless:** permits exact 4 KiB data addressing without v9's 32-byte in-band header and is compatible with later paging and shared extents. It does **not by itself** implement mmap, O_DIRECT or hardware atomic writes. CRC32C detects typical accidental corruption but does not provide cryptographic authentication or a collision-free stale-write identity.

**Why a global physical checksum tree:** fixed-sized extent records, independently checked partial reads, physically ordered scrub and eventual clone support. A snapshot's data checksum remains available only while the owning data remains retained, so checksum retirement and data retirement are one accounting obligation, not independent cleanup jobs.

**F5.0b status:** [PR #23's restricted writer](F5_0B_CHECKSUM_DATA.md) tests one 4 KiB logical blob, full checksums and historical snapshot roots. The following is the **broader target gate**, not yet discharged for multi-block files or general extents.

**F5.0b gate:** validate snapshot pin → overwrite → F0 root rotations → historical read; drop snapshot → crash at each publication barrier → check both manifest generations; reuse address only after actual ownership retirement. An independent full-graph scanner must compare the exact physical set of `REG` blocks with checksum coverage, including historical roots.

### 6.5 Root manifest v10 and complete snapshot descriptors

The F0 superblock v2 and its three-block root-payload ring remain unchanged. A new v10 manifest includes:

| Field | Width | Notes |
| --- | ---: | --- |
| Magic | 8 | `BFSNGM10` |
| Format | 4 | 10 |
| Node size | 4 | Must be 4096 in v10 |
| Generation | 8 | Current F0 generation |
| High-water mark | 8 | Conservative virgin-region boundary (§8.3) |
| Next inode | 8 | Never reused |
| fs tree root | 24 | BP |
| Checksum tree root | 24 | BP; global to this manifest and all its pinned snapshots |
| Free tree root | 24 | BP |
| Dead-list tree root | 24 | BP |
| Snapshot catalog root | 24 | BP; **values are complete snapshot descriptors**, not just fs root BPs |
| Attr row tree root | 24 | BP, zero if no schema |
| Attr posting tree root | 24 | BP, zero if no schema |
| Attribute schema root | 24 | BP, zero if no schema; immutable/versioned schema state |
| Sweep cursor | 16 | Persistent bounded cursor (§9.5) |
| Metadata reserve | 8 | Blocks; policy §8.5 |
| Counters | 32 | Checked inodes, used, free and pending counters |
| CRC32C | 4 | Root payload is also protected by F0 |

The snapshot catalog maps each `snapshot_generation` to the descriptor below. A zero BP means **absent**, not "inherit current live tree." The descriptor is immutable while pinned, except for separately journaled retirement state:

```rust
struct SnapshotDescriptorV10 {
    generation: u64,
    fs_root: BlockPointer,              // includes inode table + extents
    attribute_rows_root: BlockPointer,
    attribute_postings_root: BlockPointer,
    attribute_schema_root: BlockPointer,
    flags: u32,                         // live, dropping, reserved; defined by codec
}
```

The isolated [F5.0a wire codec](F5_0A_WIRE_FORMAT.md) now specifies the snapshot descriptor as exactly **112 bytes**: generation[0..8], fs BP[8..32], attribute rows BP[32..56], postings BP[56..80], schema BP[80..104], state u32[104..108] (`Pinned=1`, `Dropping=2`), reserved zero[108..112]. It implements encode/decode, **authenticated multilevel snapshot traversal**, and an independent **restricted, zero-verified snapshot-only writer**; it does not replace the working v9 filesystem, write v10 user files, or reclaim snapshots. Absent attribute roots must be all zero and never fall back to current live roots.

**Snapshot pin invariant:** capture the currently committed namespace root, attribute row/posting roots and schema root as one logical snapshot at the pin transaction's F0 publication. All later historical attribute lookups use the descriptor's roots, never the live manifest's attribute roots. The descriptor's filesystem root and retained schema must agree on inode identities and attribute normalization. Any validation failure is a historical-snapshot read error or reuse quarantine, not a reason to guess current rows.

**Checksum invariant:** snapshot descriptors intentionally **do not** carry their own checksum-tree root. The selected F0 manifest's global checksum tree covers the union of data blocks in every pinned descriptor and the live root; old F0 candidates still retain their own complete checksum roots. This is a distinct obligation tested in F5.0b. It avoids pinning every generation of a large global checksum tree as a separate snapshot.

**Why:** F4 currently obtains historical attributes through the private catalog inside its historical filesystem root. Separating attribute trees without preserving their historical BPs would silently lose snapshot query correctness after root rotation; [the PR #18 review](https://github.com/HiroSakuraba/bfs-ng/pull/18#discussion_r4475000939) identified this failure mode.

### 6.6 Lazy read verification versus conservative reuse certification

**Read/open phase:** parse both F0 superblock candidates, their complete v10 manifests and directly referenced tree-root pages. Verify checksums, identities, root generation and bounds. Do not traverse every data block at open. Later reads verify every traversed metadata BP and data block's immutable checksum/birth contract; deep corruption returns an error on the affected operation rather than silently rolling back unrelated changes.

**Reuse-authority phase (separate):** successful lazy opening **does not certify a single free block**. Before any address from the CoW free tree can be reused in the default `Full` mode, perform independent complete ownership verification over both F0 candidates, all snapshot descriptors, all reachable namespace/data/attribute pages, and global metadata trees. A candidate whose deeper contents or ownership cannot be verified prevents *reuse*, even if ordinary unaffected reads are allowed.

When ownership certification is unavailable, only a conservatively demonstrated virgin range beyond any recoverable candidate's published high-water mark may be allocated. The virgin boundary must be checked against both F0 roots and staged/failed writes; an unexplained generation/geometry mismatch blocks even this path. Never infer a writable address from an unchecked free-tree entry.

**Trust upgrade:** `Accounted` mode is a future F5.3 option, not activated by F5.0a, F5.0b or F5.2. It requires the canonical ownership-domain invariant, interval-aware accounting, adversarial model tests, and periodic scrub checks in §9.6. No automatic transition from lazy `open` to `Accounted` occurs.

**Why:** fast mount and safe allocation are independent questions. v9's full candidate verification is intentionally retained as a reuse oracle while F5 adds lazy data reads. This changes the user-visible recovery promise without relaxing overwrite safety.

## 7. F5.1 — extent map, inline files and streaming writes

**Partial implementation:** [F5.1 bounded prototype](F5_1_EXTENTS.md) provides one single-leaf sparse file with 64 logical blocks, checked physical extent records, partial writes, truncation, historical snapshot references, and CoW checksum path-copy. This is **not** a completed inode-table or general B+tree implementation; the full design below remains the target.

### 7.1 fs tree key space

One fs tree per snapshot root holds inodes, directory entries and extents. Keys are memcmp-ordered bytes:

```text
key = object_id: u64 BE | item_type: u8 | suffix
```

| Item type | Value | Suffix | Used for |
| --- | ---: | --- | --- |
| INODE | `0x01` | none | Inode record (§10) |
| DIRENT | `0x20` | name bytes | Directory entry under parent inode (§10) |
| EXTENT | `0x40` | file offset: u64 BE | File data mapping |

All items for one inode sort together: inode record, then its directory entries or its extents.

**Why one tree:** a snapshot is then exactly one BP, and an inode's metadata and extent map share leaves, so a small-file write usually touches one leaf. This is the btrfs layout.

### 7.2 Extent item

```text
key   = (inode, EXTENT, file_offset)    file_offset is 4 KiB-aligned for REG/PREALLOC
value = kind: u8, flags: u8, reserved: u16,
        blocks: u32,                     length in 4 KiB blocks, 1..=MAX_EXTENT_BLOCKS
        disk_start: u64,                 physical address
        birth: u64                       generation that wrote these blocks
```

| Kind | Meaning |
| --- | --- |
| `REG` (1) | Ordinary data at `disk_start .. disk_start + blocks` |
| `PREALLOC` (2) | Allocated but reads as zeros (fallocate) |
| `INLINE` (3) | Data bytes stored in the value itself; only at file offset 0 |

A byte range with no covering extent is a hole and reads as zeros. File size comes from the inode, not the extents.

`MAX_EXTENT_BLOCKS = 8192` (32 MiB). Flags bit 0 is `MAY_BE_SHARED` (§12); all other bits are zero in v10.

**Why no "offset into a larger disk extent" field (unlike btrfs):** F5 extents are block-granular and uncompressed, so any aligned sub-range of a physical run is itself a valid physical run with its own per-block checksums. A partial overwrite can trim the surviving pieces by adjusting `disk_start` and `blocks` and release the overwritten middle immediately. btrfs keeps an inner offset because compressed extents cannot be cut, and pays for it with "bookend" space that stays allocated after partial overwrites. If compression is added later, compressed extents become indivisible units with their own kind and keep the inner-offset field then.

**Why cap extents at 32 MiB:** bounds the work for one split, limits checksum lookup scope and stops one huge write from monopolizing the allocator's largest free run. A file can contain arbitrarily many extents; the limit applies per record, not per file.

### 7.3 Inline small files

Files whose size is at most `INLINE_MAX = 2048` bytes store their bytes in an `INLINE` extent item in the fs tree leaf. Inline data is covered by the leaf's own CRC and BP. Growing past `INLINE_MAX` converts the inline item into a `REG` extent in the same transaction; shrinking does not convert back automatically.

**Why:** the benchmark corpora this project cares about (AxiomZig source, DVE manifests, MegaDoc attributes) are dominated by small files. An inline file costs zero data blocks, zero checksum entries and no extra seek. 2,048 bytes leaves room for at least one more item plus keys in a 4 KiB leaf and matches the common btrfs default.

**Why not convert back on shrink:** avoids flip-flopping on files whose size oscillates around the threshold.

### 7.4 Write path

`FsTransaction` gains byte-addressed operations:

```rust
fn create(&mut self, parent: Ino, name: &[u8], kind: Kind) -> Result<Ino>;
fn pwrite(&mut self, ino: Ino, offset: u64, bytes: &[u8]) -> Result<()>;
fn truncate(&mut self, ino: Ino, size: u64) -> Result<()>;
fn punch_hole(&mut self, ino: Ino, offset: u64, len: u64) -> Result<()>;
fn fallocate(&mut self, ino: Ino, offset: u64, len: u64) -> Result<()>;
```

`write_file(path, bytes)` remains as a convenience that truncates to zero and writes.

Dirty data is held in a per-transaction overlay of 4 KiB buffers keyed by `(inode, block index)`, with a byte budget. When the budget is exceeded, the oldest dirty runs are **spilled**: blocks are allocated (§8), written without sync, and recorded in the overlay as staged extents. A spilled block that is **known** to have never been published can return to the live transaction's private certified pool on an in-process abort, following F0.R1. After a power loss the corresponding address is an *unreconciled orphan* until a full independent scan proves it unreferenced; do not trust a stale in-memory ready set or assume crashed allocations can be returned immediately.

At commit:

1. Coalesce dirty blocks per inode into maximal runs (up to `MAX_EXTENT_BLOCKS`).
2. Allocate physical runs, preferring physical contiguity with the file's neighbouring extent (§8.4).
3. For every replaced range, split or trim existing extents and emit a **kill event** for every released physical run (§9.1).
4. Insert new extent items and checksum items.
5. Proceed with the existing F0.R1 ordering (§13).

Unaligned writes read-modify-write only the first and last 4 KiB blocks of the range.

**Why delayed allocation:** the allocator sees the whole transaction's writes at once and can place each file's data contiguously. ext4, XFS and btrfs all do this.

**Why spill:** without it, transaction memory is bounded by file size, which is row 1 of §2 again.

**Atomicity property gained:** every `pwrite` in a transaction becomes visible atomically at publication, at any size. ext4's `RWF_ATOMIC` support requires hardware atomic-write units and explicitly has no software or CoW fallback; a CoW filesystem can offer large untorn writes in software. This is worth exposing in the mount milestone.

### 7.5 Read path

`pread(ino, offset, len)` seeks to the greatest EXTENT key at or below `(ino, EXTENT, offset)`, iterates forward, reads covering blocks, verifies each against the checksum tree (batched by run), and fills holes and `PREALLOC` with zeros. Reads past end-of-file are clamped by the inode size.

### 7.6 Extent-map invariants (checked by the oracle)

- Extents of one inode are sorted and non-overlapping.
- `REG` and `PREALLOC` offsets are 4 KiB-aligned and `1 <= blocks <= MAX_EXTENT_BLOCKS`.
- At most one `INLINE` item, only at offset 0, only when inode size `<= INLINE_MAX`, and never alongside other extents.
- No extent starts at or beyond `round_up(size, 4096)` after truncate.
- Every `REG` block has exactly one checksum entry; `PREALLOC` blocks have none.
- `birth <= ` current generation.

---

**Conservative F5.2 progress:** [I3](INTEGRATION_I3_CERTIFIED_REUSE.md) now recycles physical pages only after independent **full two-F0-root ownership certification**. Small v10 images can survive repeated overwrites, but this is *not* a localized CoW free-range tree or incremental birth/dead-list algorithm. All performance and final F5.2/F5.3 proof gates below remain open.

## 8. F5.2 — extent free-space tree

### 8.1 Stable eligibility epochs

The free-space index is a sorted CoW tree keyed by physical run start. **Durable eligibility is absolute**, never re-encoded relative to the latest F0 generation:

```text
key   = start: u64 BE
value = blocks: u64, state: u8, reserved: 7 bytes (zero),
        eligible_after_generation: u64
```

`state = EPOCH` identifies an independently proven, released run. `state = DEFERRED` indicates a retirement candidate still awaiting proof/bounded sweep; it is **never** available for allocation irrespective of the generation. Other states are invalid in v10. A run in `EPOCH` state must be absent from the **newly published manifest's live/pinned ownership**. It may remain referenced by its **older F0 fallback**, but only with a future eligibility epoch that prevents allocation until that fallback rotates out. An EPOCH run may never be allocated while still referenced by either candidate.

**Precisely define eligibility:** let `g` be the currently selected, fully published F0 generation at the **start** of the next transaction. An `EPOCH` run is available for reservation only if `eligible_after_generation <= g`, the current handle is ownership-certified in `Full` mode (or the explicitly gated F5.3 `Accounted` mode), and the run is not claimed by that transaction. This distinguishes being *marked* ready in the newly published manifest from being safely allocatable while preparing it.

Example: a previously published block becomes fallback-only when g20 publishes. If g21 will rotate out its last F0 owner, store `eligible_after_generation = 21`. It is **not** allocatable while preparing g21 from g20, but it becomes eligible while preparing g22 from published g21. Its tree record needs no restamp in generations 22, 23, ...

`ready` and `pending` are **derived views** of EPOCH records with `eligible_after_generation <= g` and `> g`, respectively. Deferred is an explicit state, not an invented generation 0. Neighbouring EPOCH runs may coalesce only if their proof/retirement state and eligibility are compatible; retain separate records when a merge would hide a stricter eligibility boundary. A new node's birth generation remains independent of its free-run eligibility epoch.

**Never-published staged allocations:** addresses allocated from certified ready/virgin space but discarded before *any* F0 publication can be returned to the same transaction's private pool without a fallback delay. Do not confuse pages written before sync but never root-published with blocks reachable from a successfully published prior root.

**Why:** the v9 journal was rewritten every generation, so generation-relative encoding cost little extra. F5's locally updated CoW free tree must leave unrelated extents untouched through thousands of commits. An epoch recorded as "current generation" would demand a whole-tree restamp and defeat F5.2.

### 8.2 In-memory size index

After ownership certification, derive a size-class lookup index over only the **currently eligible, EPOCH** extents. Maintain this derived state incrementally within the single-writer handle after each published generation, including runs that become eligible by epoch maturation. If rebuilding it needs O(free extents) reads after an open, measure that cost separately from lazy read-open time; it is part of allocator certification, not the mount latency claim.

**Why one durable tree plus a derived index:** two independently updated durable size/address trees create an additional atomicity invariant; the derived index can always be reconstructed from the single checked free tree. Revisit only after measurement.

### 8.3 Conservatively virgin region

The descending high-water mark remains the boundary for candidate virgin addresses. Its **safety meaning is stronger than "not in the free tree"**: those blocks must be absent from every recoverable F0 candidate and from committed snapshot/accounting metadata. Where a newer damaged root or interrupted staging makes the boundary ambiguous, a failed/opened older handle must not assume all addresses below its own historical cursor were physically unwritten.

In `reuse_blocked` mode, allocate only if this virgin claim is demonstrated by the published geometry/high-water invariants for the surviving candidate(s) and the crash model. Never treat physical pages left by uncommitted writes as recoverable user data. Reserve F0 blocks 0–4 regardless of generation.

### 8.4 Allocation policy

Allocate requested data using adjacent ready extents, best-fit eligible extents, then split the request across multiple ready extents, followed by the proven virgin region. Physical locality is a **preference**, not a correctness requirement. Metadata pages generally prefer short runs; data benefits from long runs.

All candidates must pass the same epoch/certification checks and must be removed from the transaction's private availability set *before* staging writes. Two extents must never overlap, including partially consumed runs. If no legal run or safe metadata budget remains, return `NoSpace` without publishing the new root.

**Why:** fragmentation should reduce throughput, not force `NoSpace` when total safe data and metadata capacity is sufficient.

### 8.5 Geometry-aware metadata reserve

A fixed `max(256 blocks, 1% of image)` consumes most of a 384-block research image, while a constant 256 blocks does not prove that a worst-case delete will succeed on a much larger image.

**Design target, not a proven final constant:**

```text
usable_blocks = total_blocks - 5       // F0 slots and rotating payloads
reserve_target = clamp(ceil(usable_blocks / 1000), 16, 4096)
reserve = min(reserve_target, floor(usable_blocks / 10))
```

Only a separately proved *minimum legal recovery/delete metadata budget* can authorize a formatted F5 filesystem. If geometry yields a smaller reserve, **reject formatting or reduce supported operations with an explicit contract**; never silently promise that every delete will succeed. A reserve is measured in available **physical space**, not a second tree of blocks that are secretly shared with ready files. Ordinary data allocations cannot consume it; metadata-only progress (delete/truncate/snapshot drop and the corresponding CoW updates) may consume it under documented budgets.

**Why:** a percentage cap accommodates small disposable images and a maximum avoids an arbitrarily huge reserved percentage in a TiB-scale image. The formula is provisional; prove the per-operation bound first and benchmark actual peak page demand.

### 8.6 Free-tree self-allocation and bounded staging

Allocating pages to record allocations changes the free tree itself. A casual estimate such as `changed_leaves * (height + 1) * 2` is **not an established upper bound**: leaf splits, cascading separator changes, dead-list/checksum/snapshot updates, and retired accounting-tree pages can create new keys and additional CoW paths.

**Proposed non-recursive, all-or-nothing transaction:**

1. Compute a *candidate* transaction delta and a conservative upper bound on required metadata nodes using the actual tree-height, occupancy and mutation count, including maximum cascading splits and all checksum/dead-list/snapshot changes. Report that budget before any state is committed.
2. Reserve an exclusive **private metadata arena** from already certified ready space or the proven virgin cursor, debiting the metadata reserve exactly once. No free-tree path-copy routine may recursively allocate from its own newly generated release records.
3. Stage the data updates and all tree changes using only the private arena. Track every block allocated and every superseded unpublished intermediate separately from blocks retired from earlier F0 manifests.
4. Compute the final free-tree delta **once**, including consumption of reserved arena pages, used pages, and returned unused pages, then stage its final CoW root. This step must have an accounted, acyclic upper bound; if the budget is exceeded, discard this entire candidate and retry with a larger upfront reserve *before publication*.
5. Run the independent full-graph ownership/free-set oracle and metadata page-budget assertions in the research mode. No `ready` run may include a staged, live, fallback, snapshot or accounting block. Write immutable pages, sync, publish the new F0 payload, sync, publish the superblock, sync. Only after the final barrier does the handle replace its certificate and private arena.

**Stop condition:** no upper bound, no implementation approval. An exhausted arena is a recoverable `NoSpace` / retry with no published partial result, not permission for the free tree to allocate additional pages from uncertified space.

**Why:** CoW-space accounting has a dependency cycle; it must be broken by an independent reservation strategy, not a heuristic page count. The v9 full-run rewrite remains a sound fallback until the bounded algorithm is separately tested.

### 8.7 F5.2 acceptance examples

- Keep an unchanged EPOCH free-run record across at least **1,000 successful generations**; verify the same physical free-tree leaf is shared rather than restamped simply because `g` changed.
- Place a delayed F0-fallback-only run at `eligible_after_generation = 21`; prove it cannot be allocated by the g20→21 transaction and can be allocated when starting from published g21.
- At 384 blocks, fail only when an explicit and tested metadata reserve or safe-space constraint is reached; never silently reserve 256 blocks.
- Force worst-case free-tree leaf/root splits, snapshot descriptor updates and checksum edits inside one transaction; compare the arena's budget with measured staging. On insufficient reserve, assert the previous root and both recoverable manifests remain intact.

**Implemented opt-in foundation:** [F5.3 indexed allocator integration](F5_3_INDEXED_ALLOCATOR_INTEGRATION.md) publishes Free/DeadList CoW roots, reserved index metadata, and bounded retirement-cursor transitions with ordinary v10 transactions. **It still independently traverses both F0 ownership graphs each transaction**; no fully incremental reference-accounting proof or production qualification is established.

## 9. F5.3 — birth-time reclamation and dead lists

**Safety status:** birth-time reclamation is a proposed optimization, **not an established proof for BFS-NG's tree graph**. Through F5.2, keep the full two-root/snapshot reachability oracle as the allocator's default authority. F5.3 may replace the per-commit full scan only after the ownership model and gates below pass.

### 9.1 Ownership domains and kill events

There are **different kinds of roots**. A pinned filesystem snapshot retains only the trees explicitly captured by its snapshot descriptor:

| Domain | Owners | Retirement mechanism |
| --- | --- | --- |
| **Snapshot-scoped** | Inode/namespace/extent fs tree, attribute-row and attribute-posting trees, historical schema tree, and the data extents they reach | Candidate birth-time/dead-list algorithm; may have multiple snapshot references |
| **Manifest-global** | Physical checksum tree (covering all currently retained snapshot data), free-space tree, dead-list tree, snapshot catalog, allocator bookkeeping | F0 two-root retirement and bounded accounting sweep; **never** assume an arbitrary filesystem snapshot pins an old global-tree root |
| **F0-only root payloads** | Alternating superblocks and three reserved payload ring blocks | Fixed F0.R1 ring protocol; blocks 0–4 **never** enter the general allocator |

Every snapshot descriptor and every F0 candidate may point at different generations of metadata. Global checksum data *describes* retained snapshot blocks, but that does **not** turn a checksum-tree page into an ancestor of a snapshot's fs root. An old global-tree page born before a snapshot is not necessarily owned by that snapshot.

A `kill` is the removal of one logical reference from a new candidate root graph—not an unconditional permission to free the physical address. Carry:

```rust
struct KillCandidate {
    physical_start: u64,
    blocks: u64,
    birth_generation: u64,
    ownership_domain: OwnershipDomain,
    releasing_owner: RootIdentity,
    source: KillSite,
}
```

`KillSite` covers CoW path replacements, extent trimming/overwrite/delete, attribute-tree changes, checksum-tree changes, free-tree page replacements, dead-list changes and snapshot-catalog changes. The validator rejects double retirements, overlaps and birth/owner mismatches. Each method that drops a block pointer must emit a typed candidate or prove the pointer was never published. **A kill event is a claim to verify, not an assertion of last ownership.**

### 9.2 Candidate rules and F0 epoch fence

Let `g` be the generation being published, `b` the allocation birth generation, and `s` the newest pinned snapshot generation in the proposed new manifest.

| Candidate | Proposed disposition | Required proof |
| --- | --- | --- |
| Allocated and discarded entirely within this transaction, never F0-published | Return to transaction-private certified ready pool | Address absent from all recoverable roots and no staged reference remains |
| Published **global** page removed from the new manifest | Retire on F0 fallback rotation using absolute `eligible_after_generation` | Neither snapshot nor latest root references it; only a still-valid fallback manifest may do so |
| Published snapshot-scoped page or data run removed from live namespace; `b > s` or no snapshots | Fallback-fenced release candidate | Prove no pinned descriptor and no other logical references; delay availability beyond last F0 owner |
| Published snapshot-scoped page or data run removed from live namespace; `b <= s` | Candidate entry in newest-snapshot dead list | Prove that the newest snapshot actually reaches this block under the no-clone lineage assumptions; birth comparison alone does not substitute for this proof in the implementation gate |
| Clone/reflink/shared, reference count unresolved, or damaged ownership | **Quarantine**; never release | Full multi-owner reference accounting is required |

Even when a dead-list entry becomes logically unreferenced after a snapshot drop, it is not immediately allocatable if an **older F0 manifest still pins that snapshot**. Record the relevant absolute release-after generation and wait for rotation plus certified ownership. Global tree pages are never inserted into per-snapshot dead lists merely because their birth predates `s`.

**Why this differs from the original rule:** the earlier text applied `b <= s` to every tree under the manifest, which can strand free-tree and checksum-tree pages that no snapshot actually owns. It also conflated "no longer referenced by the new root" with "absent from both recoverable roots."

### 9.3 Candidate dead-list representation

```text
key   = snapshot_gen: u64 BE | birth: u64 BE | start: u64 BE
value = blocks: u32, ownership_domain: u8, reserved
```

A `D(s)` entry is a **witness of a physical run retained by a snapshot lineage but no longer live**. It is not itself another live extent pointer and must not be interpreted as an independent exclusive physical owner. Multiple snapshots may still reference its described physical blocks.

A snapshot pin stores a **complete descriptor** (§6.5), including historical attribute/schema roots; creating the descriptor can be O(1) in fs tree size but still mutates the snapshot-catalog pages. The new snapshot's dead list starts empty.

Only snapshot-scoped blocks can enter `D(s)`; global accounting tree pages have a different retirement path. Before migration to `Accounted`, independently reconstruct the snapshot root reachability for every entry and verify that its birth, range and canonical dead-list assignment agree. Data-checksum entries for these runs remain in the global checksum tree while any descriptor or F0 fallback can require them.

### 9.4 Dropping a snapshot, including a middle snapshot

Let `s` be the descriptor being dropped, `p` the nearest surviving older snapshot, and `n` the nearest surviving newer snapshot.

The original proposed split of `D(s)` by `birth > p` versus `birth <= p` is a **candidate algorithm**, not a completed proof. In particular, a block may have multiple historical snapshot references, and an F0 fallback can retain a snapshot that the new manifest has dropped.

The implementation must, for each affected run, decide whether the surviving descriptors and **both** F0 candidate generations still reach it. If an older surviving snapshot retains the run, move its canonical dead-list witness to the correct interval; otherwise record an F0-epoch-fenced retirement, **not** an immediate free. Newer-snapshot witnesses must not be removed just because `s` disappears. Retain the dropping descriptor and progress marker until every required move or retirement is durably represented.

**Required proof statement:** for any no-clone history of creates/writes/overwrites/truncates, snapshot pins and arbitrary drops, every physical snapshot-scoped block referenced by any surviving pinned root is excluded from the allocatable set; each dead-list entry has a well-defined canonical lineage position; dropping a middle snapshot preserves that property. Prove with a small executable model before making this the allocator's authority. If the proof fails or the model disagrees, fall back to full two-root reachability scanning, without a heuristic partial fix.

### 9.5 Bounded processing

Dropping a snapshot can produce millions of interval moves and retirements. Mark it `dropping` and persist a **versioned processing cursor** in the manifest. Each committed `reclaim_step()` processes at most a configured budget of canonical entries; it must durably publish both the cursor and the dead-list/free-tree changes in the same F0 root transaction. Power loss resumes from the last **fully published** generation; processing is replay-safe and idempotent on reopening.

While a descriptor is `dropping`, treat every not-yet-processed reference it protects as *still retained*, including across F0 fallback candidate rotation. Persisting a cursor must never authorize blocks from an incomplete interval.

### 9.6 Reuse trust modes and exact invariants

`reuse_verification = Full | Accounted` is an in-handle policy, not an on-disk excuse to trust unverified free-space entries.

- **Full (default until proof):** independently reconstruct both F0 candidate graphs and all pinned snapshot trees, including all global metadata roots, before certifying new reusable regions. An unopened or damaged older root is a quarantine condition, not a reason to presume its blocks are free.
- **Accounted (future F5.3 only):** use incrementally updated kills/dead lists/epochs as the fast-path authority *only after* the exact model and fault tests prove agreement with Full. Reverting to Full on discrepancy is mandatory; a periodic scrub is an additional detector, not a replacement for validation before enabling Accounted.

**Correct set invariants** (physical addresses are compared per actual block/range, not just identical extent start keys):

1. `Allocatable(g) ∩ Reachable(current_F0, fallback_F0, pinned_snapshots) = ∅`.
2. A future-epoch pending record **may overlap fallback ownership** by design; it must not overlap current-root ownership, and cannot enter `Allocatable` before its absolute epoch matures.
3. A dead-list witness **may overlap blocks reachable through a snapshot**. It is an accounting relationship, **not** a mutually exclusive class of physical space. Dead-list entries can never be used as unverified free-space records.
4. Every published metadata/data reference has a valid birth and checksum contract; every live/pinned `REG` data block has exactly one current checksum entry under the selected F0 manifest. Global accounting pages remain protected through the complete F0 two-root fence.
5. No candidate block is freed twice, no overlapping subrange has mismatched retirement state, and no `DEFERRED` or unverified orphan region becomes allocatable because an epoch counter advanced.
6. The independent complete graph oracle is the authority for the testing contract: the proposed `Accounted` free-set must match exactly, under snapshot pin/drop, F0 rollback, arbitrary payload/data corruption and every injected crash point.

The old invariant "every block is exactly one of live, dead list, free tree, pending, or virgin" is **false** as stated: snapshot-owned data can legitimately appear in a dead-list witness, and F0 fallback ownership can overlap a pending free-run record. Distinguish *referenced*, *retirement evidence*, and *allocation eligibility*.

**Failure asymmetry:** missing kills leak space; an incorrect or duplicate kill can destroy live/snapshot data. Optimize only after correctness is established with the complete oracle.

### 9.7 What can eventually retire

On successful F5.3 proof and rollout, ordinary commits can stop reconstructing entire live/pinned reference counts. The F2.R2 CoW reference tree may become a testing/oracle or migration-only structure for unshared data, with future clone-specific accounting independent of the dead lists. The F2.R3/R4 whole-run journal can be replaced by the CoW free tree and dead-list metadata. Until then, **retain existing conservative reference accounting, delayed-free fences, and fail-closed recovery**. A speed claim that merely removes verification without a matching ownership proof is not an accepted F5 milestone.

**Partial integration:** [I2](INTEGRATION_I2_V10_NAMESPACE.md) publishes a **path-keyed** multi-file namespace with stable inode IDs and CoW per-file extents. It provides the atomic integration test for v10 file contents, checksums and snapshots. **It does not yet deliver the proposed `(parent inode, name)` index, hard links or incremental path-copy updates**; those remain in this section.

## 10. F5.4 — inode table and directory entries

### 10.1 Records

Inode record, key `(ino, INODE)`:

| Field | Width |
| --- | ---: |
| Kind (file, directory; symlink reserved) | 1 |
| Flags | 1 |
| Reserved | 2 |
| Link count | 4 |
| Size | 8 |
| Creation generation | 8 |
| Parent inode (directories only) | 8 |
| mtime, ctime (ns since epoch) | 16 |
| Mode, uid, gid | 12 |

Directory entry, key `(parent_ino, DIRENT, name)`, value `(child_ino: u64, kind: u8)`. Names are 1 to 255 bytes; v10 accepts UTF-8 only, as v9 does. The root directory is inode 1. Inode numbers come from `next_inode` and are never reused.

**Why:** rename becomes delete-one-entry plus insert-one-entry instead of rewriting every descendant. Hard links become possible (several entries, one inode, link count). Keys shrink from up to 240 bytes to a few dozen, which is what lets fanout reach roughly 80. Never-reused inode numbers keep ARCHITECTURE commitment 3: a recreated file cannot inherit stale postings.

**Why sorted names rather than hashed names:** directory listing is then a plain range scan in name order, and there are no hash collisions to handle. btrfs keeps both a hash index and a sequence index; one sorted index is enough for a single writer.

**Why carry POSIX fields now:** the mount milestone needs them, and adding them later is another format break. Enforcement of permissions stays out of scope until then.

### 10.2 Operations

`lookup`, `create`, `mkdir`, `unlink`, `rmdir`, `rename` (with a check, by walking parent inodes, that a directory is not moved into its own subtree), `link`, `readdir(cursor)` and `stat`.

**Cost accepted:** path resolution is now one lookup per component, O(depth × log n), instead of O(log n). The C5 metadata cache, once integrated, is the mitigation. Every real filesystem pays this cost for cheap renames.

**API impact:** `scan_files_disk(prefix)` becomes a directory-subtree walk with a cursor of `(directory inode, last name)` per level. The generation-scoped cursor rule stays.

---

## 11. F5.5 — attribute catalog off the file path

The `/.bfs_f4_attrs` file disappears. The manifest points at two CoW trees under the same root:

- **Rows:** key `(ino, attr_id)`, value: canonical typed value.
- **Postings:** key `(attr_id, canonical value bytes, ino)`, empty value.

Updates path-copy only affected leaves. Snapshot queries read the snapshot's trees. File deletion removes its rows and postings in the same transaction, as today.

**Why:** removes row 9 of §2 and is the storage half of backlog item P1 ("incremental typed posting index"). Query planning, change feeds and cursors stay in a separate F4.R1 spec.

---

## 12. F5.6 — shared extents (optional, off by default)

Clone (`FICLONERANGE`-style) lets several inodes and snapshots reference overlapping *subranges* of the same physical extent. **Do not implement** the earlier candidate `(start, blocks, extra_refs)` share table as-is: a fixed physical-start key cannot express every interior split or partial clone. The actual interval-partitioning/refcount codec belongs to a separate F5.6 design review. Cloning may mark both extents with `MAY_BE_SHARED`, but only as a conservative hint that must be propagated across all splits and copies. The original simple keyed-by-start share table is **not approved for implementation**: partial clones, overlapping subranges and split extents can make independent physical intervals share one address range. Future sharing requires an **interval-aware, normalized reference model** that splits/coalesces on every change, counts *all* live and snapshot references, and delays physical release until both F0 candidates and every saved descriptor are clear. The `MAY_BE_SHARED` flag may optimize lookups, but must never by itself authorize freeing a region or skipping a required reference check.

**Why the flag bit:** OpenZFS block cloning has no such bit, so every free must check its Block Reference Table. A new format can add it cheaply.

**Why off by default:** OpenZFS 2.2.0 shipped block cloning enabled and users hit silent corruption; 2.2.1 disabled it and 2.2.2 fixed it. Sharing interacts with every kill site. It ships only after the oracle covers clone, overwrite-of-clone, snapshot-of-clone and drop-of-clone-source under fault injection.

---

## 13. Commit ordering

The F0.R1 barrier order remains unchanged; **the source of allocation authority does not change until F5.3 is proved**:

1. Start with a certified live generation and a complete snapshot catalog; gather a transaction-local delta to fs, checksum and attribute trees. A transaction that pins a snapshot captures one **well-defined pre-commit view** (the current published roots); a future pin-new-generation API would need a separate explicit contract.
2. Reserve data runs and an independent, pre-bounded metadata arena (§8.6), only from certified ready or proven virgin space.
3. Path-copy the fs tree and the exact historical attribute/schema descriptors; preserve physical checksums for *all* live and pinned data (§6.4). Build typed release candidates, distinguishing global-tree pages from snapshot-owned pages (§9.1).
4. Apply stable eligibility epoch updates and bounded deferred/dead-list bookkeeping. Do **not** promote a run merely because a fallback will rotate on this same commit.
5. Apply the final free-tree delta once, consuming its reserved arena. Return genuinely never-published, unused reservations to the private pool; emit no accounting record that overlaps a staged page or two-root owner.
6. In `Full` verification mode, independently verify the **staged new candidate and existing F0 fallback** plus all snapshot descriptors, checksum reachability, free-run eligibility and metadata self-allocation. Any disagreement aborts without publication. Later `Accounted` mode needs the F5.3 proof gate.
7. Write all staged immutable metadata and data blocks not already spilled. **Sync.** Prewritten spill blocks are included in this barrier; uncommitted orphan cleanup remains a separate verified operation.
8. Write the new v10 manifest into the next F0 payload ring slot. **Sync.**
9. Write the alternate F0 superblock. **Sync.**
10. Only after final successful sync update the writer's state/certificate. Any ambiguous write/sync failure poisons the handle and requires reopening under the read-vs-reuse policy (§6.6).

**Not in F5, but unblocked by it:** group commit (several transactions per generation, with `fsync` meaning "wait for the next published generation"). This belongs in the mount milestone. It matters for throughput and for how fast generations are consumed.

## 14. Decision log

| Decision | Current v10 proposal | Rejected / deferred option | Reason |
| --- | --- | --- | --- |
| Generation width | `u64` in all new references and metadata | Continue v9's `u32` data birth | Prevent practical lifetime commit ceiling |
| Format staging | F5.0a pointer/descriptor foundation followed by F5.0b data/checksums | Expose partially implemented v10 disk images as stable | Isolate format and recovery changes while keeping a single public format break |
| Snapshot identity | Versioned descriptor with fs, attr-row, attr-posting and schema roots | Fs-only BP in snapshot catalog | Otherwise historical attribute queries lose roots after manifest rotation |
| Data checksums | Global physically keyed tree covering **all live and pinned snapshot data** | Remove checksums at first live overwrite | Ensure historical `REG` reads remain verifiable without per-snapshot checksum-tree pinning |
| Data layout | Headerless, 4 KiB-addressable blocks with authenticated checksum-tree root | v9 in-band per-data-block header | Align partial reads; 4 KiB addressability does not guarantee hardware atomicity |
| Metadata pages | Generic slotted CoW page with self address, volume, CRC and parent-held birth/CRC BP | Parent or child checksum alone | Detect stale/foreign metadata and support offline identification |
| Free runs | Absolute `eligible_after_generation` with separate deferred state | Relative "current" vs "current + 1" restamping | Avoid rewriting unchanged free-tree pages each commit |
| Free-space index | One durable extent tree plus derived in-memory size index | Two separately synchronized durable indexes | Maintain one authoritative on-disk ordering |
| Virgin high-water | Conservatively checked against both recoverable manifests | Assume old cursor means never written after any crash | Avoid trusting addresses potentially written by failed generations |
| Metadata reserve | Geometry-aware, derived from proved CoW delete budget | Fixed `max(256, 1%)` on every test image | Tiny images need proportional reservations; capacity alone does not prove deletion progress |
| Free-tree self-allocation | Bounded private metadata arena and one final accounted delta | Estimate-only, recursively allocate in free tree | Termination and atomic accounting require a verified upper bound |
| Mount/recovery | Lazy *read* verification; separate Full ownership certification before reuse | Lazy open silently upgrades allocator authority | Data-read latency and overwrite safety are different guarantees |
| Snapshot reclamation | Birth/dead-list accounting **only for snapshot-scoped roots**, pending a model proof | Apply `birth <= newest_snapshot` to all global metadata | Snapshot pins do not own historical global free/checksum/dead-list trees |
| Epoch retirement | Protect last F0 fallback owner in addition to logical snapshot lifetime | Free immediately when final snapshot drops | Earlier F0 manifest may still be recoverable |
| Kill semantics | Typed ownership-domain-aware release candidate with full-graph oracle | Assume dropping one BP proves last physical owner | Shared pages and overlapping snapshot roots invalidate unconditional frees |
| Reuse authority | **Full** two-root/snapshot check until F5.3 gate; Accounted opt-in after proof | Turn off full checks as part of lazy mount | Fail closed without sacrificing the eventual incremental fast path |
| Dead-list and free-space accounting | Model referenced set, retirement evidence and allocatable set **separately** | False "exactly one of live/dead-list/ready/pending" partition | Dead-list witnesses and pending epochs may overlap referenced blocks |
| Snapshot destruction | Durable `dropping` state with bounded idempotent cursor | Free the entire dead list in one commit | Crash-resumable progress and F0 epoch safety |
| Namespace | Inode table and `(parent, name)` keys | Full pathname key for every file | Localized rename, hard links, higher fanout |
| Indexed attributes | Separate CoW row/posting trees captured in snapshot descriptor | Rewritten private F4 catalog | Incremental updates and historical query correctness |
| Shared extents | Disabled, until normalized physical interval ownership oracle | Bare `(start, blocks, extra_refs)` share table | Partial clones/splits cannot be safely counted by whole-run start alone |
| Compatibility | Single v10 format break, v9 test images rejected | Silent migration or successive incompatible v10/v11 versions | Explicitly testable representation and recovery boundaries |

## 15. Validation plan and acceptance gates

The reference model is **independent of the fast allocator**. Simulated crash outcomes must be either the complete pre-commit published state or complete post-commit published state, never a mixed namespace, checksum map, snapshot descriptor or free-space checkpoint.

### Independent reference models

- **File model:** `BTreeMap<Ino, SparseBytes>` with inode/directory maps; holes and uninitialized PREALLOC read as zero.
- **Reference model:** complete traversal of both F0 manifests and every pinned snapshot descriptor; counts physical data and metadata pages by address, birth and ownership domain. Reconstruct historical attr/schema and checksum coverage independently of incremental kill events.
- **Space model:** per-block `Reachable(current, fallback, snapshots)`, certified/released epoch and private staged reservations. Model distinguishes `Allocatable`, *pending fallback-owned*, *deferred*, *virgin* and *orphan*, without claiming the classes always partition referenced blocks.
- **Attribute model:** M0 rows/postings oracle plus historical per-snapshot schema semantics.

### Milestone gates

| Milestone | Required reproducible gate |
| --- | --- |
| **F5.0a** | Complete `SnapshotDescriptorV10` codec and schema BPs; pin at g, change rows/postings at g+1, rotate 3+ F0 roots, query g's historical values. Inject torn writes and corrupt an attribute/schema root; reject inconsistent snapshot data. Test-only `u64` start near `2^32` without skipping F0 publication generations. |
| **F5.0b** | Headerless data checksum tree covers exact union of live/pinned `REG` blocks; live overwrite never removes old snapshot checksum. Lost and misdirected data writes are detected **when read**; checksum CRC failures fail closed for reads and reuse. Lazy open has bounded root reads while first untrusted free-tree allocation still runs independent full certification. |
| **F5.1** | Random `pwrite`, `truncate`, `punch_hole`, `fallocate`, append and crash/reopen match sparse-file reference states. Include unaligned data tails, 2,047/2,048/2,049-byte inline transitions, spill-before-sync and crash-only orphan handling. |
| **F5.2** | Stable free-run epoch does not require leaf copy across 1,000 unaffected generations; fallback-owned run g20→g21 not allocatable until starting from published g21. Prove no space aliasing under fragmentation and random CoW splits; geometry-aware reserve works on small image; worst-case metadata arena never recursively allocates, aborts atomically if insufficient. |
| **F5.3** | Formal small-state model plus property/fuzz tests for arbitrary snapshot creation, nonadjacent/middle drops, parent/child tree changes, large global free/checksum/dead-list churn, F0 rollback and crash. A snapshot retained during thousands of global-tree updates must **not** accumulate dead-list entries for those global pages. Oracle agreement **exactly** before every release; every slice of every run respects birth/epoch/ownership. Only then make Accounted a benchmark opt-in. |
| **F5.4** | Namespace oracle covers hard links, unlink with remaining links, directory rename with many descendants, rename-into-descendant rejection and generation-stale enumeration cursors. Rename of large subtree updates changed paths, not all descendants. |
| **F5.5** | Pin snapshot with old attribute schema/rows; mutate schema-compatible live rows; rotate F0 roots; verify historical query against descriptor's own row/posting/schema BPs. A single attribute edit writes O(height) CoW pages rather than serializing the full catalog. |
| **F5.6** | Remains off until normalized overlapping physical refcounts are tested under arbitrary clone ranges, split-of-split, write-into-clone, truncate, snapshots, drop-middle-snapshot and every publication crash point. |

**F5.3 required negative cases:** (a) an old global free-tree page born before a snapshot must not enter its dead list, (b) a dead-list witness overlapping a snapshot-owned block must not be offered as free, (c) a pending run still owned by the last fallback must not be allocated, (d) damaged latest-root ownership must not be inferred from a lazy-open success, and (e) two overlapping subranges with different eligibility epochs must never be coalesced into an early-ready run.

### Benchmarks (disposable images only)

Measure metadata pages read/written, page lifetime, ready-run and deferred-run counts, actual logical/data bytes, commit latency and recovery/certification time separately. Workloads include small-file create, 4 KiB overwrite of large files, append, large fragmented allocation, snapshot pin+overwrite+drop, F4 attribute edits, AxiomZig/DVE/MegaDoc-like access patterns. Compare v9 wherever operation semantics match, without treating modeled cache refill penalties as actual device latency.

**Acceptance principle:** reducing full scans is an optimization only when the independently checked set of reusable physical addresses is identical to the conservative oracle. A throughput improvement that silently weakens that equivalence is a failed milestone.

## 16. Risks and open questions

1. **Most important: ownership-domain completeness.** Confirm every future metadata tree's owner and retirement rule; snapshot birth comparisons cannot blindly govern global allocator structures.
2. **Snapshot schema and checksum retention.** All historical row/posting/schema roots must survive until descriptor retirement; a global checksum tree must cover every retained file data block through its last F0 owner.
3. **Free-tree reservation proof.** A reserved arena must have a termination bound covering worst-case splits, tombstones and secondary accounting pages. The §8.5 reserve formula is a target pending a measured/proved minimum legal geometry.
4. **Birth/dead-list proof.** Formalize and model the distinction between canonical lineage witnesses, physical references and allocatable blocks, including arbitrary middle snapshot drops. The original sketch alone is insufficient.
5. **Lost writes and lazy recovery.** CRC32C is accidental-error detection, not authentication. A successful lazy open has no authority to release unchecked physical addresses.
6. **Spilled orphans.** Staged addresses from failed writes can be unjournaled; never return them after a crash without independent ownership proof.
7. **Clone interval accounting.** Physical range splitting and multiple logical references require normalized refcounts; keep F5.6 disabled until proved.
8. **Workload cost.** Checksum-tree writes, mtime updates and full certification remain measurable hotspots. Avoid claiming end-to-end O(log n) until reference accounting and namespace overlays also avoid volume-wide work.
9. **Open questions:** reserve formula after page-budget proof; PREALLOC charging policy (proposed: ordinary data space); deferred/dead-list sweep budget (start experimentally at 256 canonical entries); future schema migration and ACL semantics.

## 17. What F5 does not do

Compression, encryption, content-hash dedupe, multi-device redundancy or repair, back-references, concurrent writers, the FUSE / WinFsp adapters, group commit, online defragmentation, and v9 → v10 migration. Reserved fields exist for compression kind, checksum kind and node size; all must be zero (or 4096 for node size) in v10.

## 18. References

- BFS-NG [ARCHITECTURE.md](ARCHITECTURE.md), [F0.R1](F0_R1_ROTATING_ROOTS.md), [F2.R2](F2_R2_INCREMENTAL_REFERENCES.md), [F2.R4](F2_R4_CERTIFIED_RECLAMATION.md), [F4](F4_DURABLE_ATTRIBUTES.md)
- Btrfs design (file extent items, inline extents, checksum tree, extent reference counts): https://btrfs.readthedocs.io/en/stable/dev/dev-btrfs-design.html
- OpenZFS block cloning and the 2.2.0–2.2.2 history: https://slicker.me/zfs/block-cloning.html
- OpenZFS documentation (block pointers, birth transaction groups, dead lists): https://openzfs.github.io/openzfs-docs/
- ext4 atomic writes (`RWF_ATOMIC`, no CoW fallback): https://kernelsources.org/source/xref/linux/Documentation/filesystems/ext4/atomic_writes.rst
- XFS delayed logging design: https://docs.kernel.org/filesystems/xfs/xfs-delayed-logging-design.html
- Practical File System Design (BeFS block runs and small data): https://www.haiku-os.org/legacy-docs/practical-file-system-design.pdf
- ShardStore validation approach (SOSP 2021), summary: https://muratbuffalo.blogspot.com/2021/10/using-lightweight-formal-methods-to.html
- FUSE over io_uring (Linux 6.14), for the later mount milestone: https://www.phoronix.com/news/Linux-6.14-FUSE
