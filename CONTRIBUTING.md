# Contributing

This repository is research-stage. Please use issues and pull requests for changes.

## Local validation

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
python3 crates/cache-lab/scripts/lru_oracle.py crates/cache-lab/fixtures/golden.csv 8192
```

Changes to cache algorithms should include deterministic traces, expected outcomes, byte-capacity invariants, and an explanation of any difference from the cited reference algorithms. Do not interpret modeled costs as measured hardware latency.

For eventual storage code: demonstrate crash safety and recovery in fault-injection tests before enabling a write path. Document every unsafe block, persistence ordering assumption and on-disk format change. No real-user-data tests without explicit opt-in.

Avoid adding new crates solely as scaffolding: add functioning, tested modules in focused pull requests.
