use super::{Outcome, Policy};

pub struct NoCache {
    capacity: u64,
}

impl NoCache {
    pub fn new(capacity: u64) -> Self {
        Self { capacity }
    }
}

impl Policy for NoCache {
    fn name(&self) -> &'static str {
        "none"
    }
    fn capacity(&self) -> u64 {
        self.capacity
    }
    fn used(&self) -> u64 {
        0
    }
    fn resident_count(&self) -> usize {
        0
    }
    fn access(&mut self, _key: u64, _bytes: u64, _speculative: bool) -> Outcome {
        Outcome::default()
    }
}
