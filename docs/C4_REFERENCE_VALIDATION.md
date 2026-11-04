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

The runner replays exactly the same demand object IDs, sizes and order against the independent library and Rust. It outputs request and byte miss deltas. LRU acts as an exact-match control; S3-FIFO/TinyLFU deltas are descriptive, **not** a claim that implementations are incorrect.

Traces with speculative access, size-mutating identities, zero sizes or zero capacity cannot currently be compared fairly. The supplied miss-cost estimates are not physical storage measurements.

## Reproducibility and acceptance

The `reference-comparison.yml` manual GitHub Actions workflow installs libCacheSim, records the installed distribution versions, and uploads evidence as JSON. It is intentionally **not a required branch-protection check** while the upstream Python package version remains unpinned.

C4 is only independently validated once that job completes successfully, artifacts have been preserved, the exact upstream version is recorded and unexpected LRU mismatches investigated. A subsequent tranche can drive Caffeine's own simulator and align TinyLFU configuration in more depth.
