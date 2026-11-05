# Integration I4 — F4 typed searchable attributes in the unified v10 image

**Status:** Experimental, CI-gated implementation in [PR #29](https://github.com/HiroSakuraba/bfs-ng/pull/29). The same v10 F0 transaction now covers directory/inode identities, sparse file extent roots, physical checksums, snapshot descriptors, and **typed indexed metadata**. This is an integration of the existing F4 catalog semantics, not a new incompatible query language or a production mount.

## 1. Preserve F4 normalization and indexing

The original F4 \`Catalog\` already has checked typed schemas, stable-inode attribute rows, secondary search postings, exact normalization, equality/range/prefix/set predicates, independent query oracles, and canonical binary serialization. I4 makes its checked encode/decode and inode-set validation available **inside the storage crate**. It adds a reproducible \`encode_sections\` / \`decode_sections\` split:

1. **Schema** — original F4 wire header, version and typed attribute definitions.
2. **Rows** — original per-inode value map with canonical sorted attributes.
3. **Postings** — original searchable posting set, verified against the independent normalized reconstruction of the rows.

Concatenating these three byte sections reproduces the existing F4 format exactly. The v9 disk image remains unchanged, and v10 uses these exact normalization and query semantics.

## 2. Genuine, distinct v10 CoW root roles

V10 \`Manifest.roots.schema\`, \`attribute_rows\`, and \`attribute_postings\` are now three **different authenticated metadata B+tree roots**. Each section is chunked into 3,800-byte values under sorted big-endian chunk indices in its own v10 metadata tree family. Each leaf and ancestor page has its own CRC, volume ID, birth generation and parent block-pointer CRC; reads validate complete high-key branch order, exact chunk order, checked lengths and nonoverlapping physical addresses.

Unlike the existing F4 single private namespace file, these roots belong directly to the same F0 v10 manifest as the namespace and data checksum tree.

Every snapshot descriptor retains its own **historical triple**, not the most recently published live schema, rows, or postings. A query against snapshot generation \`g\` reconstructs only that descriptor's schema/posting/row roots, even after newer F0 generations rotate away.

## 3. Integrated writer operations and authority

\`UnifiedV10\` adds \`install_schema\`, \`set_attributes\`, \`file_attributes\`, \`search_attributes\`, and \`search_snapshot_attributes\`. Rows use stable inode IDs, so renaming a directory or file never rewrites posting keys; unlink removes the inode's rows and postings in the same committed transaction as its directory entry.

Before reusing any physical block, the strict two-F0-root audit independently validates the current generation and **every pinned historical attribute triple**, checks rows reference actual regular-file inode identities in that version's namespace, and verifies postings match the reconstructed row index. An invalid historical attribute root, cross-role alias or missing file inode fails closed. Proposed new attribute roots are staged with namespace, physical checksums and the snapshot catalog, then checked in a read-only overlay before F0 publication.

## 4. Limits and next work

The current implementation **bulk rebuilds each changed attribute section**; it does not provide a localized O(log N) per-inode row/posting updater. It bounds each F4 encoded catalog to 256 KiB and uses conservative 4 KiB metadata pages, a 512-address staging budget, and full-volume/two-root verification on commits. The regular file interface is still at most **256 KiB per file**, without large streaming, hardware-tested power-cut durability, multiwriter exclusion or a mount.

The remaining major user-facing tasks are: a multilevel per-file extent map and bounded streaming writes, localized namespace and attribute CoW updates, durable incremental F5.2/F5.3 free-run/epoch accounting, and a standalone archive tool to benchmark AxiomZig/DVE/MegaDoc files against normal filesystem+database storage.

The regression suite \`storage_v10_f4_attributes\` tests typed normalization, indexed queries across rename and reopen, snapshot-specific historical rows/schema after F0 rotations, unlink with pinned history, immutable schema errors and corrupted fallback metadata.

See [I2 namespace integration](INTEGRATION_I2_V10_NAMESPACE.md), [I3 certified reuse](INTEGRATION_I3_CERTIFIED_REUSE.md), [F5 design](F5_DATA_LAYER.md), and [architecture](ARCHITECTURE.md).
