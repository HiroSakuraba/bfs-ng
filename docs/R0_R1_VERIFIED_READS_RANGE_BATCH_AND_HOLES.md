# R0/R1 partial closeout — verified reads, one-inode batches, sparse holes

**Status:** experimental v10 research implementation, not mountable; no on-disk format change. The established two-F0 full-graph safety audit remains mandatory for ordinary writes. The former 256 KiB single-leaf file cap still applies. This is a subset of [Roadmap R0–R7](ROADMAP_TO_MOUNTABLE.md), not the completion of R0 or R1.

## R0.1: audited immutable read views

`VerifiedReadView<D>::open(device)` performs the existing *complete* independent audit of both recoverable F0 roots at open, then retains the verified selected namespace, snapshot descriptors, data checksums and F4 catalog. `list_dir`, `file_attributes`, `search_attributes` and `snapshots` now require no repeated physical full-tree scans. `stat` validates a per-file authenticated extent root, and `read_at` checks the extent root and CRC32C of every allocated data block actually read. Snapshot reads still follow the historical authenticated namespace root, and historical F4 searches still validate the old attribute roots. A single immutable read view cannot publish any changes.

**Isolation assumption:** there is no image-level OS file locking yet. The caller must guarantee that no other process or handle modifies the underlying image while a read view is active. Read views are **not** an allocation certificate and must never authorize free/reuse decisions. R0 remains incomplete until lazy, height-bounded on-disk namespace lookups and explicit OS read-only isolation are implemented.

Deterministic `CrashDevice` counters test that repeated `stat` + `read_at` + `list_dir` + attribute reads use at most three physical reads per iteration after open, independent of the full graph size. A standard `UnifiedV10` writer still runs the full audit before read calls.

## R1.1: atomic ordered range batches

`UnifiedV10::write_ranges(path, &[(offset, bytes), ...])` applies all nonempty ranges in one logical inode update and **one** F0 generation. Overlaps are legal; later input ranges overwrite earlier bytes. This shares dirty block reads, CoW data staging, extent-root and namespace-root publication, physical checksum deltas, owner-tree changes, Free/Dead accounting and F0 sync barriers. Every range receives an overflow/256 KiB bound check before any write. A batch with no data is a no-op. This reduces F0 sync amplification for multi-part updates to a single file, but **is not a multi-file transaction**, and memory/staging limits are unchanged.

`write_at` reuses `write_ranges` with one range to keep a single implementation of byte overwrite semantics.

## F5.1 partial closeout: sparse hole punching

`UnifiedV10::punch_hole(path, offset, length)` preserves logical file size; fully covered allocated blocks become holes, while partly covered blocks are copied and zeroed. An edge block that becomes entirely zero is also made sparse. The old physical extent stays protected by snapshots, the fallback F0 candidate and delayed retirement. Repeating a punch that makes no change does not publish another generation. Invalid paths and block-bound errors fail closed; a range beyond EOF is clamped to EOF.

These operations work under the existing indexed allocator and under the default full-scan allocator; no allocator authority changes are introduced. Differential byte-buffer tests, durable reopen tests, pinned snapshot checks and torn-publication matrices cover the new write paths.

## Work still open

- R0: lazy read-only open, metadata-height on-disk lookups, pinned generation views and OS-level isolation.
- R1: exclusive FileDevice lock, full multi-inode transactions, journal-neutral batch APIs and flat write-read amplification.
- R2: multilevel extent trees, streaming writes with spill, scalable directory keys, metadata-reserve sizing, 1 GiB file tests.
- R3–R7: POSIX/Windows semantics, mount adapters, fsck/scrub and real hardware fault testing.
