use bfs_ng_cache_lab::policies::make_policy;
use bfs_ng_cache_lab::simulator::{replay_all, replay_with_policy};
use bfs_ng_cache_lab::trace::{Event, Kind, Pattern, generate, validate};

fn e(time: u64, key: u64, bytes: u64) -> Event {
    Event {
        timestamp_ns: time,
        stream_id: 0,
        key,
        charged_bytes: bytes,
        kind: Kind::Metadata,
        refill_cost_ns: 100,
        speculative: false,
    }
}

#[test]
fn lru_golden_trace_matches_hand_calculation() {
    let trace = [
        e(0, 1, 4096),
        e(1, 2, 4096),
        e(2, 1, 4096),
        e(3, 3, 4096),
        e(4, 1, 4096),
    ];
    let lru = replay_with_policy(&trace, make_policy("lru", 8192).unwrap());
    assert_eq!((lru.total().hits, lru.total().misses), (2, 3));
    assert_eq!(lru.total().modeled_miss_cost_ns, 300);
    let baseline = replay_with_policy(&trace, make_policy("none", 8192).unwrap());
    assert_eq!((baseline.total().hits, baseline.total().misses), (0, 5));
}

#[test]
fn capacity_zero_and_oversized_objects_are_never_admitted() {
    let trace = [e(0, 1, 8192), e(1, 1, 8192)];
    for name in ["none", "lru", "s3fifo", "wtinylfu"] {
        for cap in [0, 4096] {
            let r = replay_with_policy(&trace, make_policy(name, cap).unwrap());
            assert_eq!(r.total().hits, 0, "{name} at {cap}");
            assert_eq!(r.resident_bytes, 0, "{name} at {cap}");
        }
    }
}

#[test]
fn repeated_object_is_a_hit_when_alone_and_fits() {
    let trace = [e(0, 7, 4096), e(1, 7, 4096)];
    for name in ["lru", "s3fifo", "wtinylfu"] {
        let r = replay_with_policy(&trace, make_policy(name, 4096).unwrap());
        assert_eq!(r.total().hits, 1, "{name}");
    }
}

#[test]
fn byte_capacity_is_never_exceeded_under_mixed_workloads() {
    for pattern in [
        Pattern::Hotset,
        Pattern::Scan,
        Pattern::Mixed,
        Pattern::Phase,
    ] {
        let trace = generate(pattern, 5000, 42);
        validate(&trace).unwrap();
        for cap in [0, 4096, 32768, 65536, 524288] {
            for name in ["none", "lru", "s3fifo", "wtinylfu"] {
                let mut policy = make_policy(name, cap).unwrap();
                for ev in &trace {
                    policy.access(ev.key, ev.charged_bytes, ev.speculative);
                    assert!(policy.used() <= cap, "{name} at {cap}");
                }
            }
        }
    }
}

#[test]
fn sweep_returns_one_row_per_policy_capacity_and_class() {
    let t = [e(0, 1, 4096), e(1, 2, 4096), e(2, 1, 4096)];
    let r = replay_all(&t, &["none".into(), "lru".into()], &[0, 4096, 8192]).unwrap();
    assert_eq!(r.len(), 6);
    for row in &r {
        assert_eq!(row.total().requests, 3);
    }
}

#[test]
fn immutable_size_identity_is_enforced() {
    let t = [e(0, 5, 4096), e(1, 5, 8192)];
    assert!(replay_all(&t, &["lru".into()], &[4096]).is_err());
}

#[test]
fn speculative_preload_does_not_count_as_demand_request() {
    let a = e(0, 7, 4096);
    let trace = [
        Event {
            speculative: true,
            ..a
        },
        e(1, 7, 4096),
    ];
    for name in ["lru", "s3fifo", "wtinylfu"] {
        let r = replay_with_policy(&trace, make_policy(name, 8192).unwrap());
        assert_eq!((r.total().requests, r.total().hits), (1, 1), "{name}");
        assert_eq!(r.speculative_requests, 1);
    }
}

#[test]
fn no_aliasing_across_generations_by_distinct_keys() {
    let trace = [e(0, 99, 4096), e(1, 100, 4096), e(2, 100, 4096)];
    let report = replay_with_policy(&trace, make_policy("lru", 4096).unwrap());
    assert_eq!((report.total().hits, report.total().misses), (1, 2));
}
