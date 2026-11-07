# R2.1 — Multilevel v10 file extents and chunked streaming (experimental)

**Status:** proposed PR tranche. The v10 unified writer gains a 1 GiB logical file limit (including sparse holes), and a path-copying multilevel extent tree. The legacy standalone `ExtentDataWriter` retains its 64-block/256 KiB single-leaf format and `MAX_FILE_BYTES` bound. All existing single-level v10 file roots remain readable; old readers that expect one leaf reject new internal roots.

## On-disk contract

Each per-inode v10 `TreeId::Fs` root may now point to a checksummed slotted B+tree. Leaves preserve the original key/value schema: big-endian logical block starts, 24-byte physical extents (physical start, count, birth), and precisely one all-ones key recording the u64 logical file size. Internal nodes hold the maximum key of each child and the same 24-byte authenticated parent-child pointer used by other v10 trees. Their separators, sorted nonoverlapping keys, physical nonaliases, page births, volume identity, CRCs, root level, one-size-record rule and data-tail zeroes are independently checked on every full audit. All child and leaf metadata pages are in the protected two-root owner union; snapshot versions of one inode may share pages, but unrelated inodes may not alias metadata.

`file_item_deltas()` compares old canonical leaf records against changed extents. `stage_space_delta(TreeId::Fs)` path-copies only edited leaves and ancestors using already certified free reservations, sharing unchanged immutable child pages. The new extent root, checksums, owner-tree changes, Free/DeadList changes, and namespace pointer all publish through the same F0 data/sync/root sequence. The existing v10 292-byte manifest and F0 publication protocol are unchanged. This is a forward format extension within disposable experimental v10 images, **not** a frozen format or production migration.

## Streaming and memory guarantees

`UnifiedV10::write_from_reader()` reads at most 128 KiB of input at once and publishes each chunk in its own F0 transaction. `UnifiedV10::read_into_writer()` and `VerifiedReadView::read_into_writer()` copy verified 128 KiB windows to an ordinary `Write`. The read-all convenience methods refuse files exceeding 4 MiB rather than allocate gigabyte-sized vectors or silently truncate. A single `read_at` is also capped to 4 MiB. Sparse extents permit 1 GiB *logical* files on small experimental images without allocating zero-filled holes.

**Important limitations:** streaming is **not all-or-nothing across the whole file**; an I/O error leaves a durable committed prefix. Input buffering is bounded but current in-memory allocation and file metadata maps still grow with allocated extent count, and each ordinary commit still audits both F0 root graphs. Thus this does not yet meet the roadmap's strict fixed-total-memory 1 GiB streaming or flat commit read-amplification gates. It is a useful compatibility-safe first stage, not final R2 completion.

## Validation

The new tests cover multilevel split with many sparse extents, physical child-page sharing with snapshots, single-page compatibility, 1 GiB logical sparse size and tail read, repeated 128 KiB streamed writes/reads and reopen, corrupt physical data, torn F0 commits and an injected input-stream error leaving a complete committed prefix. Run workspace compilation, full tests, Clippy and rustfmt, plus the existing C4 and real-world benchmark workflows before merging.

## Remaining R2 work

Separate scalable physical extent-run updates directly on disk to avoid materializing every block, bounded in-memory transaction staging with spill/temporary CoW journal, a complete all-or-nothing streaming transaction, localized namespace and F4 attribute edits, geometry-sized metadata arena and fragmentation endurance.
