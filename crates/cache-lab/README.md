# BFS-NG Cache Lab — first implementation tranche

A **dependency-free, safe-Rust** trace replay simulator for the initial BFS-NG
cache research program (C1–C3). It is **not** a filesystem, a block-cache
implementation, a Windows driver, or a benchmark of physical devices.

The immediate goal is to compare cache policy decisions on an identical,
reproducible request stream without touching the correctness-critical write
path. This repository implements:

- `none`: no BFS-NG cache (baseline for measuring potential added-cache value)
- `lru`: straightforward exact, **slow** resident-byte LRU reference
- `s3fifo`: Small/Main/Ghost FIFO model with 2-bit reference counters
- `wtinylfu`: a **simplified research variant** with a window, segmented LRU
  main, frequency-sketch admission, and periodic aging
- Strict version-1 CSV trace schema, reproducible deterministic synthetic
  generators, per-type miss reports, CSV capacity sweeps and invariant tests.

## Requirements

Rust **1.85+**, Cargo. This project declares Rust edition 2024 and has **no
third-party dependencies**. The pinned compiler is in `rust-toolchain.toml`.

### First run

```bash
cargo test
cargo run --release -- demo --events 50000 --seed 42 --capacity-bytes 1048576
```

### Trace generator and replay

```bash
cargo run --release -- generate --pattern mixed --events 100000 \
  --seed 42 --output mixed.csv

cargo run --release -- replay --input mixed.csv \
  --capacity-bytes 1048576 --policies none,lru,s3fifo,wtinylfu \
  --output replay.csv

cargo run --release -- sweep --input mixed.csv \
  --capacities 65536,262144,1048576,4194304 \
  --output mrc.csv

# Independent Python 3 reference oracle for the five-event golden trace
python3 scripts/lru_oracle.py fixtures/golden.csv 8192
```

`replay.csv` and `mrc.csv` include one `class=all` row and four class-specific
rows per (policy, cache capacity). Hit ratios and modeled miss-cost totals
exclude speculative events, though speculative events **can** populate the
cache. The no-cache model ignores all accesses for residency but still counts
demand misses. The simulator is single-threaded and deterministic.

## Trace schema v1

```csv
timestamp_ns,stream_id,key,charged_bytes,kind,refill_cost_ns,speculative
0,0,1,4096,metadata,100,0
1,0,2,4096,metadata,100,0
2,0,1,4096,metadata,100,0
```

- `key`: stable **opaque u64** identity. When content changes, allocate a
  distinct key incorporating its generation; the simulator does not support
  invalidation yet.
- `charged_bytes`: resident bytes including the simulated cache-entry charge.
  Cache budgets use bytes, not item counts. The *actual policy bookkeeping*
  memory is not included in the capacity limit, and must later be measured.
- `kind`: `data`, `metadata`, `directory` or `index`.
- `refill_cost_ns`: **hypothetical marginal penalty supplied by the trace**.
  It is neither an observed user wait nor a measured SSD service time. These
  model costs are useful only for sensitivity analyses.
- `speculative`: `1` means the object was prefetched and assumed available at
  that trace point; no asynchronous completion time is modeled. These events
  do **not** count as demand requests in the resulting hit rate.
- Timestamps must be nondecreasing; a key's kind and size cannot change.
- Real traces should use per-run salted/opaque keys and must not contain paths,
  file names, personally identifying metadata, or secrets.

A warmed block in the model might already reside in the OS page cache. A
BFS-NG cache *hit* does not imply a prevented physical SSD read. This trace
lab cannot measure Linux/Windows page-cache overlap, CPU L1/L2/L3 misses,
TLB behavior, I/O scheduling, concurrent lock contention, SSD latency or
application p99 latency. Those require independent system measurements.

## Policy fidelity and known limitations

`lru` is a correctness baseline, **not** a low-overhead production design:
eviction scans a map to find the oldest object, O(cache entries) per eviction.

`s3fifo` models the Small/Main/Ghost structure and capped hit counters from
the SOSP 2023 paper. It differs from optimized native implementations:
variable-size objects are charged by bytes; ghost history has a byte-equivalent
budget; ghost removal is O(n); no atomic hit path, concurrency, or physical
memory overhead is simulated. Do not infer the paper's throughput claims from
this model.

`wtinylfu` is **not the exact Caffeine implementation**. It uses a fixed 1%
window with a 4 KiB minimum, a simple four-row count-min sketch that ages
periodically, and a probation/protected main area with an 80% protected target.
It omits hill climbing, a doorkeeper, buffering of access records, and some
size-aware victim optimizations. In particular, an object larger than both
its fixed window and its main region is currently bypassed even if it could
fit in the total cache; treat this as an explicit experimental limitation. Refer to the source for the precise model.

Neither policy's numeric results should be presented as conclusive without
cross-checking with libCacheSim or a known reference implementation.

## Exit criteria for C1–C3

- [x] Strict trace schema and reproducible generator source
- [x] Policy-independent byte-capacity accounting and per-class reporting
- [x] No-cache and LRU reference implementations
- [x] S3-FIFO and Window TinyLFU-inspired simulator source
- [x] Golden trace, property-style invariants and GitHub Actions CI definition
- [ ] Run `cargo fmt`, `cargo clippy`, `cargo test` on a working Rust toolchain
- [ ] Compare S3-FIFO against libCacheSim on common unit-sized traces
- [ ] Compare TinyLFU behavior to a reference implementation
- [ ] Collect real BFS-NG/OS page-cache traces and latency measurements
- [ ] Determine how much *additional* cache memory benefits application p99

**Build status:** source and fixtures prepared; a Rust compiler was not
available in the authoring environment, so compilation and Rust test success
have **not** yet been verified. The CI workflow is provided for this next step.

## Research sources

1. Juncheng Yang et al., *FIFO Queues Are All You Need for Cache Eviction*,
   SOSP 2023: https://s3fifo.com/ and https://doi.org/10.1145/3600006.3613147
2. Gil Einziger, Roy Friedman, Ben Manes, *TinyLFU: A Highly Efficient Cache
   Admission Policy*, ACM Transactions on Storage 2017:
   https://doi.org/10.1145/3149371
3. Caffeine author documentation, W-TinyLFU design:
   https://github.com/ben-manes/caffeine/wiki/Efficiency
4. libCacheSim reference/simulator:
   https://github.com/1a1a11a/libCacheSim

See [`docs/DESIGN.md`](docs/DESIGN.md) for the policy ownership model, goals,
measurement protocol and next implementation milestones.
