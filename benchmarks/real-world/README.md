# Real-world cache workload suite — AxiomZig and DVE

This directory turns **real checked-out files** into reproducible **application-access workloads** for the BFS-NG cache-lab. It does **not** mount or write a BFS-NG filesystem. The source repositories remain untouched.

The input formats are based on [AxiomZig's corpus design](https://github.com/HiroSakuraba/AxiomZig/blob/main/axiomzig_500_bundle_corpus_design.md), where documents, generator labels and compiler-observed attributes are distinct, and [DVE's packaging/scenes documentation](https://github.com/HiroSakuraba/destructible-voxel-engine/blob/main/docs/PACKAGING.md), where a DVOXSCENE manifest refers to individual .dvox/.dmesh assets. DVE also includes glTF buffers and HLSL includes, which the harness follows when the referenced files exist.

**There are two trace modes:**

- **model**: no content reads. Emits a deterministic *modeled* logical request sequence based on inventory sizes, dependency graph and scheduling pattern. Identical inputs, seed and revision produce identical CSV.
- **record**: explicitly opens and reads source file contents (or bounded ZIP-member contents), and records the logical chunks accessed by this harness. Timestamps and observed read byte counts are real; **directory, metadata and index trace events remain modeled**, and \`refill_cost_ns\` is a hypothetical penalty—not a measured latency. This cannot observe OS page cache hits, SSD I/O, kernel block requests, read-ahead, mmap or application accesses outside this harness.

In both modes each trace \`key\` is an opaque 64-bit hash of project revision, object identity, kind and logical chunk number. A trace contains **no filenames**, but because the seed is known it is **not anonymized** against dictionary attacks. Do not publish traces or local inventory manifests from private data without review. A new or modified source generation **requires a new \`--revision\`**, because the cache-lab rejects key size/kind changes and has no invalidation events.

## Prerequisites

- Python 3.10+ (standard library only).
- Rust 1.85+ for downstream \`bfs-ng-cache-lab replay\`.
- Optional local checkouts of AxiomZig and DVE. The GitHub CI suite uses tiny fixtures and does **not** clone other projects or download large archives.

## 1. Generate a safe AxiomZig synthetic corpus

\`\`\`sh
python3 benchmarks/real-world/bench.py generate-axiomzig \
  --output benchmarks/real-world/local-data/az-synthetic --count 128 --seed 42

python3 benchmarks/real-world/bench.py trace --project axiomzig \
  --root benchmarks/real-world/local-data/az-synthetic \
  --revision bfsng-az-synthetic-v1 --pattern compile --passes 4 \
  --mode model --output benchmarks/real-world/results/az-model.csv

python3 benchmarks/real-world/bench.py trace --project axiomzig \
  --root benchmarks/real-world/local-data/az-synthetic \
  --revision bfsng-az-synthetic-v1 --pattern shuffle --passes 3 \
  --mode record --output benchmarks/real-world/results/az-record.csv \
  --summary benchmarks/real-world/results/az-record-summary.json
\`\`\`

Generates \`src/*.az\`, \`documents/shard-000.jsonl\` and \`labels/*.json\`. **These are synthetic arithmetic examples, not outputs validated by the AxiomZig compiler**. The intended label records \`validation_status.mode = schema_only\` and \`compiler_observed = null\`; no ground-truth attributes are fabricated. Do **not** use them as proven compiler training examples. The generator refuses to overwrite nonempty output directories.

## 2. Reuse AxiomZig's actual ZIP corpora

\`\`\`sh
git clone https://github.com/HiroSakuraba/AxiomZig.git ../AxiomZig
git -C ../AxiomZig rev-parse HEAD

python3 benchmarks/real-world/bench.py inventory --project axiomzig \
  --root ../AxiomZig --zip-members \
  --output benchmarks/real-world/results/axiomzig-inventory.json

python3 benchmarks/real-world/bench.py trace --project axiomzig \
  --root ../AxiomZig --zip-members --pattern compile --passes 2 \
  --mode model --max-items 20000 --max-read 65536 \
  --output benchmarks/real-world/results/axiomzig-real-model.csv
\`\`\`

\`--zip-members\` enumerates supported archive members without extraction. Under \`--mode record\`, the selected members are actually decompressed up to \`--max-read\` bytes per scheduled access. Archives are scanned with bounded item counts; path traversal entries, encrypted entries and symlink entries are excluded. For compressed shard scans, **CPU decompression costs confound storage latency**; use \`model\` to isolate the cache-policy trace, or retain separate decompress/read timing measurements.

By default an indexed ZIP archive is treated as a single file; \`--zip-members\` instead treats eligible contained objects as logical cache objects, not literal physical blocks.

## 3. DVE asset-loading workloads

\`\`\`sh
git clone https://github.com/HiroSakuraba/destructible-voxel-engine.git ../destructible-voxel-engine
git -C ../destructible-voxel-engine rev-parse HEAD

python3 benchmarks/real-world/bench.py trace --project dve \
  --root ../destructible-voxel-engine --pattern scene --passes 3 \
  --mode record --output benchmarks/real-world/results/dve-scene.csv \
  --summary benchmarks/real-world/results/dve-scene-summary.json

python3 benchmarks/real-world/bench.py trace --project dve \
  --root ../destructible-voxel-engine --pattern hotreload --passes 5 \
  --mode model --output benchmarks/real-world/results/dve-hotreload.csv

python3 benchmarks/real-world/bench.py trace --project dve \
  --root ../destructible-voxel-engine --pattern package --passes 1 \
  --mode model --output benchmarks/real-world/results/dve-packaging.csv
\`\`\`

- **scene**: read \`game.dvegame\` / \`*.dvoxscene.json\` / \`*.gltf\` and follow existing referenced .dvox, .dmesh, external buffers/images, startup scripts and settings (depth-first, cycle-safe). It is a *representative* loader schedule, not a DVE runtime capture.
- **hotreload**: repeated shader and material texture reads, including existing HLSL \`#include\` relationships.
- **package**: deterministic scan through eligible assets, exercising streaming and cache-pollution resistance.
- **mixed**: combines scenes, hot content and a seeded shuffled cold set.

Only runtime-like paths in \`assets/\`, \`examples/\`, \`shaders/\`, \`content/\`, \`projects/\`, \`tests/fixtures/\` and root \`game.dvegame\` are inventoried. Missing dependencies are **not invented**. The included \`fixtures/dve\` project uses *trace-only dummy payloads*: its .dvox/.bin objects are **not valid DVE engine assets**.

## 4. Compare cache policies

\`\`\`sh
cargo run --release -p bfs-ng-cache-lab -- sweep \
  --input benchmarks/real-world/results/axiomzig-real-model.csv \
  --capacities 65536,262144,1048576,4194304 \
  --policies none,lru,s3fifo,wtinylfu \
  --output benchmarks/real-world/results/axiomzig-cache-sweep.csv
\`\`\`

All policies receive the **same v1 trace**:
\`timestamp_ns,stream_id,key,charged_bytes,kind,refill_cost_ns,speculative\`.

- \`charged_bytes\` is *logical fixed-size cache-page charge* (\`--chunk\`, default 4096 B), **not raw file size**.
- Trace directory and inode-like metadata accesses are modeled 4 KiB objects. The harness does not know filesystem physical layout.
- \`--max-read\` (default 64 KiB, max 16 MiB) bounds bytes processed **per scheduled access**; \`--max-events\` bounds total events.
- The simulator's modeled miss-cost and hit-rate metrics are **not device-level performance numbers**. Use eBPF/ETW/perf separately when a real kernel/filesystem exists.
- ZIP-member workloads model individual logical corpus records, not the archive's actual compressed physical-block layout.

## Repeatability and safeguards

1. Record both source Git SHAs, the BFS-NG commit, seed, revision, trace mode, \`--zip-members\`, page size, \`--max-read\`, and cache capacities in a benchmark report.
2. If files change, **change revision** to avoid stale cache-key identities; if the source is not a Git tree, pass \`--revision\` explicitly. The \`record\` mode detects size/mtime modifications during each file read.
3. Avoid publishing inventories, model summaries or traces from confidential repositories. Inventory JSON deliberately contains relative filenames to support provenance and must be reviewed.
4. CI runs deterministic synthetic and trace-only fixtures. It does not claim production performance or metadata-index superiority.
5. Next stages: DVE runtime instrumented \`ContentSource\` capture; AxiomZig compiler pipeline instrumentation; syscall/page-cache telemetry; true filesystem images once CoW write/recovery are safe.
