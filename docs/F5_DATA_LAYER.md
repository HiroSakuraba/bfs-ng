# F5 — extent data layer, birth-time reclamation and inode namespace (proposed)

**Status:** design proposal. **Nothing in this document is implemented.** Target format: F0 superblock **v2** (unchanged) and root manifest **v10**. Disposable single-writer test images only; v9 images are rejected, not migrated. Source code and tests remain the authority once any part of F5 lands.

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

The F0 publication protocol, the two-root reuse invariant and the "oracle referees the fast path" methodology are kept unchanged.

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
| **F5.0** | Format v10 foundation: `u64` generations, generic CoW B+tree page format, block pointers, headerless data blocks, checksum tree, lazy verification at open | — |
| **F5.1** | Extent map, inline small files, streaming writes, partial overwrite, truncate, punch hole, preallocate | F5.0 |
| **F5.2** | Extent free-space tree with epoch tags, allocation policy, metadata reserve | F5.0 |
| **F5.3** | Birth-time reclamation and dead lists; retire per-commit `count_roots` | F5.1, F5.2 |
| **F5.4** | Inode table and `(parent, name)` directory entries; rename, hard links | F5.0 |
| **F5.5** | F4 attribute rows and postings in their own CoW trees | F5.0, F5.4 |
| **F5.6** | Optional shared extents (clone), off by default | F5.3 |

F5.0 through F5.4 together are one format break (manifest v10). They can land as separate PRs behind a test-only format flag, but only the combined result is published as v10.

**Why one format break:** v9 policy is to reject old images rather than migrate. Shipping v10, v11 and v12 in quick succession would churn every test fixture three times for no user benefit.

---

## 6. F5.0 — format v10 foundation

### 6.1 Generations are `u64` everywhere

Every stored generation (birth generations in pointers and extents, snapshot generations, journal eligibility, dead-list keys) is `u64`. The `generation > u32::MAX` check in `commit` is removed.

**Why:** F0 already uses `u64`. The `u32` limit exists only because the data block header had 4 spare bytes. A volume-lifetime commit ceiling reachable in weeks is a format bug, not a tunable. Eight bytes per pointer is negligible.

### 6.2 Generic CoW B+tree page

All F5 trees (fs tree, checksum tree, free-space tree, dead-list tree, attribute trees) share one page format. Pages are 4,096 bytes.

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

**Why 4 KiB pages:** smaller pages keep CoW amplification low for small updates, and 4 KiB matches the minimum atomic write unit of common devices. A `node_size` field is reserved in the manifest; larger nodes are a measured follow-up, not a default.

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

Data blocks contain only file bytes. Tail bytes past end-of-file in the last block are zero.

Data checksums live in the **checksum tree**, keyed by physical address:

```text
key   = csum_run_start: u64 (big-endian)
value = count: u16, reserved: u16, crc32c[count]: u32...
```

One item covers a run of physically contiguous data blocks, up to the space available in a leaf (about 1,000 checksums, so about 4 MiB of data per leaf page). Checksum entries are inserted when a data block is written and deleted when that block is freed (§9).

**Why headerless:** removes every problem in rows 5, 6 and 8 of §2. File offset *n* now lives at byte *n mod 4096* of a block, so page-cache, mmap, O_DIRECT and device atomic-write alignment all become possible. Blocks no longer name their owner, so they can be shared.

**Why a separate tree rather than checksums in the extent record:** a 32 MiB extent has 8,192 blocks; inline checksums would make extent records 32 KiB. A separate tree keeps extent records fixed-size, lets cloned extents share checksums for free, and lets scrub walk data in physical order. This is the btrfs design.

**Why keyed by physical address:** a checksum describes physical bytes. Keying by `(inode, offset)` would force a checksum rewrite on every clone and make shared blocks carry two copies.

**Why per-4 KiB checksums:** any aligned 4 KiB read can be verified alone (row 8 of §2). The cost is 4 bytes per 4 KiB, about 0.1%.

**Lost and misdirected data writes:** the checksum tree is written in the same transaction as the data. A dropped data write leaves stale bytes that fail the new checksum; a misdirected write fails at both the intended and the overwritten location. Detection is equivalent to v9's header checks without the header.

**Why CRC32C:** it is already in the codebase, hardware-accelerated and adequate for media-error detection. A checksum-kind field is reserved for a stronger hash if content dedupe is ever added; CRC32C is not suitable as a dedupe key.

### 6.5 Root manifest v10

The F0 superblock v2 and three-block root-payload ring are unchanged. The payload carries manifest v10:

| Field | Width | Notes |
| --- | ---: | --- |
| Magic | 8 | `BFSNGM10` |
| Format | 4 | 10 |
| Node size | 4 | Must be 4096 in v10 |
| Generation | 8 | |
| High-water mark | 8 | Lowest block ever written from the never-written region (§8.3) |
| Next inode | 8 | Never reused |
| fs tree root | 24 | BP |
| Checksum tree root | 24 | BP |
| Free tree root | 24 | BP |
| Dead-list tree root | 24 | BP |
| Snapshot catalog root | 24 | BP (a leaf of `(snapshot_gen → fs root BP, flags)`) |
| Attr row tree root | 24 | BP, zero if no schema |
| Attr posting tree root | 24 | BP, zero if no schema |
| Sweep cursor | 16 | `(snapshot_gen, key position)` for bounded dead-list processing (§9.5) |
| Metadata reserve | 8 | Blocks (§8.5) |
| Counters | 32 | Inodes, used blocks, free blocks, pending blocks |
| CRC32C | 4 | |

Well under the existing payload capacity.

### 6.6 Lazy verification at open

On open, verify both F0 candidates, their manifests, and the root page of each tree. Do **not** read every page or every data block. Every later read verifies what it touches through BPs and the checksum tree. Full verification moves to `scrub()` and `allocation_report()`.

**Why:** v9's full verification makes mount time grow with total data. No production filesystem works that way.

**What this costs, stated honestly:** v9 can fall back to the previous root if *any* part of the newest tree or file data is corrupt. v10 falls back only when corruption is visible at the manifest or tree-root level. Deeper corruption found later returns an error for the affected file or directory; it does not silently roll the whole volume back. That is the right behavior for a real filesystem (rolling back a whole volume because one file has a bad block loses every other change), but it is a semantic change and tests must be updated to assert it.

**Fail-closed rule kept:** if certification (§8.4) or a scrub finds an ownership structure it cannot verify, reuse stays blocked and allocation falls back to the never-written region, exactly as v9's `reuse_blocked`.

---

## 7. F5.1 — extent map, inline files and streaming writes

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

**Why cap extents at 32 MiB:** bounds the work for one split, keeps checksum lookups to a handful of leaves, and stops one huge write from monopolizing the allocator's largest free run.

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

Dirty data is held in a per-transaction overlay of 4 KiB buffers keyed by `(inode, block index)`, with a byte budget. When the budget is exceeded, the oldest dirty runs are **spilled**: blocks are allocated (§8), written without sync, and recorded in the overlay as staged extents. A spilled block is never-published, so on abort it returns to the ready pool immediately, by the same rule as the F0.R1 staging correction.

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

## 8. F5.2 — extent free-space tree

### 8.1 Free tree

```text
key   = start: u64 BE
value = blocks: u64, eligible_generation: u64
```

`eligible_generation` keeps the F2.R4 meaning:

| Value | Class |
| --- | --- |
| current generation | **ready** |
| current generation + 1 | **pending** (owned by the fallback root until the next rotation) |
| 0 | **deferred** (released, awaiting bounded processing, §9.5) |

Adjacent free extents merge only when they have the same class.

**Why carry the epochs into the tree:** the two-root reuse argument from F2.R3/R4 and F0.R1 is the strongest part of the current design. F5 changes how free space is indexed, not when it may be reused.

### 8.2 In-memory size index

At certification, the allocator reads the free tree (O(free extents), not O(all metadata)) and builds an in-memory index of ready extents by size class (powers of two). It is derived state, never written to disk.

**Why one durable tree plus a derived index (rejected: XFS-style two durable trees):** two durable trees double the CoW path copies per allocation and create a new cross-tree consistency invariant to prove. The size index is cheap to rebuild and cannot disagree with anything on disk. Revisit if open-time rebuild cost is measured to matter.

**Why not bitmaps:** a bitmap cannot carry a per-run eligibility epoch without one generation per bit, and finding a large run means scanning.

### 8.3 Never-written region

The v9 descending virgin cursor is kept as the **high-water mark**. Blocks below it that have never been written are not in the free tree; the allocator takes them by moving the mark down.

**Why keep it:** it is the fail-closed escape hatch. When ownership cannot be verified, v9 still allows commits from space nothing has ever referenced. Folding that space into the free tree would make every allocation depend on the free tree being readable.

### 8.4 Allocation policy

For each run of `n` blocks requested at commit:

1. **Goal:** if the inode has a neighbouring extent, look for a ready extent starting at its physical end, then nearby in the free tree.
2. **Best fit:** smallest size class with an extent of at least `n` blocks.
3. **Split:** take the largest available extents until `n` blocks are covered; the file gets several extents.
4. **High-water:** take fresh blocks below the high-water mark.
5. Otherwise `NoSpace`.

Metadata pages prefer small extents (filling holes); data prefers large ones.

**Why step 3 exists:** it is the direct fix for row 3 of §2. A file may span many extents; fragmentation degrades performance, not correctness.

**Why separate preferences for metadata and data:** keeps large runs available for data and stops scattered 4 KiB metadata pages from fragmenting them. btrfs gets the same effect with separate block groups; a preference is enough at this scale.

### 8.5 Metadata reserve

`metadata_reserve = max(256 blocks, 1% of image)`, fixed at format. Ordinary writes may not allocate into the reserve. Deletes, truncates, snapshot drops and the commit's own tree updates may.

**Why:** in a CoW filesystem, freeing space costs space (every delete path-copies pages). Without a reserve, a full volume can reach a state where nothing can be deleted. btrfs spent years on this class of ENOSPC bug.

### 8.6 Updating the free tree without recursion

Allocating blocks changes the free tree, and changing the free tree allocates blocks. F5 breaks the loop at commit:

1. Collect the commit's allocations and releases as an in-memory delta.
2. Estimate the pages the free tree, checksum tree and dead-list tree updates will need (`changed leaves × (height + 1) × 2`, plus margin) and reserve that many blocks from ready space up front.
3. Apply the delta and path-copy the trees, allocating their pages only from that reservation.
4. Return unused reservation blocks to ready (they were never published).
5. Old pages replaced in step 3 are released by the normal kill rule in this same commit (§9.2).

**Why:** the v9 journal dodges this by being rewritten in full each commit. An incremental tree cannot. A fixed up-front reservation is simple to prove terminating; ZFS's log-structured space maps are the main alternative and are worth benchmarking if free-tree write amplification turns out high.

---

## 9. F5.3 — birth-time reclamation and dead lists

This replaces the per-commit `count_roots` reconstruction and the F2.R2 physical-reference tree for the snapshot case.

### 9.1 Kill events

A **kill** happens when the live fs tree, or any other tree under the manifest, stops pointing at a physical run. Kill sites are enumerated exhaustively:

- A CoW path copy replaces a page (old page killed, birth from its BP).
- A `REG` or `PREALLOC` extent is trimmed, split, truncated, punched, overwritten or deleted (released sub-run killed, birth from the extent).
- Checksum, free, dead-list and attribute tree pages replaced by path copy.

Each kill carries `(start, blocks, birth)`.

### 9.2 Immediate rule

Let `S_newest` be the generation of the newest snapshot, including one pinned in this transaction. For a kill in generation `g` with birth `b`:

| Condition | Result |
| --- | --- |
| `b == g` | Never published. **Ready** immediately (the F0.R1 rule). |
| `S_newest < b < g`, or no snapshots | Referenced only by the previous live tree, which the fallback root still holds. **Pending**. |
| `b <= S_newest` | The newest snapshot may reference it. Append to that snapshot's **dead list**. |

**Why this is exact:** a snapshot taken at generation `s` references precisely the blocks that were live at `s`: born at or before `s` and not yet killed. A block born after the newest snapshot cannot be in any snapshot. This is how ZFS frees blocks without global reference counts.

### 9.3 Dead lists

```text
key   = snapshot_gen: u64 BE | birth: u64 BE | start: u64 BE
value = blocks: u32
```

`D(s)` is the set of runs with birth at or before `s` that were killed after `s` and before the next **surviving** newer snapshot was taken (or are being killed now, if `s` is the newest). Every run in `D(s)` is referenced by snapshot `s`: it was live at `s` and killed after. "Surviving" matters: when a newer snapshot is dropped, its interval merges into `s`'s (§9.4).

Taking a snapshot is O(1): add a catalog entry; the new snapshot starts with an empty dead list.

### 9.4 Dropping a snapshot

Let `s` be the snapshot being dropped, `p` the next older snapshot (if any), and `n` the next newer snapshot (if any).

Two sets of runs are affected:

**Runs in `D(s)`** (killed while `s` was newest, birth `<= s`):

- `birth > p` (or no `p`): not in `p` (born after it), not in `n` or newer (killed before they existed), not live. **Free** (pending, by the two-root rule).
- `birth <= p`: still referenced by `p`. **Move** to `D(p)`. This matches `D(p)`'s definition, because with `s` gone, `s`'s interval is now part of `p`'s.

Because dead-list keys sort by birth within a snapshot, this split is a range operation: the `birth <= p` prefix moves as a whole.

**Runs in `D(n)`** (killed while `n` was newest, birth `<= n`). These may also have been referenced by `s`, but `n` still references all of them, so nothing in `D(n)` can be freed by dropping `s`. Its meaning is unchanged, since its condition `birth <= n` does not mention `s`.

So dropping a snapshot costs O(|D(s)|), not O(volume).

**Proof obligation for the implementation PR:** state and test the invariant "every physical run is exactly one of: reachable from the live tree or a manifest tree; in exactly one dead list; in the free tree; in the pending set; above the high-water mark", and that the immediate rule and drop rule preserve it.

### 9.5 Bounded processing

A snapshot drop can free millions of runs. Dropping marks the snapshot `dropping` in the catalog and moves its dead-list processing into the **deferred** class. Each commit (or an explicit `reclaim_step()`) processes at most a fixed budget of dead-list entries and persists the sweep cursor in the manifest. Interruption resumes from the last published cursor.

**Why:** this reuses the F2.R4 bounded-sweep idea and its crash argument unchanged, applied to dead lists instead of a per-block queue.

### 9.6 The trust shift, stated plainly

v9 authorizes reuse with an independent full two-root ownership scan. F5.3 authorizes reuse by **accounting**: correct kill events plus the rules above. That is how ZFS and btrfs work, and it is what makes commits O(changes). It is also a real change in what the code trusts.

Mitigations:

- `reuse_verification = Full | Accounted`, recorded per handle, not on disk.
  - **Full:** before any freed run becomes ready, check it against an independent full ownership scan (current behavior). Default in tests.
  - **Accounted:** trust the accounting. Default for benchmarks and, eventually, mounts.
- `count_roots` and the F2.R2 reference reconstruction are kept as the **oracle**. In test builds, every commit compares the free tree, dead lists and pending set against it.
- A periodic `scrub()` re-runs the oracle on a live volume and flips the handle to `reuse_blocked` on any disagreement.

**Why the failure modes are asymmetric:** a missed kill leaks space (benign, found by scrub). A double kill or a wrong birth frees live data (catastrophic). The oracle's job in tests is mostly to hunt the second kind; fuzzing should bias toward overwrite-after-snapshot, split-of-split and drop-middle-snapshot sequences.

### 9.7 What retires

- Per-commit `count_roots` traversal: becomes oracle-only.
- F2.R2 CoW physical-reference B+tree: removed for snapshots; a much smaller shared-extent table replaces it only if F5.6 is enabled.
- F2.R3/R4 rewritten sorted-run journal: replaced by the free tree plus dead lists.

---

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

Clone (`FICLONERANGE`-style) lets two inodes reference the same physical run. It breaks the assumption in §9.2 that the live tree references each block at most once, so it needs a small **share table**:

```text
key   = start: u64 BE
value = blocks: u32, extra_refs: u32
```

Cloning sets `MAY_BE_SHARED` on both the source and destination extent items and increments `extra_refs`. A kill of a run with `MAY_BE_SHARED` set consults the table: if `extra_refs > 0`, decrement and stop; otherwise apply §9.2. Kills of runs without the flag skip the table.

**Why the flag bit:** OpenZFS block cloning has no such bit, so every free must check its Block Reference Table. A new format can add it cheaply.

**Why off by default:** OpenZFS 2.2.0 shipped block cloning enabled and users hit silent corruption; 2.2.1 disabled it and 2.2.2 fixed it. Sharing interacts with every kill site. It ships only after the oracle covers clone, overwrite-of-clone, snapshot-of-clone and drop-of-clone-source under fault injection.

---

## 13. Commit ordering

Unchanged from F0.R1 in its barriers:

1. Apply the transaction to an in-memory delta of the fs, checksum and attribute trees.
2. Allocate data runs (§8.4); reserve tree-update blocks (§8.6).
3. Path-copy all trees; record kill events; apply §9.2; update free tree and dead lists; process at most one bounded dead-list budget (§9.5).
4. Return unused reservations to ready.
5. In test builds, run the oracle against the staged state.
6. Write all staged pages and data blocks not already spilled. **Sync.**
7. Write manifest v10 into the next ring slot. **Sync.**
8. Write the alternating superblock. **Sync.**
9. Install the new in-handle state only after step 8 succeeds; any failure poisons the handle.

Spilled data blocks (§7.4) may be written before step 6; the step-6 sync covers them.

**Not in F5, but unblocked by it:** group commit (several transactions per generation, with `fsync` meaning "wait for the next published generation"). This belongs in the mount milestone. It matters for throughput and for how fast generations are consumed.

## 14. Decision log

| Decision | Chosen | Rejected | Reasoning |
| --- | --- | --- | --- |
| Generation width | `u64` everywhere | `u32` birth in data blocks | Volume-lifetime ceiling reachable in weeks at high commit rates |
| Data integrity location | Parent-held: checksum tree plus birth in extent | In-band 32-byte header | Header breaks alignment and sharing; parent checksums still catch lost and misdirected writes |
| Metadata integrity | Parent BP checksum **and** self header with own address | Either alone | Parent catches stale pages; header lets orphan scans and repair recognize pages |
| Checksum storage | Separate tree keyed by physical address | Inline in extent; keyed by file offset | Fixed-size extents; free sharing on clone; physical-order scrub |
| Checksum granularity | Per 4 KiB | Per extent; whole file | Partial reads verifiable; ~0.1% overhead |
| Page format | One generic slotted 4 KiB page | Per-tree formats; 16 KiB nodes | One codec to fuzz; small CoW units; larger nodes need measurement first |
| Extent sub-addressing | Trim physical run directly | btrfs inner offset | Block-granular uncompressed data is cuttable; avoids bookend waste |
| Max extent | 32 MiB | Unbounded | Bounds split and lookup work |
| Small files | Inline at or below 2,048 bytes | Always a data block | Corpora are small-file heavy; zero extra I/O |
| Allocation timing | Delayed to commit, with spill | Allocate on write | Better contiguity; bounded memory via spill |
| Free-space index | One durable tree by start plus in-memory size index | Two durable trees; bitmaps; log space maps | Single source of truth; epochs per run; log maps kept as measured fallback |
| Fragmented allocation | Split across several extents | Require one contiguous run | Fragmentation should cost speed, not correctness |
| Never-written space | Keep high-water mark | Fold into free tree | Preserves fail-closed allocation when ownership is unverifiable |
| ENOSPC handling | Metadata reserve | None | CoW needs space to free space |
| Snapshot reclamation | Birth time plus per-snapshot dead lists | Per-commit full reference count | O(changes) commits; exact rule; proven ZFS approach |
| Reuse authority | Accounting, with Full mode and oracle | Full scan on every reuse | Full scan is O(volume); keep it as referee and opt-in mode |
| Bulk frees | Bounded deferred processing with persisted cursor | Free everything in one commit | Reuses the F2.R4 crash argument; bounded commit latency |
| Namespace keys | Inode table plus `(parent, name)` | Full paths | O(1) rename, hard links, short keys |
| Directory index | Sorted names | Hashed names (plus second index) | One index, no collisions, ordered readdir |
| Mount verification | Lazy, through BPs | Full read at open | Mount time independent of data size |
| Attribute catalog | Two CoW trees | One rewritten file | Removes 256 KiB cap and whole-index rewrites |
| Sharing | Optional share table plus flag bit | Always-on; no flag | ZFS 2.2.0 history; flag avoids lookups on unshared frees |
| Back-references | None | btrfs-style backrefs | Not needed for freeing; costly write amplification; offline walk can report affected files |
| Format transition | Single v10 break | Several successive versions | Policy already rejects old images; avoid fixture churn |

## 15. Validation plan and acceptance gates

The testing approach follows the one AWS used for ShardStore: an executable reference model per component, property-based tests comparing model and implementation, crash tests checking that persisted writes survive, and stateless model checking for any concurrency added later.

### Reference models

- **File model:** `BTreeMap<Ino, SparseBytes>` plus a directory map. Holes read as zeros.
- **Space model:** the existing full two-root ownership scan (`count_roots`, `allocation_report`).
- **Attribute model:** existing M0 oracle.

### Gates

| Milestone | Gate |
| --- | --- |
| F5.0 | Commits continue past generation `2^32` (test-only format option to start near it). Flipping any single byte in any data block is detected on read. A `CrashDevice` mode that acknowledges a write but drops it (lost write) never returns stale data. Writing file A's block contents to file B's address is detected. Open reads O(tree height) pages, asserted by a page-read counter. |
| F5.1 | Random `pwrite` / `truncate` / `punch_hole` / `fallocate` / append sequences match the file model after every commit and after every injected crash point (either the pre- or post-commit model state, never a mix). Extent invariants in §7.6 hold after every commit. Inline ↔ extent transitions covered at 2,047 / 2,048 / 2,049 bytes. |
| F5.2 | Free tree plus pending plus dead lists plus live ownership equals the oracle after every commit. Fragmentation test: fill the image with small files, delete every other one, then write a file larger than any free run; it must succeed if total free space minus reserve suffices. Deletes still succeed on a volume filled to the reserve. |
| F5.3 | Oracle agreement under random snapshot pin / drop (including middle snapshots), overwrite-after-snapshot and crash injection during bounded dead-list processing. No run is ever both owned and ready. Commit metadata reads grow no faster than O(log n) between 10³, 10⁴ and 10⁵ files (flag if the 10⁵ cost exceeds 3× the 10³ cost per changed file). |
| F5.4 | Rename, hard link, unlink-with-links and rename-into-self rejection match the model. Renaming a directory with 10⁴ descendants writes O(height) pages. |
| F5.5 | Existing F4 query tests pass; posting oracle agrees after every commit; a single attribute change rewrites O(height) pages, not the catalog. |
| F5.6 | Clone, overwrite-of-clone, snapshot-of-clone and drop-of-clone-source under fault injection agree with the oracle. Remains off by default until a separate review. |

### Benchmarks (on disposable images, reported per op class)

Physical blocks written per logical byte (write amplification) and pages read for: small-file create, 4 KiB random overwrite in a 1 GiB file, sequential append, delete, snapshot then overwrite, snapshot drop. Compare with v9 where v9 can express the operation. Per ARCHITECTURE §1.6, these are image-file measurements, not device latency claims.

## 16. Risks and open questions

1. **Kill-site completeness** is the largest risk (§9.6). Every new code path that drops a pointer must emit a kill. Consider a type-level guard: make "drop a BP or extent" impossible without producing a kill record.
2. **Reservation estimate in §8.6** must be an upper bound. If it is exceeded, the commit must fail cleanly (and retry with a larger reservation), never allocate outside it.
3. **Checksum tree as a hot spot:** every data write updates it. Measure; batching per run should keep it to one leaf per contiguous write.
4. **Lazy verification semantics** (§6.6) change what recovery promises. Document it in user-facing terms before any mount ships.
5. **Directory scaling:** sorted names in one tree are fine for single-writer use; very large directories under concurrent writers may want hashed placement later.
6. **mtime updates** dirty the inode leaf on every write. Usually the same leaf as the extents; measure before optimizing.
7. **Open question:** should `PREALLOC` extents count against the metadata reserve rules or ordinary space? Proposed: ordinary space.
8. **Open question:** default dead-list processing budget per commit. Proposed starting point: 256 runs, tuned by benchmark.

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
