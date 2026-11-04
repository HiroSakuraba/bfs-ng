# BFS-NG C1–C3 design and correctness boundaries

## Scope

This first tranche deliberately **does not cache real disk data**. It is a
research simulator for evaluating possible read-side caching choices. The
filesystem's CoW commit ordering, metadata validation, snapshot isolation,
checksums and space accounting remain independent of the policy evaluator.

## Memory ownership and double caching

A user-space Windows WinFsp or Linux FUSE adapter may already benefit from
native OS caching. A hypothetical second BFS-NG cache must not assume that a
simulated miss is an actual storage-device miss. Instrument at least these
layers independently:

1. In-process filesystem-cache lookup, eviction and buffer charge.
2. OS page-cache residency/fault data, where observable and safe to collect.
3. Submitted storage I/O, completion time and queue depth.
4. End-to-end application lookup and read latency distributions.
5. CPU cache/TLB misses and LLC stalls on the target machine.

This lab models *only* the first layer's decisions.

## Replay invariants

- Demand requests are counted once per trace event marked non-speculative.
- Speculative requests may admit objects but are excluded from demand hit rate.
- Resident charged bytes never exceed the per-run capacity.
- Each key has an immutable logical size and kind within a trace. Real
  implementations must key CoW metadata by immutable generation or explicitly
  invalidate stale residents before a later generation becomes visible.
- `refill_cost_ns` is a modeled, exogenous input; no observed p99 result can
  be inferred from it. Never equate summing modeled costs with end-to-end
  critical-path time when I/O overlaps.
- Each policy consumes the *same* trace in order, with identical byte budget.
- Cache metadata overhead (hash tables, sketch and ghost keys) is *reported or
  described*, not yet charged to a global system memory budget. Do not claim
  equal total RAM usage among policies.

## Preliminary experiments

Compare all policies across multiple capacities and workloads:

| Workload | Purpose | Failure mode to detect |
|---|---|---|
| hotset | Skewed reuse and small working set | Poor promotion or frequent refault |
| scan | One-hit-wonder stream | Scan pollution of repeatedly used metadata |
| mixed | Concurrent hot metadata and large data | Unfair allocation to large one-off objects |
| phase | Changing working set | Frequency history adapting too slowly |
| small / oversized objects | Byte accounting | Capacity overflow or improper admission |
| speculative arrivals | Read-ahead consequences | Inflated reported demand hit rate |

For rigorous evaluation, include **real** anonymized traces from BFS-NG
prototypes and compare against OS-default caching with application-visible
p50/p99 latency, physical read requests/bytes, throughput, memory used and
CPU cycles per operation. Never select a winner based on synthetic hit rate
alone. Use controlled A/B tests, repeated trials, pinned workloads and
confidence intervals.

## Next milestones

C4: production-ready read-only immutable metadata cache, with a strict
per-generation key, single-flight fetch, checked block contents, bounded
memory, proper integration with OS page-cache behavior, and invalidation.

C5: compact B+ tree memory layouts, batched leaf scans and CPU hardware-
performance-counter experiments (including LLC misses and TLB misses).

C6: conservative async prefetch with demand queue priority, admission control,
max outstanding I/O and cancellation on phase changes. Speculative I/O must
not block a normal demand read in a saturated queue.

C7: sampled miss-ratio curves, budget limits per class, and slow feedback
control based on *measured* marginal user-visible latency rather than raw
hit-rate alone.

**Do not** put dynamic caching or learning into the transactional write path.
