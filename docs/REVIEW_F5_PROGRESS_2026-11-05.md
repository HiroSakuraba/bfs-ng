# F5 progress review — 2026-11-05

**Scope:** an outside review of the F5 / v10 work merged between [PR #18](https://github.com/HiroSakuraba/bfs-ng/pull/18) (F5 design proposal, 2026-11-05 00:14 UTC) and [PR #33](https://github.com/HiroSakuraba/bfs-ng/pull/33) (`9a2648b`, 14:35 UTC). Open [PR #34](https://github.com/HiroSakuraba/bfs-ng/pull/34) is discussed but not measured.

**Status of this document:** review notes and recommendations, not a format contract. Source code, tests and the milestone specs remain the authority. Measurements are reproducible with [`crates/storage/examples/v10_commit_io.rs`](../crates/storage/examples/v10_commit_io.rs).

## 1. Summary

About 13 hours of work produced 17 merged PRs and 192 commits. v10 now exists as a working experimental filesystem alongside v9: headerless data blocks, a checksum tree, sparse extents with partial writes, snapshots, F4 attributes, certified block reuse, and an opt-in persistent free-space tree. All storage tests pass on `9a2648b`.

The fail-closed safety model has been kept everywhere, which is the most important thing to get right first.

The main finding: **v10 currently reads about three times as many blocks per commit as v9**, and that cost grows linearly with the number of files. Every commit re-runs a full audit of both F0 root graphs. The safety work is ahead of the performance work. That is the right order, but the next phase needs to bring per-commit cost back down before more features land on top.

## 2. Corrections to the original F5 proposal

[PR #19](https://github.com/HiroSakuraba/bfs-ng/pull/19) fixed real errors in the PR #18 proposal. They are recorded here so the reasoning is not lost.

- **The partition invariant was unsound.** The proposal claimed every physical run is in exactly one of: live, one dead list, free, pending, or never-written. By the proposal's own definition, a run in dead list `D(s)` is still referenced by snapshot `s`, so it is in two places at once. The corrected model allows dead-list witnesses to overlap snapshot-referenced blocks.
- **The snapshot birth rule was applied to volume-global trees.** Checksum, free, dead-list and snapshot-catalog pages belong to the whole volume, not to any snapshot. Putting them on per-snapshot dead lists was wrong (at best it over-retains, and it confuses the ownership argument). They now retire through F0 fallback fencing. PR #33 later fixed the same class of issue for snapshot-catalog pages in the implementation.
- **The reservation estimate was a guess.** The proposal reserved "changed leaves × (height + 1) × 2, plus margin" blocks for tree updates. That is not a proof of termination. It was replaced with a private metadata arena with an explicit worst-case bound and atomic abort.
- **The fixed 256-block reserve floor** did not suit tiny test images. Reserve sizing is now geometry-aware.

## 3. Status against the F5 milestones

| Milestone | Done | Not yet |
| --- | --- | --- |
| F5.0 format foundation | 64-bit generations, block pointers with parent-held CRC, generic page header, headerless data blocks, checksum B+tree | **Lazy open.** Open still reads about 5 blocks per file (§4) |
| F5.1 extents | Sparse extents, byte-range `write_at` / `read_at`, truncate, localized checksum CoW | Capped at 64 blocks (256 KiB) per file in a single-level map. No inline files, `punch_hole`, `fallocate` or streaming/spill |
| F5.2 free space | Opt-in persistent free tree with absolute eligibility epochs; 64-block private metadata arena | Fragmentation endurance test; arena sized independently of volume |
| F5.3 retirement | Persisted dead lists and bounded retirement cursor, both published atomically through F0 | Accounted mode intentionally disabled; full two-root audit still runs on every commit |
| F5.4 namespace | Rename preserving stable inodes | Still keyed by full path (240 bytes, 2,048 records); namespace tree rebuilt in full on every write (see [I2](INTEGRATION_I2_V10_NAMESPACE.md)) |
| F5.5 attributes | Schema, rows and postings in three separate authenticated trees; historical roots kept in snapshot descriptors | Each changed section is rebuilt in full and bounded at 256 KiB (documented in [I4](INTEGRATION_I4_V10_ATTRIBUTES.md)) |
| F5.6 clone | — | Not started, which is correct until the oracle can cover partial-interval sharing |

## 4. Measurements

**Method.** A 64 MiB `CrashDevice` image (16,384 blocks) is filled with 1,000-byte files in one directory. At each fill level the image is reopened, then the harness makes 5 one-byte overwrites of a single existing file and counts physical block reads and writes per commit. v9 has no byte-range write, so its row rewrites the whole 1,000-byte file each time. Open reads are counted separately.

Block counts are the meaningful metric. Wall-clock time is dominated by the `CrashDevice` durability model and is **not** a storage latency claim (ARCHITECTURE §1.6).

Measured on `9a2648b`:

| Files | v10 reads/commit | v10 indexed reads/commit | v9 reads/commit | v10 writes/commit | v10 indexed writes/commit | v9 writes/commit | v10 open reads | v9 open reads |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 10 | 91 | 98 | 32 | 6 | 7 | 6 | 49 | 27 |
| 100 | 800 | 807 | 315 | 8 | 9 | 8 | 413 | 243 |
| 300 | 2,936 | 2,943 | 938 | 13 | 15 | 8 | 1,549 | 723 |

**Reading the table**

- **Writes are good.** Copy-on-write stays local: a handful of blocks per commit, growing slowly with tree height. This is the payoff of the F5.0 / F5.1 page and extent design.
- **Reads are the bottleneck.** A one-byte v10 write reads about 8–10 blocks per file already in the volume, roughly three times v9 (2,936 vs 938 at 300 files). The cause is the `audit()` call at the top of `UnifiedV10::commit`, which walks both F0 root graphs on every commit.
- **Indexed mode does not reduce reads yet.** As its docs state, the full two-root audit remains mandatory when the persistent free tree is enabled, so the free tree currently adds bookkeeping without removing the scan.
- **Open is linear too.** Lazy open (F5 §6.6) is not implemented.
- **Filling is quadratic.** Each create also pays the audit, so filling a v10 image to 1,000 files did not finish within 8 minutes in this harness, while v9 reached it in seconds on the same simulated device.

## 5. What still blocks real-world use

In recommended order:

1. **A full two-root audit on every commit.** v9's F2.R4 already certifies the free pool once per open and advances the certificate after each successful publication. v10 currently does not, which is a regression. PR #34 adds an in-handle certificate, but only for an exclusive private `CrashDevice`. The same argument holds for a `FileDevice` opened under an exclusive OS file lock: one writer, one handle, certificate rebuilt by a full audit on every open. Moving the full audit to open time plus periodic scrub should make reads per commit roughly flat as the volume grows.
2. **Every call is its own commit.** Each `write_at`, `mkdir` or `rename` costs three syncs plus an audit. v10 needs multi-edit transactions (as v9's `FsTransaction` has), and later group commit.
3. **The 256 KiB file cap.** Replace the single-level 64-block extent map with a multilevel extent tree, and add streaming writes with spill so transaction memory is bounded.
4. **Whole-namespace rebuild per write.** Use localized path-copy first, then move to `(parent inode, name)` keys (F5 §10).
5. **Two filesystems in one crate.** v9 (`fs.rs`, about 2,000 lines) and `experimental_v10` (about 5,800 lines; `unified_writer.rs` alone is 1,435) now duplicate most concepts. Decide the point at which v10 becomes the only write path, and start splitting `unified_writer.rs` into commit, allocation and audit modules.
6. **Attribute sections rebuilt whole.** Incremental row and posting path-copy (backlog P1).

## 6. Suggested acceptance gates for the next tranche

- **Flat commit reads.** With the in-handle certificate enabled on a locked `FileDevice`, reads per one-byte overwrite at 1,000 files are within 2× of the value at 10 files. `v10_commit_io` can become a CI check for this.
- **No certificate across recovery.** Any reopen, failed publication or poisoned handle forces a full audit; tests assert the audit counter increments.
- **Differential oracle.** PR #34's cached-vs-full differential workload passes on `FileDevice` as well as `CrashDevice`.
- **Transactions.** N edits in one transaction publish one generation, with crash points showing all-or-nothing.
- **Large files.** Writing and reading back a 64 MiB file with random 4 KiB overwrites matches a sparse-file model across injected crashes.

## 7. Process notes

- **Duplicate commits.** History has many repeated messages, for example "Apply exact Rust 1.85 rustfmt output for F5.1 CoW extents/checksum implementation" six times. Running `cargo fmt --all` before each push (or as a pre-commit hook) and squash-merging PRs would keep `main` history readable.
- **Stale status header.** `ARCHITECTURE.md` still says "Updated 2026-11-04" and names v9 as the current format, and §3–5 describe only v9. §2 and §7 are current. A short v10 overview section would help readers.
- **Honest documentation.** The milestone docs consistently state what is not proven or not implemented. That habit has made this review much easier and should continue.

## 8. Reproducing

```sh
cargo run --release -p bfs-ng-storage --example v10_commit_io -- v10 10,100,300
cargo run --release -p bfs-ng-storage --example v10_commit_io -- v10-indexed 10,100,300
cargo run --release -p bfs-ng-storage --example v10_commit_io -- v9 10,100,300
```

Larger fill levels (for example `1000`) work but take several minutes on v10 because of the quadratic fill cost described in §4.
