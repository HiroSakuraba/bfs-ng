use crate::policies::{Policy, make_policy};
use crate::trace::{Event, Kind, validate};

#[derive(Debug, Clone, Copy, Default)]
pub struct ClassStats {
    pub requests: u64,
    pub hits: u64,
    pub misses: u64,
    pub miss_bytes: u128,
    pub modeled_miss_cost_ns: u128,
}

impl ClassStats {
    pub fn hit_rate(&self) -> f64 {
        if self.requests == 0 {
            0.0
        } else {
            self.hits as f64 / self.requests as f64
        }
    }
    fn record(&mut self, event: &Event, hit: bool) {
        self.requests += 1;
        if hit {
            self.hits += 1;
        } else {
            self.misses += 1;
            self.miss_bytes += u128::from(event.charged_bytes);
            self.modeled_miss_cost_ns += u128::from(event.refill_cost_ns);
        }
    }
    fn add(&mut self, other: Self) {
        self.requests += other.requests;
        self.hits += other.hits;
        self.misses += other.misses;
        self.miss_bytes += other.miss_bytes;
        self.modeled_miss_cost_ns += other.modeled_miss_cost_ns;
    }
}

#[derive(Debug, Clone)]
pub struct Report {
    pub policy: &'static str,
    pub capacity_bytes: u64,
    pub per_kind: [ClassStats; 4],
    pub speculative_requests: u64,
    pub evicted: u64,
    pub resident_bytes: u64,
    pub resident_count: usize,
    pub history_count: usize,
}
impl Report {
    pub fn total(&self) -> ClassStats {
        let mut total = ClassStats::default();
        for &c in &self.per_kind {
            total.add(c);
        }
        total
    }
}

pub fn replay_with_policy(events: &[Event], mut policy: Box<dyn Policy>) -> Report {
    let mut by_kind = [ClassStats::default(); 4];
    let mut speculative = 0;
    let mut evicted = 0;
    for event in events {
        let outcome = policy.access(event.key, event.charged_bytes, event.speculative);
        evicted += outcome.evicted;
        assert!(
            policy.used() <= policy.capacity(),
            "{} exceeded byte capacity",
            policy.name()
        );
        if event.speculative {
            speculative += 1;
        } else {
            by_kind[event.kind.index()].record(event, outcome.hit);
        }
    }
    Report {
        policy: policy.name(),
        capacity_bytes: policy.capacity(),
        per_kind: by_kind,
        speculative_requests: speculative,
        evicted,
        resident_bytes: policy.used(),
        resident_count: policy.resident_count(),
        history_count: policy.history_count(),
    }
}

pub fn replay_all(
    events: &[Event],
    policy_names: &[String],
    capacities: &[u64],
) -> Result<Vec<Report>, String> {
    validate(events)?;
    let mut reports = Vec::new();
    for &capacity in capacities {
        for name in policy_names {
            let policy = make_policy(name, capacity)?;
            reports.push(replay_with_policy(events, policy));
        }
    }
    Ok(reports)
}

pub const RESULTS_HEADER: &str = "policy,capacity_bytes,class,requests,hits,misses,hit_rate,miss_bytes,modeled_miss_cost_ns,speculative_requests,evictions,resident_bytes,resident_entries,history_entries";

pub fn report_csv(reports: &[Report]) -> String {
    let mut rows = String::from(RESULTS_HEADER);
    rows.push('\n');
    for report in reports {
        let mut classes: Vec<(&str, ClassStats)> = vec![("all", report.total())];
        classes.extend(
            Kind::ALL
                .iter()
                .map(|&k| (k.as_str(), report.per_kind[k.index()])),
        );
        for (name, c) in classes {
            use std::fmt::Write as _;
            let _ = writeln!(
                rows,
                "{},{},{},{},{},{},{:.6},{},{},{},{},{},{},{}",
                report.policy,
                report.capacity_bytes,
                name,
                c.requests,
                c.hits,
                c.misses,
                c.hit_rate(),
                c.miss_bytes,
                c.modeled_miss_cost_ns,
                report.speculative_requests,
                report.evicted,
                report.resident_bytes,
                report.resident_count,
                report.history_count
            );
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{Event, Kind};
    #[test]
    fn speculative_access_is_not_a_demand_hit() {
        let a = Event {
            timestamp_ns: 0,
            stream_id: 0,
            key: 42,
            charged_bytes: 4096,
            kind: Kind::Metadata,
            refill_cost_ns: 100,
            speculative: true,
        };
        let b = Event {
            timestamp_ns: 1,
            speculative: false,
            ..a
        };
        let result = replay_with_policy(&[a, b], make_policy("lru", 4096).unwrap());
        assert_eq!(result.total().requests, 1);
        assert_eq!(result.total().hits, 1);
        assert_eq!(result.speculative_requests, 1);
    }
}
