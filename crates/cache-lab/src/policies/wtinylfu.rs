use std::collections::{HashMap, VecDeque};
use super::{Outcome, Policy};

const WIDTH: usize = 2048;
const ROWS: usize = 4;
const SEEDS: [u64; ROWS] = [
    0x9e3779b97f4a7c15, 0xbf58476d1ce4e5b9,
    0x94d049bb133111eb, 0xd6e8feb86659fd93,
];

fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58476d1ce4e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

/// Four-row, saturating count-min frequency sketch with periodic halving.
/// Deliberately simple: no doorkeeper bloom filter or hill-climbing window.
struct FrequencySketch {
    counters: Vec<u8>,
    since_aging: u64,
}

impl FrequencySketch {
    fn new() -> Self {
        Self { counters: vec![0; WIDTH * ROWS], since_aging: 0 }
    }
    fn indexes(key: u64) -> [usize; ROWS] {
        let mut idx = [0; ROWS];
        for (row, seed) in SEEDS.iter().enumerate() {
            idx[row] = row * WIDTH + (mix(key ^ seed) as usize % WIDTH);
        }
        idx
    }
    fn record(&mut self, key: u64) {
        for index in Self::indexes(key) {
            self.counters[index] = self.counters[index].saturating_add(1).min(15);
        }
        self.since_aging += 1;
        if self.since_aging >= (WIDTH as u64) * 10 {
            for value in &mut self.counters { *value >>= 1; }
            self.since_aging = 0;
        }
    }
    fn estimate(&self, key: u64) -> u8 {
        Self::indexes(key).iter().map(|&idx| self.counters[idx]).min().unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Region { Window, Probation, Protected }
#[derive(Debug, Clone, Copy)]
struct Entry { bytes: u64, region: Region }

/// Window TinyLFU **research variant**: 1% LRU window (min 4 KiB), segmented
/// LRU main (probation + 80%-target protected), aged frequency sketch.
/// It does not implement Caffeine's concurrency or adaptive window tuning.
pub struct WTinyLfu {
    cap: u64,
    used: u64,
    window_used: u64,
    protected_used: u64,
    entries: HashMap<u64, Entry>,
    window: VecDeque<u64>,
    probation: VecDeque<u64>,
    protected: VecDeque<u64>,
    sketch: FrequencySketch,
}

impl WTinyLfu {
    pub fn new(cap: u64) -> Self {
        Self {
            cap, used: 0, window_used: 0, protected_used: 0,
            entries: HashMap::new(), window: VecDeque::new(),
            probation: VecDeque::new(), protected: VecDeque::new(),
            sketch: FrequencySketch::new(),
        }
    }
    fn window_target(&self) -> u64 { self.cap.min((self.cap / 100).max(4096)) }
    fn main_target(&self) -> u64 { self.cap - self.window_target() }
    fn protected_target(&self) -> u64 { self.main_target() * 8 / 10 }

    fn move_to_back(queue: &mut VecDeque<u64>, key: u64) {
        let at = queue.iter().position(|&item| item == key).expect("queue index valid");
        queue.remove(at);
        queue.push_back(key);
    }
    fn remove_entry(&mut self, key: u64) -> u64 {
        let entry = self.entries.remove(&key).expect("entry must exist");
        let q = match entry.region {
            Region::Window => &mut self.window,
            Region::Probation => &mut self.probation,
            Region::Protected => &mut self.protected,
        };
        let pos = q.iter().position(|&x| x == key).expect("queue index valid");
        q.remove(pos);
        self.used -= entry.bytes;
        if entry.region == Region::Window { self.window_used -= entry.bytes; }
        if entry.region == Region::Protected { self.protected_used -= entry.bytes; }
        entry.bytes
    }
    fn rebalance_protected(&mut self) {
        while self.protected_used > self.protected_target() {
            let Some(key) = self.protected.pop_front() else { break };
            let entry = self.entries.get_mut(&key).expect("protected entry exists");
            self.protected_used -= entry.bytes;
            entry.region = Region::Probation;
            self.probation.push_back(key);
        }
    }
    fn touch_resident(&mut self, key: u64) {
        let e = *self.entries.get(&key).expect("resident key");
        match e.region {
            Region::Window => Self::move_to_back(&mut self.window, key),
            Region::Protected => Self::move_to_back(&mut self.protected, key),
            Region::Probation => {
                let pos = self.probation.iter().position(|&x| x == key).unwrap();
                self.probation.remove(pos);
                self.entries.get_mut(&key).unwrap().region = Region::Protected;
                self.protected.push_back(key);
                self.protected_used += e.bytes;
                self.rebalance_protected();
            }
        }
    }
    fn consider_main(&mut self, key: u64, bytes: u64) -> (bool, u64) {
        if bytes > self.main_target() { return (false, 0); }
        let main_used = self.used - self.window_used;
        let mut need = main_used.saturating_add(bytes).saturating_sub(self.main_target());
        let candidate_frequency = self.sketch.estimate(key);
        // Dry-run selection: never evict other entries if admission eventually
        // fails. Prioritize probation LRU, then protected LRU only if necessary.
        let mut victims: Vec<u64> = Vec::new();
        if need > 0 {
            for &other in self.probation.iter().chain(self.protected.iter()) {
                if candidate_frequency <= self.sketch.estimate(other) {
                    return (false, 0);
                }
                victims.push(other);
                need = need.saturating_sub(self.entries.get(&other).unwrap().bytes);
                if need == 0 { break; }
            }
            if need > 0 { return (false, 0); }
        }
        let mut evicted = 0;
        for victim in victims {
            self.remove_entry(victim);
            evicted += 1;
        }
        self.entries.insert(key, Entry { bytes, region: Region::Probation });
        self.probation.push_back(key);
        self.used += bytes;
        (true, evicted)
    }
}

impl Policy for WTinyLfu {
    fn name(&self) -> &'static str { "wtinylfu" }
    fn capacity(&self) -> u64 { self.cap }
    fn used(&self) -> u64 { self.used }
    fn resident_count(&self) -> usize { self.entries.len() }

    fn access(&mut self, key: u64, bytes: u64, speculative: bool) -> Outcome {
        if !speculative { self.sketch.record(key); }
        if self.entries.contains_key(&key) {
            if !speculative { self.touch_resident(key); }
            return Outcome { hit: true, admitted: false, evicted: 0 };
        }
        if bytes == 0 || bytes > self.cap { return Outcome::default(); }
        self.entries.insert(key, Entry { bytes, region: Region::Window });
        self.window.push_back(key);
        self.window_used += bytes;
        self.used += bytes;
        let mut evicted = 0;
        while self.window_used > self.window_target() {
            let candidate = *self.window.front().expect("window over target");
            let size = self.remove_entry(candidate);
            let (_admitted, evicted_main) = self.consider_main(candidate, size);
            evicted += evicted_main;
            // Moving an object out of the window and dropping it or admitting
            // it to main counts as one eviction only if it was NOT retained.
            if !self.entries.contains_key(&candidate) { evicted += 1; }
        }
        debug_assert!(self.used <= self.cap);
        Outcome { hit: false, admitted: self.entries.contains_key(&key), evicted }
    }
}
