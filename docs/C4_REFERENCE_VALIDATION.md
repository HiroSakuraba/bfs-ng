# C4 — Independent cache policy comparison

## Reference implementations

Use independently maintained [libCacheSim](https://github.com/1a1a11a/libCacheSim), [its Python interface](https://cachemon.github.io/libCacheSim-python/api/), the [SOSP S3-FIFO research code](https://github.com/Thesys-lab/sosp23-s3fifo), and the [Caffeine simulator](https://github.com/ben-manes/caffeine/wiki/simulator) as references.

The current BFS-NG S3-FIFO and W-TinyLFU policies are **research variants**, not exact replicas. In particular, the Rust W-TinyLFU uses a fixed minimum 4 KiB window, no doorkeeper, no adaptive window hill-climbing, and its own aged frequency sketch. Mixed-size S3-FIFO queue management also differs from published reference implementations. Report these differences instead of demanding exact hit counts.

## Run the independent comparison

```sh
python3 -m pip install libcachesim
python3 -m unittest discover -s crates/cache-lab/scripts -p 'test_*.py'
python3 crates/cache-lab/scripts/compare_libcachesim.py \
  --trace crates/cache-lab/fixtures/golden.csv --capacity 8192 \
  --require-lru-match --output c4-golden.json
cargo run --release -p bfs-ng-cache-lab -- generate \
  --output c4-mixed.csv --events 5000 --seed 42 --pattern mixed
python3 crates/cache-lab/scripts/compare_libcachesim.py \
  --trace c4-mixed.csv --capacity 262144 \
  --require-lru-match --output c4-mixed.json
```

The runner normalizes the reference W-TinyLFU window fraction to the Rust model's minimum 4 KiB window. That aligns the window budget in bytes, but not the policies' frequency sketches, admission decisions or protected-region behavior. It replays exactly the same demand object IDs, sizes and order against the independent library and Rust. It outputs request and byte miss deltas. LRU acts as an exact-match control; S3-FIFO/TinyLFU deltas are descriptive, **not** a claim that implementations are incorrect.

Traces with speculative access, size-mutating identities, zero sizes or zero capacity cannot currently be compared fairly. The supplied miss-cost estimates are not physical storage measurements.

## Reproducibility and acceptance

The `reference-comparison.yml` GitHub Actions workflow pins libCacheSim 0.3.5, records installed distribution versions, and uploads evidence as JSON. It is intentionally **not a required branch-protection check** while the upstream Python package version remains unpinned.

C4 is only independently validated once that job completes successfully, artifacts have been preserved, the exact upstream version is recorded and unexpected LRU mismatches investigated. A subsequent tranche can drive Caffeine's own simulator and align TinyLFU configuration in more depth.

## First observed differential results (libCacheSim 0.3.5)

On the 5,000-event mixed workload at 262,144 bytes, the independent LRU control matched exactly (4,602 misses each). The Rust S3-FIFO research variant had 4,248 misses versus 3,616 for libCacheSim; Rust W-TinyLFU had 3,689 versus 3,321 after aligning the minimum admission window to 4 KiB. This is **not algorithm conformance**, nor a general performance ranking. The original 5-event golden trace also matched LRU exactly (3 misses). The workflow archives JSON including byte-miss totals because request and byte outcomes can move in opposite directions.

## Comparative results across cache capacities (5,000 requests each)

These measurements come from the PR's independent GitHub Actions run with libCacheSim **0.3.5**, a fixed seed of 42, and identical demand traces. They compare **request misses**; lower is better **within the same trace and capacity**. They are not general performance benchmarks or evidence of implementation equivalence.

| Workload | Capacity | Policy | BFS-NG misses | libCacheSim misses |
|---|---:|---|---:|---:|
| Mixed | 256 KiB | S3-FIFO | 4,248 | 3,616 |
| Mixed | 256 KiB | W-TinyLFU | 3,689 | 3,321 |
| Mixed | 1 MiB | S3-FIFO | 2,411 | 2,424 |
| Mixed | 1 MiB | W-TinyLFU | 2,348 | 3,266 |
| Hotset | 1 MiB | S3-FIFO | 984 | 975 |
| Hotset | 1 MiB | W-TinyLFU | 916 | 2,432 |
| Phase | 1 MiB | S3-FIFO | 1,423 | 1,425 |
| Phase | 1 MiB | W-TinyLFU | 1,366 | 2,729 |

**Every LRU control matched exactly** across the sampled capacities and workloads. At 256 KiB, S3-FIFO and W-TinyLFU differ substantially from libCacheSim; at 1 MiB S3-FIFO approaches the reference on some workloads while W-TinyLFU remains different. The byte-miss results can diverge from request-miss results because blocks vary in size.

Full per-workload JSON and dependency versions are preserved by the [independent reference workflow](https://github.com/HiroSakuraba/bfs-ng/actions/workflows/reference-comparison.yml). Future work should isolate allocation-size effects, frequency-sketch differences and S3-FIFO ghost handling before treating either Rust model as production-policy equivalent.
