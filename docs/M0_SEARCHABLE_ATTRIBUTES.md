# M0 — typed, searchable metadata reference engine

**Status: in-memory correctness prototype, not a mounted filesystem, not a persistent index.**

This crate implements a foundational piece of BFS-NG's BeFS-inspired attribute querying: typed schemas, stable (volume, inode) identifiers, canonical sortable keys, secondary postings, and snapshot-consistent queries. It shares the Rust 2024 workspace with the cache simulator and read-only physical metadata cache, but does not yet integrate either of them into persistent storage.

## Supported types and constraints

| Type | Encoding concept | Query |
| --- | --- | --- |
| Boolean | 1-byte 0/1 tagged scalar | equality |
| Signed i64 | sign-bit-flipped big-endian for order | equality/range |
| Unsigned u64 | big-endian | equality/range |
| Timestamp nanoseconds i64 | sign-bit-flipped big-endian | equality/range |
| Exact UTF-8 string | raw bytes, no hidden Unicode normalization | equality/prefix |
| ASCII folded string | lowercase ASCII, rejects non-ASCII | equality/prefix |
| 32-byte hash | fixed 32-byte tagged key | equality |
| String set | one posting per distinct normalized element | membership |

The schema assigns stable numeric IDs and makes indexing opt-in. Schema names must be lowercase ASCII with dotted components; no more than 256 attributes per schema and 128 stored attributes per file. Individual text values are bounded at 1 KiB, and multivalue sets at 128 elements. No JSON coercion, substring, fuzzy search, floating-point order, locale collation or vector search is promised. This reference encoding is **not a disk format** and cannot be changed into one without an explicit versioned format contract.

## Transactions and consistent snapshots

\`IndexStore::begin()\` opens a writer at the current generation. \`Transaction::upsert()\` replaces the whole attribute map for a stable file ID, or \`delete()\` removes it. \`commit()\` publishes a new immutable \`Arc\` state containing both file attributes and corresponding index postings **atomically under a write lock**.

- Old \`Snapshot\` instances hold their previous immutable state across updates.
- Two writers starting from the same generation cannot both publish: the stale writer gets an explicit \`Conflict\` and must retry.
- On replace and delete, every old posting is removed before the new record is inserted.
- \`Snapshot::indexed_query\` intersects posting sets and sorts stable file IDs. It reports generation and truncation.
- \`Snapshot::full_scan_oracle\` independently enumerates records and evaluates predicates, without reading the index. Randomized differential tests compare both.
- No external filesystem locks, block reservations, disk flushes, root-slot publication, ACLs, quotas, namespaces, or crash recovery exist in this crate. **In-memory atomicity is not disk crash consistency.**

## Query examples

The API uses explicit typed values, not a SQL parser:

\`\`\`rust
use bfs_ng_attribute_index::{Predicate, Value};

let query = [
    Predicate::eq("asset.kind", Value::Text("shader".into())),
    Predicate::contains("asset.depends_on", "texture:7"),
];
// A committed, immutable snapshot:
let result = store.snapshot()?.indexed_query(&query, 1000)?;
for file_id in result.files {
    println!("{}:{}", file_id.volume, file_id.inode);
}
\`\`\`

AxiomZig can index *separate* compiler-observed diagnostics and intended labels; the generator's \`schema_only\` status must not be silently treated as \`compiler_checked\`. AxiomMegaDoc can index \`megadoc.split\`, \`megadoc.construction\`, \`megadoc.validation_trust\`, and source record identifiers; the 260 benchmark-only views must not be confused with the 20 verified documents. DVE can use multivalue attributes for \`asset.depends_on\`, allowing a reverse dependency query by stable ID.

Current test fixtures use *representative fields*; the repository's separate real-world benchmark branch contains source-backed AxiomZig, DVE and AxiomMegaDoc workloads.

## Performance and cost boundaries

The in-memory index uses a \`BTreeSet\` of \`(attribute_id, encoded_value, file_id)\` postings and \`BTreeMap\` records, with **full state cloning on each committed transaction**. This deliberately favors verifiable semantics over write throughput. Query intersections materialize candidate sets in memory. A future CoW B+ tree must maintain the same logical correctness contract but use logarithmic node copy-on-write rather than clone the entire index.

For now benchmark query result correctness (query hit sets and updates), query-entry counts, deterministic limits, and snapshot behavior. Do not compare in-memory prototype timing against on-disk BeFS or SQLite without accounting for durable writes, cache warming, ACL filtering and index-build/update cost.

## Next milestones

1. F0/F1: specified checksummed superblock, block device abstraction, image-backed read validation and fault injection.
2. F2: crash-safe transaction publication and snapshot/root retention. Generation-safe keys alone do **not** provide retention.
3. F4: persistent CoW attribute nodes and B+ tree postings, index and inode changes committed to the same root.
4. Reuse real-world AxiomZig, DVE and AxiomMegaDoc fixture manifests as a full-scan oracle for query behavior and cost.
5. Add authorization-aware query cursors, durable change log/live queries and comprehensive adversarial fuzzing.

Run \`cargo test -p bfs-ng-attribute-index\` for tests. CI also runs Clippy with warnings-as-errors, rustfmt, and the rest of the workspace suite.
