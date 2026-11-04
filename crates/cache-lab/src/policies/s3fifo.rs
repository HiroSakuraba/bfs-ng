use std::collections::{HashMap, VecDeque};
use super::{fits, Outcome, Policy};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Queue { Small, Main }

#[derive(Debug, Clone, Copy)]
struct Entry {
    bytes: u64,
    freq: u8, // capped 2-bit second-chance count
}

/// S3-FIFO-inspired, byte-capacity trace model with Small/Main/Ghost queues.
/// Compared with the original paper's implementation this lab uses full u64
/// ghost keys, an O(n) ghost deletion and a byte-equivalent ghost-history cap.
/// It models the algorithm, not its multithreaded throughput or metadata cost.
pub struct S3Fifo {
    cap: u64,
    used: u64,
    small_used: u64,
    main_used: u64,
    entries: HashMap<u64, Entry>,
    small: VecDeque<u64>,
    main: VecDeque<u64>,
    ghosts: HashMap<u64, u64>,
    ghost_queue: VecDeque<u64>,
    ghost_bytes: u128,
}

impl S3Fifo {
    pub fn new(cap: u64) -> Self {
        Self {
            cap, used: 0, small_used: 0, main_used: 0,
            entries: HashMap::new(), small: VecDeque::new(),
            main: VecDeque::new(), ghosts: HashMap::new(),
            ghost_queue: VecDeque::new(), ghost_bytes: 0,
        }
    }

    fn forget_ghost(&mut self, key: u64) -> bool {
        if let Some(bytes) = self.ghosts.remove(&key) {
            self.ghost_bytes -= u128::from(bytes);
            if let Some(p) = self.ghost_queue.iter().position(|&x| x == key) {
                self.ghost_queue.remove(p);
            }
            true
        } else { false }
    }

    fn remember_ghost(&mut self, key: u64, bytes: u64) {
        self.forget_ghost(key);
        self.ghosts.insert(key, bytes);
        self.ghost_queue.push_back(key);
        self.ghost_bytes += u128::from(bytes);
        // Account for the logical size of evicted objects to bound history.
        // Actual in-memory ghost keys also consume RAM; report separately.
        while self.ghost_bytes > u128::from(self.cap) {
            let old = self.ghost_queue.pop_front().expect("ghost queue nonempty");
            let sz = self.ghosts.remove(&old).expect("ghost map and queue agree");
            self.ghost_bytes -= u128::from(sz);
        }
    }

    fn evict_one(&mut self) -> bool {
        // Return true only if an actual resident was displaced. A Small ->
        // Main promotion is not an eviction and doesn't free space.
        let main_target = self.cap - self.cap / 10;
        let choose_main = !self.main.is_empty()
            && (self.main_used > main_target || self.small.is_empty());
        if choose_main {
            let key = self.main.pop_front().expect("main nonempty");
            let entry = self.entries.get_mut(&key).expect("main index valid");
            if entry.freq > 0 {
                entry.freq -= 1;
                self.main.push_back(key);
                return false;
            }
            let sz = entry.bytes;
            self.entries.remove(&key);
            self.main_used -= sz;
            self.used -= sz;
            true
        } else if let Some(key) = self.small.pop_front() {
            let entry = self.entries.get_mut(&key).expect("small index valid");
            let sz = entry.bytes;
            self.small_used -= sz;
            if entry.freq > 0 {
                entry.freq = 0;
                self.main_used += sz;
                self.main.push_back(key);
                false
            } else {
                self.entries.remove(&key);
                self.used -= sz;
                self.remember_ghost(key, sz);
                true
            }
        } else {
            // Used bytes can only be nonzero if at least one queue has data.
            assert_eq!(self.used, 0, "nonzero bytes without resident queue");
            false
        }
    }
}

impl Policy for S3Fifo {
    fn name(&self) -> &'static str { "s3fifo" }
    fn capacity(&self) -> u64 { self.cap }
    fn used(&self) -> u64 { self.used }
    fn resident_count(&self) -> usize { self.entries.len() }
    fn history_count(&self) -> usize { self.ghosts.len() }

    fn access(&mut self, key: u64, bytes: u64, speculative: bool) -> Outcome {
        if let Some(e) = self.entries.get_mut(&key) {
            if !speculative { e.freq = (e.freq + 1).min(3); }
            return Outcome { hit: true, admitted: false, evicted: 0 };
        }
        if bytes > self.cap || bytes == 0 { return Outcome::default(); }
        let ghost_hit = self.forget_ghost(key);
        let mut evicted = 0;
        while !fits(self.used, bytes, self.cap) {
            if self.evict_one() { evicted += 1; }
        }
        let queue = if ghost_hit { Queue::Main } else { Queue::Small };
        match queue {
            Queue::Small => { self.small.push_back(key); self.small_used += bytes; }
            Queue::Main => { self.main.push_back(key); self.main_used += bytes; }
        }
        self.entries.insert(key, Entry { bytes, freq: 0 });
        self.used += bytes;
        debug_assert_eq!(self.used, self.small_used + self.main_used);
        Outcome { hit: false, admitted: true, evicted }
    }
}
