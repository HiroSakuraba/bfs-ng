# AxiomMegaDoc as a BFS-NG benchmark workload

This add-on consumes *generated data* from [HiroSakuraba/AxiomMegadoc](https://github.com/HiroSakuraba/AxiomMegadoc), not AxiomZig. Its upstream v0.11 baseline contains 520 Scheme/ML/Prolog atomic records and a reusable builder packaged in \`axiom_megadoc_v0_11.zip\`. The v0.12 source PR adds a deterministic materializer (\`src/build_v012.py\`), with provenance and checker-observed-diagnostic constraints.

**Important:** AxiomMegaDoc \`latent_diagnostic\` and \`runtime_trace\` are distinct constructions. The default generator emits semantically compatible, checker-grounded latent-diagnostic documents. The optional `--runtime-traces` mode executes three representative programs through Guile, MLton, and SWI-Prolog, checks runtime outputs against ledger expectations, and records runtime versions; the end-to-end BFS-NG CI enables this option. It does **not** claim every source record has been runtime-validated. Wider source-pair *benchmark views* are explicitly marked nontraining.

## Generate the source corpus

\`\`\`sh
git clone https://github.com/HiroSakuraba/AxiomMegadoc.git ../AxiomMegadoc
# Until the upstream PR merges, check out its reviewed feature branch:
git -C ../AxiomMegadoc checkout feature/v012-reproducible-megadocs

python3 ../AxiomMegadoc/src/build_v012.py \
  --output benchmarks/real-world/local-data/axiommegadoc
# To include the three separately runtime-verified documents (as in CI):
# Install guile-3.0, mlton, and swi-prolog-nox, then use --runtime-traces
# with an empty output directory.
\`\`\`

Do not commit local corpus data, inventories or replay outputs. The source generator's ZIP SHA256, ledger SHA256, and builder SHA256 are recorded in its \`manifest.json\`. Generated data include long \`megadocs/*.md\`, typed-provenance \`attributes/*.json\`, immutable source record blobs in \`records/*.json\`, per-split \`streams/*.jsonl\`, and broader nontraining \`benchmark-views/*.md\`.

## Replay four access patterns

\`\`\`sh
BASE=benchmarks/real-world
DATA=$BASE/local-data/axiommegadoc

# Actual reads by the benchmark harness, not by a mounted BFS-NG:
python3 $BASE/megadoc.py --root "$DATA" --pattern assemble \
  --split train --passes 3 --mode record \
  --output $BASE/results/megadoc-assemble.csv \
  --summary $BASE/results/megadoc-assemble.json

# Sequential/repeated megadoc reads:
python3 $BASE/megadoc.py --root "$DATA" --pattern train \
  --split train --passes 5 --mode model \
  --output $BASE/results/megadoc-train.csv

# Attribute lookup and repeated source-provenance traversal:
python3 $BASE/megadoc.py --root "$DATA" --pattern provenance \
  --split all --passes 4 --mode model \
  --output $BASE/results/megadoc-provenance.csv

# Mixed workload including assembly, reading and queries:
python3 $BASE/megadoc.py --root "$DATA" --pattern mixed \
  --split train --passes 4 --mode model \
  --output $BASE/results/megadoc-mixed.csv

cargo run --release -p bfs-ng-cache-lab -- sweep \
  --input $BASE/results/megadoc-mixed.csv \
  --capacities 65536,262144,1048576,4194304 \
  --policies none,lru,s3fifo,wtinylfu \
  --output $BASE/results/megadoc-cache-sweep.csv
\`\`\`

Training-style (`train`, `mixed`) workloads reject `--split all` to prevent accidental train/dev/test mixing. The `assemble` and `provenance` research traces may use `all` when needed to stress cache behavior, but should not be consumed as a training stream.

All trace events conform to BFS-NG's strict \`timestamp_ns,stream_id,key,charged_bytes,kind,refill_cost_ns,speculative\` schema. Chunk identities are opaque salted hashes of source archive+builder+ledger revisions, kind, relative object identity and logical block index. Repeated file accesses walk successive bounded logical read windows where the file is larger than the per-access maximum. \`--chunk\` defaults to 4 KiB and \`--max-read\` to 64 KiB; \`--mode record\` actually opens/reads bounded file portions.

### Meaning of trace classes

| Event | What generates it | Reality boundary |
| --- | --- | --- |
| \`data\` | Source-record, document, provenance-view and JSONL block reads | Actual calls only in \`record\` mode; logical bytes in \`model\` |
| \`metadata\`, \`directory\` | Simulated lookups for each accessed path | Not kernel \`stat\` or page-cache telemetry |
| \`index\` | Queries by family/language and attribute-file lookups | **Modeled**, not an implemented BFS-NG index |

The BFS-NG cache-lab provides **policy-model cache hits**, not observed SSD demand I/O, OS caching, CPU LLC misses or application p99. Compare model results only on the same trace/capacity, and separately instrument a real filesystem when available.

## Searchable metadata opportunities

A future BFS-NG typed attribute index can directly search by \`megadoc.construction\`, \`megadoc.language\`, \`megadoc.split\`, \`megadoc.source_hash\`, \`megadoc.validation_trust\`, and \`megadoc.record_ids[]\`. Exact split inheritance and provenance checks are mandatory, so a query cannot accidentally mix train and test documents. These are **proposed schema fields**; the current \`index\` trace events are synthetic.

## Tests and CI

The regular CI runs dependency-free fixture tests. The external [AxiomMegaDoc benchmark workflow](../../.github/workflows/megadoc-source.yml) checks out a pinned upstream source commit, regenerates the corpus, emits recorded/model traces, runs the Rust cache policies, and uploads only aggregate traces/reports. The upstream source commit is pinned to avoid silent training-data changes.

The benchmark suite is separated from \`main\` until the parent AxiomZig/DVE real-world benchmark PR has merged. The new source PR and this BFS-NG benchmark PR can be independently reviewed.

## Initial end-to-end corpus composition

With the optional lane verification enabled, the pinned test regenerates **20 verified megadocs** (17 paper-aligned latent-diagnostic views plus 3 executed and output-checked runtime_trace views), **260 broader benchmark-only provenance views**, and **520 source-record JSON objects**. It does not synthesize additional training examples or claim the scaling/evaluation results from the megadocs research paper.
