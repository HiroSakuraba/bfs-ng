# BFS-NG — architecture, implementation status, and decision log

**Updated:** 2026-11-04 · **Baseline:** `main` after F0.R1, [PR #16](https://github.com/HiroSakuraba/bfs-ng/pull/16) (`83e13c9`) · **Current experimental storage format:** F0 superblock **v2**, F2/F3/F4 root manifest **v9**.

**Status:** BFS-NG is an experimental Rust research workspace with a working **single-writer, regular-file-backed test-image storage engine**, transactional CoW namespace, snapshot pins, typed searchable attributes, a verified reference tree, and conservative reclamation. **It is not a mountable/production filesystem and must not store irreplaceable data.** The cache-policy simulators, independent metadata cache, and real-world trace generators are research components, not an OS-integrated filesystem I/O path.

This is the **current cross-cutting design and status document**. Format-specific invariants live in the linked milestone specifications; source code and tests remain the authority if a historical specification describes an older image version.

## 1. Design commitments

1. **Recoverability before reuse.** Every candidate physical allocation must remain unreferenced by *both* recoverable F0 roots and their pinned F2 snapshot graphs. A damaged, checksummed candidate of unknown ownership disables physical reuse; a new commit may use independently virgin space where available.
2. **Copy-on-write and one publication point.** Data, namespace changes, reference accounting, free-space checkpoint, and indexed attributes are staged as immutable pages, synced, and made visible by one F0 root-slot publication. An error during publication poisons the handle until recovery.
3. **Stable typed identity.** Paths locate records, but inode identities and schema-normalized attribute keys define searchable metadata; deleted-and-recreated files cannot inherit stale postings.
4. **Independent verification.** CRC32C, bounds checks, birth generations, key ordering, parent separators, snapshot roots, and journal epochs are checked when reading. Reference counts and indexed postings have independently reconstructed oracles; a checksum is **not** an authorization to recycle an address.
5. **Portable Rust core.** Keep the storage model independent of potential Linux FUSE and Windows WinFsp adapters. The project currently implements neither mount.
6. **Performance claims require measured work.** Physical metadata writes, full-tree traversals, cache-simulator misses, actual read I/O, and p50/p99 timing are different metrics. The external-corpus harness is not a measurement of OS page-cache hits or physical drive latency.
7. **Explicit experimental formats.** Old test-image formats are **not** silently opened under newer rules. Format migration, production locking, and power-cut qualification remain future work.

## 2. What is implemented today

| Area | Actual implementation | Important limitation / status |
| --- | --- | --- |
| C1–C3 cache research | Reproducible trace generator/replay; byte-capacity no-cache/LRU, S3-FIFO-inspired, W-TinyLFU-inspired policies; model miss costs and reports | A model of residency, not kernel I/O; cache policies are research variants |
| C4 independent policy checks | libCacheSim comparison with exact LRU control and documented S3-FIFO/TinyLFU deltas | Variant disagreement is documented, not claimed as a proof of exact upstream equivalence |
| C5 safe metadata cache | Standalone read-only cache keyed by volume, physical block and block-allocation generation; immutable handles and checked block admission | Not wired into F2 storage or its durable snapshot allocator; no write-back |
| M0 attributes | In-memory typed schema, canonical values, posting queries and differential full-scan oracle | Not itself persistent; F4 provides a separate bounded durable implementation |
| F0.R1 root publication | Alternating CRC32C superblocks and **three reusable root-payload blocks** with ordered barriers | F0 superblock v2; single writer; simulated crash tests, not real-device power-cut certification |
| F1 namespace | Immutable, CRC-checked file data and sorted multi-level namespace records, directories and transaction API | Bounded image and data limits; no mounted namespace |
| F2 snapshot retention | Persistent generation pins and historical reads; both F0 candidates protect referenced data | Bounded snapshot catalog; no live online fsck or compactor |
| F2.R1 / R2 | Durable physical reference witness, subsequently an incrementally path-copied **CoW reference B+tree** | Physical reference counts for new generations still reconstructed from the complete live/pinned graph |
| F2.R3 / R4 | Checksummed free-run journal, ready/pending/deferred epochs, durable bounded sweep cursor, once-per-open allocator certificate, explicit `reclaim_step()` | Journal remains a fully rewritten sorted page chain; no background worker; not entirely scan-free |
| F0.R1 staging correction | Never-published intermediate CoW namespace/reference pages returned to the certified ready pool before the journal is sealed | Separate from published-root retirement: these pages never require an F0 fallback rotation |
| F3 namespace updater | Path-copy changed nodes; page splits; bounded localized sibling redistribution/merges on affected deletion paths; disk range scans with generation cursors | Transactions still clone/materialize a namespace map; no end-to-end logarithmic bound |
| F4 durable searchable attributes | Private CoW catalog committed with the F3 root, typed stable-inode attributes and ordered secondary postings; historical snapshot queries | Catalog/postings are bounded and rewritten as a whole on changes; not an incremental on-disk posting tree |
| Real-world cache workload suite | Deterministic AxiomZig, DVE asset/dependency and AxiomMegaDoc corpus/trace generators, recorded bounded reads and pinned smoke workflows | Model/record traces are **not** traces from a BFS-NG mount or proof of hardware performance |

**Historical milestone docs are not current disk-format contracts.** F2.R1 introduced v5, F2.R2 v6, F2.R3 v7, F2.R4 v8, and F0.R1 advances the **current root manifest to v9**. The current free-run pages still use their checked `BFSFRJ8\0` encoding; current reference B+tree pages still use their v6 leaf/branch encodings. Do not infer page-format version from the manifest version.

## 3. Storage image and root publication (current v2/v9)

A formatted image uses fixed 4096-byte physical blocks:

| Physical blocks | Ownership |
| --- | --- |
| `0` | F0 superblock A, even generations |
| `1` | F0 superblock B, odd generations |
| `2–4` | Permanently reserved **three-slot root-payload ring** |
| `5…N−1` | F1/F2/F3/F4 data, namespace pages, CoW reference pages, immutable free-run journal pages |

For generation `g >= 1`:

```text
root_payload_block(g) = 2 + ((g - 1) mod 3)
```

Thus g1–g6 use blocks `2, 3, 4, 2, 3, 4`. The two recoverable candidate generations `g` and `g−1` occupy **different** ring blocks; publication of `g+1` overwrites only the third block, previously owned by `g−2`, which is no longer referenced by either F0 superblock. This removes the prior `generation + 1` low-end payload leak and its eventual collision with the descending high-region cursor. It **does not** make data/metadata space infinite.

F0's superblock retains the `BFSNGF0\0` magic but has version **2**, generation parity, deterministic payload address, root CRC, volume identity and geometry checks. The F2/F3/F4 root manifest uses `BFSNGM9\0` / format **9**; earlier experimental images require recreation. F1 data and metadata pages must occupy physical blocks **strictly above 4**, independent of their birth generation.

### Ordered root transaction

```text
begin transaction from live generation g
  -> reserve verified free blocks or virgin high-region blocks >= 5
  -> stage changed data + namespace + F4 attribute catalog
  -> independently reconstruct live/pinned reference map
  -> path-copy durable reference pages and build reclamation checkpoint
  -> validate staged references, journal and non-overlapping ownership
  -> write staged immutable blocks; sync
  -> write complete new payload to ring slot for g+1; sync
  -> write alternating superblock for g+1; sync
  -> install new in-handle certificate only after successful publication
```

A failure after a potentially effective write **poisons** the handle. Recovery selects the newest complete candidate, or the previous intact root when the latest cannot be verified. Torn-write tests admit either whole committed generation where appropriate, **never a partially accepted file tree or journal**. Correctness depends on the backing storage honoring durability barriers and not corrupting unrelated previously synced blocks. `CrashDevice` is a deterministic model, not a real SSD power-loss qualification.

**Endurance evidence:** F0.R1 tests publish 240 payload generations using a five-block F0-only image and exercise a 440-generation F2 filesystem workload in a 384-block disposable image with repeated reopen, scrub, checksum and crash fault injection. This demonstrates the fixed root ring and repairs a previously uncovered never-published-page accounting leak; it is not a production lifetime or latency claim.

For the complete formal reasoning and v2/v9 compatibility constraints see [F0.R1 rotating roots](F0_R1_ROTATING_ROOTS.md) and the [historical F0 v1 contract](F0_STORAGE.md).

## 4. Reachability, physical references, and reclamation

The allocator distinguishes **ownership** from **eligibility**.

- **Latest-owned**: physical blocks referenced by the selected F0 generation, including its live namespace, snapshots, reference-index pages and journal pages.
- **Fallback-owned**: blocks protected by the other recoverable F0 candidate; a block held only by this root cannot be allocated yet.
- **Ready**: journal-listed blocks certified absent from both current candidates, eligible for reuse by a future transaction. Never-published, discarded staged pages can be returned here immediately because neither candidate ever referenced them.
- **Pending**: latest root's recorded fallback-only blocks, eligible only after a successful F0 rotation removes that fallback candidate.
- **Deferred**: matured retirements awaiting a bounded, ownership-checked sweep; these remain unavailable to the current allocation transaction.
- **Virgin**: untouched high-region addresses from the descending cursor, above the fixed block-4 reservation.

### Physical-reference index

[F2.R2](F2_R2_INCREMENTAL_REFERENCES.md) replaced the v5 immutable sorted reference chain with a CoW B+tree of checked leaves/branches (`BFSRLF6\0` / `BFSRBR6\0`). Unchanged index pages can be shared, and changed leaf-to-root paths are copied. Counts apply to distinct live/pinned namespace roots **inside one F0 candidate**; reference-index pages and journal pages are owned by the corresponding manifest directly. They must be protected while either F0 candidate can recover them.

The map of live/pinned physical counts is still **recomputed on each new generation** and independently checked against the staged index. Page-local CoW updates do not yet imply end-to-end incremental reference accounting.

### Certified allocator and bounded journal sweep

[F2.R3](F2_R3_RECLAMATION_JOURNAL.md) introduced CRC32C-checked, root-owned free-run journal checkpoints. [F2.R4](F2_R4_CERTIFIED_RECLAMATION.md) added three eligibility classes and a persisted continuation cursor. Its current `BFSFRJ8\0` pages store sorted physical runs with an eligible-generation field: current generation = **ready**, next generation = **pending**, and zero = **deferred**.

On the first mutating commit after `format` or `open`, the implementation independently verifies both F0 candidates, live/pinned trees, index and journal pages, then certifies the free pool for that single-writer handle. Later successful commits advance that certificate **only after root publication**, avoiding a redundant two-root allocator graph scan on every commit. An explicit `allocation_report()` still performs an independent read-only full audit; opening another handle requires recertification.

Each ordinary commit, or explicit `FileSystem::reclaim_step()`, can promote **at most 32** deferred blocks per published generation. The remaining queue and physical `sweep_cursor` are committed with the root manifest; power-loss recovery resumes from the last **completed transaction**, not from partially written per-block progress. Ready blocks cannot include owned pages; pending must match the exact fallback-exclusive set. Unknown/corrupt F0 candidate ownership forces **`reuse_blocked`**, never optimistic allocation.

**F0.R1 accounting correction:** temporary path-copy pages discarded *before publication* are returned to `Builder.reusable` before sealing the journal. Without this, the certified fast path could report `NoSpace` while an independent full scrub found many untracked unowned blocks. This correction does **not** release any block still held by an earlier published root.

**Still expensive:** the full per-generation `count_roots` reconstruction, whole-run journal rewrite, cold-open certificate audit, and orphan-space reconciliation. A bounded sweep is explicitly driven by commits; no persistent background worker or fully incremental CoW free-range tree exists.

## 5. Namespace, snapshots, and indexed metadata

### F1/F3 namespace

The namespace is a CRC-protected sorted, B+tree-style tree. Each leaf carries path keys and bounded file records; branches carry high-key separators and immutable child addresses. New changes copy affected leaves and ancestor paths, preserve unchanged subtree addresses, split full pages and locally redistribute/merge affected underfull deletion pages. This is **not** a blanket occupancy proof across older, unchanged pages or an `O(log n)` bound on complete commits.

`scan_files_disk(expected_generation, prefix, after, limit)` and `scan_snapshot_files_disk(snapshot_generation, prefix, after, limit)` traverse checked disk nodes directly. Live cursors reject a changed generation; historical cursors are scoped to pinned snapshots. In-memory namespace records still exist for transaction construction and some APIs.

F2 snapshot catalogs persist historical generation/tree/cursor identities under the same F0 publication and protect pinned roots from block recycling. The snapshot count, path sizes, file sizes and schema shapes are intentionally bounded in this research implementation.

### M0 / F4 indexed attributes

[M0](M0_SEARCHABLE_ATTRIBUTES.md) is the independent in-memory schema/query oracle. [F4](F4_DURABLE_ATTRIBUTES.md) **does persist** typed attributes and canonical ordered secondary postings in a private `/.bfs_f4_attrs` CoW catalog file, owned by the same published F0 root as the namespace. Values are associated with stable inode identities; file deletion removes its attribute rows and index postings in the same transaction, and snapshot searches use the historical catalog.

F4 exposes `install_schema`, `set_attributes`, `file_attributes`, `search_attributes`, and `search_snapshot_attributes` operations, including typed equality, supported ranges, text prefix, set membership and predicate conjunction. A separate full-scan posting oracle detects mismatches between rows and serialized postings.

**What remains proposed, not implemented:** an independently path-copied on-disk attribute posting B+tree, incremental row/posting updates, query-planner cost model, durable changefeed/live queries, authorization-aware cursors and a mounted search surface. Today the bounded catalog is rebuilt on catalog changes. See [searchable-metadata roadmap](SEARCHABLE_METADATA.md), whose older proposal language must be read in light of current F4 implementation.

## 6. Cache research and realistic workloads

The cache experiments are distinct from the storage allocator:

- **C1–C3:** identical-capacity byte-budgeted cache simulations and reproducible workload replay; miss cost is a trace-supplied model.
- **C4:** compare with libCacheSim reference implementations, requiring LRU agreement as a control. S3-FIFO/TinyLFU deltas reflect model variants, not necessarily bugs. See [C4 comparison](C4_REFERENCE_VALIDATION.md).
- **C5:** generation-safe **read-only** metadata cache admits blocks only after checking physical address, volume, allocation birth generation and CRC. Immutable `Arc` handles and concurrent reads are tested. Cache correctness does **not** authorize premature physical reuse. See [C5 contract](C5_METADATA_CACHE.md).
- **External workloads:** [AxiomZig, DVE asset graph and AxiomMegaDoc](../benchmarks/real-world/README.md) support deterministic synthetic/model traces and bounded source-backed recording with benchmark-owned logical access patterns. CI uses pinned small fixtures; they do **not** measure BFS-NG mount throughput, OS page-cache hits or GPU game-engine frame times.

Any future kernel integration must separately measure logical requests, physical reads and writes, write amplification, actual I/O latency, CPU/LLC/TLB effects and memory-budget overhead. An increased simulator hit rate is not a filesystem speedup until that path is wired and measured.

## 7. Remaining development backlog (proposed order)

These are **proposals**, not completed milestones or a claim of existing branches.

The proposed [F5 data layer](F5_DATA_LAYER.md) (manifest v10, not implemented) groups most P0/P1 storage items into one format break, because the v9 file-data layer (whole-file values of at most 256 KiB, one contiguous run per file, a 32-byte header in every data block, and `u32` birth generations) cannot carry them. The F5 column shows where each item lands.

| Priority | Next milestone | F5 | Proof and acceptance gate |
| --- | --- | --- | --- |
| P0 | **Format v10 foundation** | F5.0 | `u64` generations everywhere; one generic CoW B+tree page format with parent-held block pointers (address, birth, CRC32C); headerless data blocks with a per-4 KiB checksum tree; lazy verification at open. Commits continue past generation `2^32`; single-byte, lost-write and misdirected-write corruption is detected on read; open reads O(tree height) pages |
| P0 | **Extent map, inline files and byte-addressed writes** | F5.1 | Per-inode extents with holes and preallocation, inline files up to 2,048 bytes, `pwrite`/`truncate`/`punch_hole`/`fallocate`, bounded transaction memory via spill. Random operation sequences match a sparse-file reference model after every commit and every injected crash point |
| P0 | **Localized durable free-space structure** | F5.2 | Replace the rewritten sorted-run chain with an extent-keyed CoW free tree carrying ready/pending/deferred epochs; split allocations across runs instead of failing on fragmentation; metadata reserve so deletes succeed on a full volume; retain fail-closed recovery via the never-written high-water region |
| P0 | **Fully incremental physical reference accounting** | F5.3 | Birth-time freeing plus per-snapshot dead lists replace the per-commit `count_roots` walk; `count_roots` remains the independent two-root oracle, compared after every commit in tests under random edits, snapshot pins/drops (including middle snapshots) and fault injection |
| P0 | **Safe orphan reconciliation / crash-resumable scrub** | enabled by F5.0 | Independently verify live and both fallback ownership before admitting crash-unpublished pages; persist bounded checkpoint progress without allowing incomplete marks to free live blocks. F5's self-identifying metadata page header (own address, volume id) is what lets a scan recognize orphans |
| P1 | **Inode table and directory entries** | F5.4 | Replace full-path keys with `(parent inode, name)` entries and an inode table; O(height) directory rename, hard links, short keys and higher fanout; behavior matches a namespace reference model |
| P1 | **Storage end-to-end benchmarks** | §15 of F5 | Measure real commit write amplification, latency and recovery scan cost under AxiomZig/DVE/MegaDoc-like operations on disposable BFS-NG image files, and compare to pre-change baselines |
| P1 | **Incremental typed posting index** | F5.5 | Path-copy only affected attribute rows/postings in their own CoW trees under the same F0 root, removing the 256 KiB catalog file; independently recompute correctness, implement deterministic snapshot-consistent range cursors |
| P1 | **Integrate C5 cache with storage** | — | Prove allocation-generation identity across reuse, invalidation and snapshot retention, impose accounted byte budget, then benchmark against no-cache |
| P2 | **Optional shared extents (clone)** | F5.6 | Small share table plus a per-extent `MAY_BE_SHARED` flag; off by default until the oracle covers clone, overwrite-of-clone, snapshot-of-clone and drop-of-clone-source under fault injection |
| P2 | **Usable mount and production hardening** | — | Specify Linux/Windows semantics, locking and multi-process exclusion, fsync boundaries, migration, device fault testing, online scrub, data/metadata repair policy, permissions and access controls |

**Non-goals at current maturity:** raw-disk use, production fault tolerance, repair of hostile checksummed data, cryptographic integrity, live multi-writer coordination, transparent in-place upgrades, or asserting hardware persistence from `CrashDevice` alone.

## 8. Validation and evidence policy

The merged F0.R1 milestone adds root-ring wraparound tests, checksummed legacy-v1 rejection, latest-root corruption fallback, interrupted write and sync tests, 240 F0-only generations on five blocks and 440 F2 commits on a 384-block test image. F2.R1–R4 test per-root references, pinned snapshots, deferred-free transitions, corrupted journals, forged eligibility fields, repeated reopen and poisoned-handle behavior. Recent merged work passed four workflow families: Rust workspace CI (tests, Clippy, formatting), independent C4 reference comparison, AxiomZig/DVE source-backed smoke, and AxiomMegaDoc generated-data benchmarks.

Use the current repository's checks for any new PR; the suite can evolve. Useful local commands:

```sh
cargo test --workspace --all-targets
cargo test -p bfs-ng-storage
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

A clean test suite establishes the behavior of this **model and test-image prototype**, not sector-atomic SSD durability, complete filesystem semantics, or latency improvements. Record measured evidence and distinctions explicitly.

## 9. Design decisions and superseded alternatives

| Decision | Chosen behavior | Rejected assumption / reason |
| --- | --- | --- |
| Atomicity | CoW staged writes with alternating F0 root publication | Do not mix in a second WAL authority without an explicit recovery protocol |
| Root payloads | Fixed rotating ring over blocks 2–4 (F0 v2) | The old `generation + 1` append-only root path exhausted disk regardless of live capacity |
| Reuse authority | Independent two-F0-root verification on certification; strict published epochs | CRC-valid free-list entries or current-root-only reachability are insufficient |
| Reference accounting | Incremental CoW physical reference **page** updates plus full count reconstruction | Page sharing alone does not prove end-to-end incremental accounting |
| Free-space durability | Journal with ready/pending/deferred and bounded persisted sweep cursor | A completed checksum does not prove early eligibility or authorize partial-sweep reclamation |
| Never-published staged pages | Return discarded addresses to ready pool before journal sealing | Do not misclassify them as previously published fallback-owned retirements |
| Namespace | Bounded path-copying CoW tree and checked disk cursors | Full namespace cloning and reference traversal still affect total transaction cost |
| Search | Durable bounded F4 catalog under same root as files; independent posting oracle | Do not claim a separate search service or incremental on-disk attribute B+tree exists |
| Cache | Explicitly separate policy lab, C5 block cache and actual filesystem I/O | Trace-derived misses and modeled refill costs are not measured drive performance |
| Format compatibility | v9 test images, no v8 migration | Unsafe to decode old append-only root pointers with a new rotating-ring allocator |

## 10. Primary project design references

- [F5 — extent data layer, birth-time reclamation and inode namespace (proposed, v10)](F5_DATA_LAYER.md)
- [F0.R1 — rotating root payload safety and v9 compatibility](F0_R1_ROTATING_ROOTS.md)
- [F2.R2 — CoW reference B+tree and delayed-free fences](F2_R2_INCREMENTAL_REFERENCES.md)
- [F2.R3 — durable retirement journal (historical v7)](F2_R3_RECLAMATION_JOURNAL.md)
- [F2.R4 — certified allocation and bounded sweep (historical v8 base)](F2_R4_CERTIFIED_RECLAMATION.md)
- [F2/F3 — localized deletion repair, cursors and scrub](F2_F3_BACKLOG_CLOSEOUT.md)
- [F3 — CoW namespace update structure](F3_INCREMENTAL_COW.md)
- [F4 — durable typed attributes, constraints and queries](F4_DURABLE_ATTRIBUTES.md)
- [M0 — independent typed-search oracle](M0_SEARCHABLE_ATTRIBUTES.md)
- [C4 — independently compared cache policies](C4_REFERENCE_VALIDATION.md)
- [C5 — checked, generation-safe metadata-cache contract](C5_METADATA_CACHE.md)
- [AxiomZig, DVE and AxiomMegaDoc benchmark harness](../benchmarks/real-world/README.md)

**External influences:** [BeFS design](https://www.haiku-os.org/legacy-docs/practical-file-system-design.pdf), [Haiku BFS](https://github.com/haiku/haiku/tree/master/src/add-ons/kernel/file_systems/bfs), [OpenZFS](https://openzfs.github.io/openzfs-docs/), [XFS delayed logging](https://docs.kernel.org/filesystems/xfs/xfs-delayed-logging-design.html), [SQLite atomic commit](https://www.sqlite.org/atomiccommit.html), [Btrfs CoW design](https://btrfs.readthedocs.io/en/stable/dev/dev-btrfs-design.html), [S3-FIFO](https://s3fifo.com/), [W-TinyLFU / Caffeine](https://github.com/ben-manes/caffeine/wiki/Efficiency), and [libCacheSim](https://github.com/1a1a11a/libCacheSim).
