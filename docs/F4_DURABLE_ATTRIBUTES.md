# F4 — persistent typed attributes and crash-coherent secondary postings

**Status: bounded experimental test-image implementation.** This is not a mountable filesystem or production search service. The F4 manifest magic changes to \`BFSNGM4\0\` (version 4): previously created F2/F3 test images need recreation; there is no migration or rollback protocol.

## Storage model and crash-consistency boundary

F4 stores a **private, checksummed catalog file** at the reserved namespace key \`/.bfs_f4_attrs\`. The catalog is an ordinary immutable F3 file-content extent internally, so **the same F0 root transaction publishes both the namespace/content tree and its attribute catalog**. The catalog contains:
- A versioned ordered schema (numeric attribute IDs, names, types, ASCII case folding, indexed flag and value length bounds).
- Canonical typed maps keyed by **stable inode ID**, not pathname, so rewriting a file preserves its attributes and removing/recreating it cannot inherit stale metadata.
- Independently encoded, sorted posting tuples \`(attribute_id, canonical_key_bytes, inode)\`, with one posting per normalized text-set element. Query operations seek these postings by ordered key.
- An in-file CRC32C and per-block CRC32C via F3's existing content format. This version's magic is \`BFSAT4\0\0\` and its schema/row/posting stream is explicitly bounded and parser-validated.

A schema is installed once using \`FsTransaction::install_schema(Schema)\`, then persists with the first commit. \`FsTransaction::set_attributes(path, values)\` validates and canonicalizes values through M0's \`AttributeDef::normalize\`, then replaces the file's complete attribute map. A normal file overwrite retains the same inode and attributes. Deleting a file removes its attribute rows and postings in the *same atomic transaction*. The protected catalog pathname cannot be accessed, created, modified or removed through ordinary file, directory, stat, or snapshot-file APIs; it is omitted from user-visible directory listings and file counts.

There is **no second root update** and no asynchronous indexer. On every catalog-modifying transaction the complete bounded catalog is serialized and staged as a new immutable F3 extent, and the F3 path-copy engine updates only affected namespace pages. The existing F0 staged-write/sync/root-payload/sync/superblock/sync ordering ensures a recoverable generation contains a matching file and attribute index, under the same device-sync assumptions as F0–F3.

## Search API

\`FileSystem::search_attributes(&[Predicate], limit)\` uses **persisted, sorted secondary postings**, not a scan of file records. Supports:
- Typed equality for bool, signed/unsigned integers, nanosecond timestamps, 32-byte SHA-256 fields and exact or ASCII-folded text.
- Inclusive-lower/exclusive-upper integer and timestamp ranges.
- Prefix search over indexed text fields, using specified case semantics.
- Text-set element membership (e.g., DVE dependency IDs), and conjunction of 1–16 predicates.
- Stable \`(volume_id, inode)\` results, increasing ID order, deterministic result limits and truncation.

\`FileSystem::attribute_scan_oracle\` rebuilds an independent M0 in-memory reference index from persisted *attribute rows* and performs **full record scans without using the posting tree**. The differential test suite compares both query engines.

\`FileSystem::file_attributes(path)\` returns normalized name/value pairs. \`FileSystem::schema()\` exposes the immutable per-volume schema. \`FileSystem::search_snapshot_attributes(generation, ...)\` evaluates the **historical catalog stored under a pinned F2 snapshot tree**, not the live attributes, and returns that historical generation in its results.

Index data is validated before accepting a root: opening an image decodes the catalog, rejects malformed or non-canonical types/keys, recomputes the posting set from rows, requires exact equality to serialized postings, and verifies each attributed inode belongs to a live regular file. This is done for each live candidate and pinned snapshot. If newest generation's catalog is corrupted, recovery can fall back to the previous valid F0 root. F2's allocator automatically protects the catalog extent because it is reachable from the same F3 tree as user files.

## Limits and non-goals

- **One schema per volume, immutable after installation.** No online migration or ACL-filtered query security.
- Max 32 schema fields, 64 attributes per file, 32,768 postings and **256 KiB serialized catalog** (F3's file-size cap). 2,048 total namespace records including the private catalog file. These bounds prevent unbounded parsing and excessive memory use; they are not performance targets.
- Every attribute modification rewrites the entire serialized catalog, and every query uses an in-memory BTreeSet parsed from that catalog. F4 therefore has durable index *semantics* and an atomic publication protocol but **not an incremental on-disk attribute B+tree**. It must not be presented as O(log N) end-to-end persistence.
- The catalog is not exposed to ordinary filesystem clients, but this is not a hostile-process security boundary; there is no OS mount, multiwriter file lock, ACL, encryption or authenticated storage.
- Existing F3 namespace operations still materialize the full namespace into memory, and F2's reachability scan still checks every retained root. Physical hardware power-loss invariants depend on the host storage stack honoring sync barriers.
- Format v4 is intentionally incompatible with older F2/F3 test images; do not downgrade a v4 image into an older writer. No migration from versions 2 or 3 exists.

## Real-world use cases

An AxiomZig source file can carry independent checker-observed status, compiler version, source hash, train/dev/test split and diagnostic labels. AxiomMegaDoc megadocs can use \`corpus.split\`, a verification/trust field and source-record IDs, ensuring its 260 benchmark-only provenance views are **not silently promoted to compiler-verified training documents**. DVE assets can expose \`asset.kind\`, \`asset.depends_on\` (a text set), material category and content hashes, allowing reverse dependency queries.

These are schema examples: F4 does **not** yet bulk-import the real-world corpora or benchmark indexed search on actual mounted images. Those are follow-on workloads, and need an import layer that explicitly preserves generated-data provenance and stable inode associations.

## Verification

\`\`\`sh
cargo test -p bfs-ng-storage --test storage_f4
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
\`\`\`

F4 tests cover durable file-backed typed attribute queries, a full-scan independent oracle, deletion/recreation identity changes, snapshot query isolation, private-path rejection, strict type and indexed-only predicate checks, catalog tampering, crash faults between file and index writes, and randomized differential queries. All preexisting F0–F3 storage tests continue to run.

**Next:** F4.1 would replace the whole-catalog rewrite with page-oriented incrementally updated CoW posting trees, add on-disk key-range cursors and richer query planning, then integrate the AxiomZig/DVE/AxiomMegaDoc real-source benchmark importer. F5 should address authorization-aware results, file-level ACLs, stress/quality fuzzing, and OS mount interfaces.
