# Searchable metadata: BFS-NG architecture and benchmark plan

**Status: design proposal, not implemented or exposed as filesystem APIs.**

BFS-NG's distinguishing capability should be **transactionally indexed, typed file attributes** with snapshot-consistent queries. This follows the spirit of BeFS/Haiku BFS attribute indices while avoiding treating a directory tree and external search database as independent sources of truth. This proposal presumes a checksummed copy-on-write metadata store and independently tested root publication/recovery; the current repository has **neither persistent indexing nor filesystem mounts**.

## 1. Model

Every file has a stable object identity (not its path), a volume identity, and a typed attribute map. Names and attributes are transactions against the same logical metadata store. Attribute names have schema identifiers and versioned normalization rules, not uncontrolled string coercions.

| Type | Canonical v1 key payload | Supported predicates |
| --- | --- | --- |
| bool | 0/1 | equal |
| signed int64 | biased big-endian 64-bit encoding (flip sign bit) | equality/range |
| unsigned int64 | big-endian unsigned integer | equality/range |
| UTC timestamp | epoch-nanoseconds int64; same sortable encoding | equality/range |
| UTF-8 string | normalized to a schema-declared case/Unicode policy; length-bounded | equality/prefix |
| bytes/hash | opaque length-bounded byte string, no automatic locale or case rules | equality |
| multivalue string | one index entry per distinct normalized element | membership |

Floating point, arbitrary JSON, substring/regex and vector similarity search are **not** indexed in v1. If introduced, each requires defined NaN/-0/collation, payload limits and query semantics. Do not expose undocumented datatype coercions.

### Example: AxiomZig

A file might have the following typed *derived attributes*, sourced from a specific compiler validation run:

\`\`\`text
az.stream                  = "ownership_moves"
az.compiler.version        = "v47.60"
az.validation.mode         = "compiler_checked"
az.validation.agreement    = "full"
az.check.ok                = false
az.check.diagnostic_code[] = ["E_OWN_MOVED", "E_OWN_UNDISCHARGED"]
az.source.sha256           = bytes(32)
\`\`\`

**Important:** AxiomZig's corpus design distinguishes **generator labels** from **compiler-observed attributes**. \`az.label.expected_outcome\` is not proof of \`az.check.ok\`; only an observed validation pass at a specific compiler version can populate \`az.check.*\`. The synthetic benchmark generator produces \`schema_only\` labels and **must never claim compiler validation**.

A test query could mean:

\`\`\`sql
SELECT file_id
FROM files
WHERE az.stream = 'ownership_moves'
  AND az.validation.agreement = 'full'
  AND az.check.diagnostic_code CONTAINS 'E_OWN_MOVED'
AT SNAPSHOT 812;
\`\`\`

### Example: DVE

\`\`\`text
asset.kind          = "shader"
asset.renderer      = "vulkan"
asset.material[]    = ["glass", "frosted_glass"]
asset.content_hash  = bytes(32)
asset.depends_on[]  = [file_id_102, file_id_117]
asset.variant       = "roughness_0.2"
\`\`\`

Queries could find \`asset.kind='shader' AND asset.renderer='vulkan'\`, or reverse dependencies \`asset.depends_on CONTAINS modified_texture_id\`. Stable file IDs matter here: moving or renaming a texture should not invalidate dependency edges merely because its pathname changes. The actual DVE project stores dependencies in differing asset formats; these are **proposed future BFS-NG attributes**, not claims that such fields are already on disk.

## 2. CoW index structure

For a first implementation, favor a copy-on-write B+ tree per indexed attribute with **bytewise sortable canonical keys**:

\`\`\`text
IndexEntry = (attribute_schema_id, type_tag, canonical_value, file_id)
Value      = { optional reference/flags }
\`\`\`

There is at most one posting per \`(attribute, value, file_id)\`, except for explicitly versioned/multivalued semantics. Directory names are another index keyed by \`(parent_dir_id, normalized_name, file_id)\`. Query planners use indexed equality/range scans and intersections with stable IDs rather than enumerate every file and call \`stat\`.

### Transactional publication invariant

For transaction \`T\` modifying a file's content/attributes/name:
1. Allocate and write changed CoW blocks and any new attribute/index nodes.
2. Update secondary index postings **within the same transaction**: delete stale keys, insert new keys.
3. Validate all new metadata references and checksums.
4. Publish a new checksum-protected root pointer with a monotonic generation using the storage durability protocol.
5. A query pins one **committed root generation**, so it sees either all of \`T\` or none of \`T\`.

No reader may mix an inode at generation \(g\) with an index root at generation \(g+1\). Index repair/rebuild must create a new tree and atomically switch its published root. Recovery must reject partial or corrupt root transactions, not attempt speculative index replay. Existing C5 metadata-cache blocks have allocation/birth generations; that is distinct from the snapshot root's generation.

**Space and write amplification caveat:** Indexing every attribute is expensive. Start with explicit opt-in indexed schema IDs and hard limits on string length, postings, and total attributes per inode. Measure CoW path copying, shared-node retention, and checkpoint/snapshot pressure. Later compare B+ trees, log-structured indexes and compressed posting lists under the same crash correctness constraints.

## 3. Query API (illustrative; not code that exists)

\`\`\`rust
let view = filesystem.snapshot()?; // pins a committed root
let query = view
    .query()
    .eq("asset.kind", Value::String("shader".into()))?
    .contains("asset.material", Value::String("glass".into()))?
    .limit(10_000);
for entry in query.execute()? {
    println!("{}", entry.file_id());
}
\`\`\`

A typed planner should reject mismatched schema types, excessive limits, unauthorized attributes, unsupported predicates and ambiguous collation. Query results carry \`file_id\` and \`snapshot_generation\` so later reads can use the same view. ACL checks must not leak private files via result counts, error timing or unfiltered index scans; authorization and namespace visibility belong in the query engine and its tests.

### Live queries after durable storage exists

Live queries require a monotonically ordered commit/change feed, with \`(volume, sequence, transaction_id, changed_file_id, old/new index keys)\` for atomic updates and reconnect/resume. Watch notifications alone are not a durable change log. Define retention, overflow, compaction, security and duplicate delivery rules before advertising exactly-once behavior. A practical first contract is at-least-once delivery with a resume cursor, duplicate suppression by the consumer, and resnapshot-required when a cursor expires.

## 4. Real-world query benchmark design

Use the existing \`benchmarks/real-world/\` trace suite as a *corpus provider*. Build a separate query harness as the indexing implementation matures:

| Corpus | Query | Correctness oracle |
| --- | --- | --- |
| AxiomZig | \`az.stream='ownership_moves'\` | full manifest enumeration |
| AxiomZig | compiler-observed diagnostic membership | independently validated sidecar attributes; never generator labels |
| AxiomZig | \`validation.agreement='full'\` and compiler version | source/attribute join at a pinned dataset revision |
| DVE | \`asset.kind='shader'\` with renderer/material filters | independently parsed source asset manifests |
| DVE | \`asset.depends_on CONTAINS file_id\` | independently traversed dependency graph |
| DVE | prefix/path query after rename | stable-file-id reference and directory enumeration |
| Both | query repeated before/after CoW update | single-snapshot full-scan oracle |

Compare:
1. An ordinary directory walk + parse/stat baseline.
2. SQLite as a **separately maintained search index** (include build/update cost and sync lag).
3. A native BFS-NG indexed query against the **same data and result set**.

Measure cold/warm p50, p95, p99, memory and CPU LLC misses where tools exist, index disk footprint, query-plan page reads, transactional write amplification, and recovery after injected crashes. A faster query that returns inconsistent or unauthorized results fails regardless of latency. Query trace generation should instrument actual index-page reads when the index engine exists; current v1 \`index\` events in \`bench.py\` are **synthetic logical requests only**.

## 5. Roadmap and correctness gates

- **M0:** typed attribute schema, codecs, normalization/collation tests, and an independent in-memory sorted-map oracle.
- **M1:** image-backed CoW inode and attribute storage; byte-level encoding and malformed metadata fuzzing.
- **M2:** CoW B+ tree with stable file IDs, index mutation atomic with inode metadata; crash/recovery model checks.
- **M3:** snapshot-consistent range/equality/membership queries; randomized differential test versus full scan.
- **M4:** cost-based plan selection, statistics, index size budgets, cache instrumentation and real-world AxiomZig/DVE query workloads.
- **M5:** durable live change feed, cursor resume, overflow/rebuild and ACL tests.

### References

- Haiku BFS code: https://github.com/haiku/haiku/tree/master/src/add-ons/kernel/file_systems/bfs
- BeFS design background: https://www.haiku-os.org/legacy-docs/practical-file-system-design.pdf
- Btrfs design (CoW/references): https://btrfs.readthedocs.io/en/stable/dev/dev-btrfs-design.html
- SQLite query planner: https://www.sqlite.org/queryplanner.html
- AxiomZig corpus design: https://github.com/HiroSakuraba/AxiomZig/blob/main/axiomzig_500_bundle_corpus_design.md
- DVE package/scene layout: https://github.com/HiroSakuraba/destructible-voxel-engine/blob/main/docs/PACKAGING.md
