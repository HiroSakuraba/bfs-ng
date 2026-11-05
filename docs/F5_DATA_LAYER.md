# F5 — extent data layer, birth-time reclamation and inode namespace (proposed)

**Status:** design proposal, **safety-corrected 2026-11-04**. **Nothing in F5 is implemented.** Target format: F0 superblock **v2** (unchanged) and root manifest **v10**. Disposable single-writer test images only; v9 images are rejected, not migrated. Source code and tests remain the authority once any part of F5 lands.

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

The actual binary codec must specify field offsets, length, zero padding and flag values before F5.0a is implemented. `F5.0a` writes and verifies these complete descriptors even while F5.5 is not active; absent attribute roots are zero and a v9-style catalog inside the fs tree is still reachable through `fs_root`.

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

### 8.1 Stable eligibility epochs

The free-space index is a sorted CoW tree keyed by physical run start. **Durable eligibility is absolute**, never re-encoded relative to the latest F0 generation:

```text
key   = start: u64 BE
value = blocks: u64, state: u8, reserved: 7 bytes (zero),
        eligible_after_generation: u64
```

`state = EPOCH` identifies an independently proven, released run. `state = DEFERRED` indicates a retirement candidate still awaiting proof/bounded sweep; it is **never** available for allocation irrespective of the generation. Other states are invalid in v10. A run must be disjoint from all live data/metadata and all root/snapshot ownership before it enters `EPOCH`; a fallback-owned block may be checkpointed as a release candidate with a future epoch but must not be treated as allocatable until that root rotates out.

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
