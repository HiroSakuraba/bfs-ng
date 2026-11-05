# F5.0a — experimental v10 codec and authenticated root-read foundation

**Implementation status:** isolated format codecs and direct-root reader from PR #20; PR #21 adds authenticated multilevel snapshot traversal and a **non-publishing CoW staging planner**. **The default \`FileSystem\` remains v9**, F0 superblocks remain v2, and all actual v9 commits still use F2.R4's two-root certified allocator. This work **does not implement a v10 filesystem writer, generic namespace tree traversal, user file I/O, physical allocation certification, or durable snapshot-drop reclamation.** The new staging planner constructs pages but does not write or publish them. No production images or migration are supported.

Entry point: \`crates/storage/src/experimental_v10.rs\`. The public module name deliberately signals that these APIs are not a production filesystem.

## 1. Exact byte encodings

All multibyte integer fields are little-endian **except** the snapshot-catalog sorted key, which is the generation in big-endian for lexicographic ordering. Every record rejects noncanonical reserved bits, bad CRC32C, invalid geometry, and inconsistent identities.

### Block pointer — 24 bytes

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | Physical block address, \`u64\` |
| 8 | 8 | Allocation/birth generation, \`u64\` |
| 16 | 4 | CRC32C over the **entire encoded target page**, including its internal CRC |
| 20 | 1 | Target tree level, 0 leaf |
| 21 | 3 | Reserved, zero |

Only an **all-zero** pointer is absent, and only in a field explicitly designated optional. Any nonzero pointer must have address \`>=5\`, birth \`>=1\`, level \`<=16\`, and must be within volume geometry and the maximum allowed generation. A required filesystem root cannot be all-zero. Pointer birth and address must match the authenticated target page's self-identifying header.

### Generic CoW metadata page — 4,096 bytes

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | ASCII \`BFSNGT10\` |
| 8 | 1 | Tree ID |
| 9 | 1 | Level, 0 leaf |
| 10 | 2 | Slotted item count |
| 12 | 4 | Self CRC32C, computed with these four bytes zero |
| 16 | 8 | Page birth generation |
| 24 | 8 | Page's own physical address |
| 32 | 16 | Volume UUID/ID, \`u128\` |
| 48 | 8 each | \`key_off, key_len, value_off, value_len\` as four LE \`u16\` values |
| tail | variable | Packed key/value bytes, from the end of the page downward |

IDs: **1** fs, **2** checksum, **3** free-space, **4** dead-list, **5** attribute rows, **6** attribute postings, **7** snapshot catalog, **8** attribute schema. IDs 7 and 8 extend the original six-tree sketch in [F5 §6.2](F5_DATA_LAYER.md).

Keys must be nonempty, strictly bytewise ascending and nonduplicated. An empty **leaf root** is valid; an empty internal page is not. Branch values must be exactly one 24-byte BP to a child at level \`parent_level - 1\` born no later than the parent, and cannot refer to the parent itself. Every slice and slot is in-bounds; the unused slack is zero. Decode then reencode must produce the **identical** 4,096 bytes, rejecting overlapping/noncanonical slotted layouts even with recomputed CRCs.

The parent-held pointer CRC is distinct from the page's self CRC: it authenticates the **encoded** page, including its own checksum field. Neither CRC proves physical allocation ownership or protects against a malicious CRC collision.

### Manifest v10 — exactly 292 bytes inside F0's checked root payload

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | ASCII \`BFSNGM10\` |
| 8 | 4 | Format version 10 |
| 12 | 4 | Node size 4096 |
| 16 | 8 | Generation, \`u64\`; must equal enclosing F0 root |
| 24 | 8 | Descending virgin high-water boundary |
| 32 | 8 | Next never-reused inode ID |
| 40 | 24 | Filesystem root BP, **required** |
| 64 | 24 | Global checksum-tree root, optional |
| 88 | 24 | Free-tree root, optional |
| 112 | 24 | Dead-list root, optional |
| 136 | 24 | Snapshot-catalog root, optional |
| 160 | 24 | Attribute-row root, optional |
| 184 | 24 | Attribute-posting root, optional |
| 208 | 24 | Immutable attribute-schema root, optional |
| 232 | 8 | Sweep snapshot generation |
| 240 | 8 | Sweep key |
| 248 | 8 | Metadata reserve, in blocks |
| 256 | 8 | Inode count |
| 264 | 8 | Used-block count |
| 272 | 8 | Free-block count |
| 280 | 8 | Pending-block count |
| 288 | 4 | Manifest CRC32C with this field zeroed |

The three attribute roots must either **all exist** or **all be absent**, never inherit the current live roots. Distinct root-tree roles cannot point to the same physical page. Every root BP's address is at or above the published high-water and below the image's total block count; birth cannot exceed the manifest generation.

The high-water, reserves, counters and sweep fields are checked for geometry but are **not currently produced or certified by any v10 allocator**. These fields are declarative test-format metadata, not a free-space claim.

### Complete snapshot descriptor — exactly 112 bytes

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | Snapshot generation, \`u64\` |
| 8 | 24 | Historical fs root BP, **required** |
| 32 | 24 | Historical attribute-row root BP, optional |
| 56 | 24 | Historical attribute-posting root BP, optional |
| 80 | 24 | Historical schema root BP, optional |
| 104 | 4 | State: \`Pinned = 1\`, \`Dropping = 2\` |
| 108 | 4 | Reserved, zero |

The snapshot-catalog leaf maps **\`snapshot_generation.to_be_bytes()\`** to the complete descriptor, with exact key/descriptor generation equality. All four tree roots are verified using their own BPs, not the manifest's live attribute roots.

The current prototype **reads multilevel snapshot catalogs**, verifies every parent high-key separator against the actual maximum descendant key, checks sibling key intervals, prevents repeated physical page addresses, and enforces a bounded maximum of 8,192 pages / 131,072 descriptors. It verifies historical fs/attribute roots on request. A pure CoW planner can stage pin insertions and pinned-to-dropping transitions from caller-supplied pages, including leaf and root splits; **only the disposable known-blank test harness publishes those pages through F0.** A safe production v10 updater still requires independently certified allocation and journal/dead-list retirement.

## 2. Read-only F0 integration and safety boundary

\`ReadOnlyV10::open(device)\` invokes F0's existing \`Volume::open_verified\`. For each candidate F0 root it validates the complete v10 manifest and **direct root pages** of its referenced trees. It falls back to an older recoverable F0 candidate if the newer manifest/direct root is invalid. It does **not** validate every namespace child, data block, snapshot item or orphan address on open.

\`ReadOnlyV10::snapshot_descriptors()\` additionally traverses **all** bounded snapshot-catalog levels and reads each descriptor's historical root pages with its own parent checksum, generation and volume identity. A deep corruption detected during this later operation is a read error; it does **not** secretly roll the volume back.

**Crucial:** nothing in \`experimental_v10\` returns a certified free-run set, changes a physical block, publishes a v10 transaction, or authorizes allocation. The pure \`plan_snapshot_edit\` function returns bytes for *caller-verified virgin reservations* without writing them; the caller must separately prove allocation safety and use correct F0 staged-page publication. A dropping pin is **retained**, never actually removed/freed. Lazy root validation must **never** be mistaken for F2.R4's full ownership scrub.

## 3. Reproducible tests

\`cargo test -p bfs-ng-storage --test storage_f5_0a\` covers:

- Canonical slotted pages, strict branch child-level/birth checks, empty leaf roots, forged self-valid pages with mismatched parent checksum, inconsistent address/volume/tree/level/generation, noncanonical slot layouts and zero-reserved fields.
- Stable, fixed-width manifest and descriptor round trips **above \`u32::MAX\`**, rejection of wrong F0 generation, old manifest magic, missing required root and partial attribute-root groups.
- Real F0 v2 ring publication across g1–g7 while the g1 attribute/schema tree pages remain reachable through a pinned historical descriptor; rejection of corrupt historical metadata on the later snapshot read.
- Corrupt latest metadata-root fallback to the earlier complete manifest; every F0 payload/superblock write/sync failure point, with torn-write prefixes.
- A **test-only synthetic, checksum-valid pair** of consecutive F0 roots near the \`u32\` boundary, followed by a real F0 commit at generation \`u32::MAX + 1\` and a v10 read-only reopen. This does **not** require 4.3 billion runtime commits.
- Normal v9 \`FileSystem\` commits, reopen, and explicit rejection by the v10 reader.

The full workspace CI, Clippy and rustfmt checks are still required.

## 4. PR #21: authenticated multilevel snapshot catalog and CoW plan

The new `experimental_v10::read_snapshot_tree` reads the whole catalog, not only its root. Each branch's key is the **exact greatest descendant key** in the child, siblings have strictly disjoint sorted intervals, and a physical metadata page cannot occur twice in one catalog. Depth cannot exceed the 16-level v10 codec bound, and reads are bounded by the page/entry budgets. A corrupt sibling, forged separator, duplicate page pointer, malformed descriptor or historical root pointer cannot be silently skipped.

`plan_snapshot_edit(device, previous_root, edit, volume_id, next_generation, previous_high_water, reservations)` returns `StagedCatalog { root, staged_pages, high_water, consumed_reservations }`. It **does not write or sync** those pages. Insertions and the `Pinned → Dropping` transition path-copy the touched leaf and ancestors, splitting packed nodes and creating a new root when necessary. A duplicate snapshot fails rather than altering the pinned descriptor; `Dropping` still retains the full historical pointer set.

**Safety contract:** reservation addresses must be strictly descending below the previously published high-water mark and above F0's reserved blocks 0–4. These checks do *not* establish physical virginity. The external caller must independently certify the addresses and the old catalog before authorizing a publication. No code in this module ever frees a pinned block; completing deletion remains an F5.3 proof obligation. It is not yet a mountable, usable or general-purpose v10 filesystem.

**Tests:** `cargo test -p bfs-ng-storage --test storage_f5_0a_snapshot_tree` exercises 144 snapshots across several leaf/root splits and F0 ring rotations, bounded staged CoW writes, duplicate pin rejection, dropped-state retention, insufficient staging pages, invalid reservation geometry, deep corrupt leaves, intentionally incorrect parent separators and a synthetic 3-level catalog.

## 4. Deliberately deferred work / next tranche

This is the **F5.0a codec + direct-root validation tranche** only. Completing F5.0a still requires independently safe durable snapshot-catalog publication, generic CoW B+tree support beyond snapshots and fully specified startup/format gating, all retaining v9's conservative allocator until the later v10 safety proofs. F5.0b adds headerless data and a global checksum tree covering every live and pinned historical data block. F5.2/F5.3 add safe free-run accounting only after a separate adversarial ownership proof.

References: [F5 design and safety corrections](F5_DATA_LAYER.md), [F0 root ring](F0_R1_ROTATING_ROOTS.md), [F2.R4 certified allocation](F2_R4_CERTIFIED_RECLAMATION.md), [architecture](ARCHITECTURE.md).
