use super::{Outcome, Policy, fits};
use std::collections::HashMap;

/// Simple O(number_of_resident_objects) reference model, not fast-path code.
/// An LRU hit always becomes the newest entry, even on a speculative access.
pub struct Lru {
    cap: u64,
    used: u64,
    tick: u64,
    residents: HashMap<u64, (u64, u64)>, // key -> (bytes, last_touch)
}

impl Lru {
    pub fn new(capacity: u64) -> Self {
        Self {
            cap: capacity,
            used: 0,
            tick: 0,
            residents: HashMap::new(),
        }
    }
}

impl Policy for Lru {
    fn name(&self) -> &'static str {
        "lru"
    }
    fn capacity(&self) -> u64 {
        self.cap
    }
    fn used(&self) -> u64 {
        self.used
    }
    fn resident_count(&self) -> usize {
        self.residents.len()
    }

    fn access(&mut self, key: u64, bytes: u64, _speculative: bool) -> Outcome {
        self.tick = self.tick.checked_add(1).expect("trace exceeds u64 clock");
        if let Some((_, last)) = self.residents.get_mut(&key) {
            *last = self.tick;
            return Outcome {
                hit: true,
                admitted: false,
                evicted: 0,
            };
        }
        if bytes > self.cap || bytes == 0 {
            return Outcome::default();
        }
        let mut evicted = 0;
        while !fits(self.used, bytes, self.cap) {
            let (&oldest, &(size, _)) = self
                .residents
                .iter()
                .min_by_key(|item| (item.1.1, *item.0))
                .expect("non-empty cache must have a victim");
            self.residents.remove(&oldest);
            self.used -= size;
            evicted += 1;
        }
        self.residents.insert(key, (bytes, self.tick));
        self.used += bytes;
        Outcome {
            hit: false,
            admitted: true,
            evicted,
        }
    }
}
