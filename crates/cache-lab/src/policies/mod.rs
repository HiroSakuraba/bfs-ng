//! Cache simulators; private implementation structures are not disk format types.

mod lru;
mod none;
mod s3fifo;
mod wtinylfu;

pub use lru::Lru;
pub use none::NoCache;
pub use s3fifo::S3Fifo;
pub use wtinylfu::WTinyLfu;

/// An access always returns `hit=true` only if the key was already resident
/// before this request. A simulated miss may or may not admit the key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Outcome {
    pub hit: bool,
    pub admitted: bool,
    /// Number of resident objects displaced (not ghost-history expirations).
    pub evicted: u64,
}

pub trait Policy {
    fn name(&self) -> &'static str;
    fn capacity(&self) -> u64;
    fn used(&self) -> u64;
    fn resident_count(&self) -> usize;
    /// `speculative` means prefetch, not a real demand touch. It can populate
    /// the cache but must not increase demand-frequency estimates.
    fn access(&mut self, key: u64, bytes: u64, speculative: bool) -> Outcome;
    fn history_count(&self) -> usize { 0 }
}

pub fn make_policy(name: &str, capacity: u64) -> Result<Box<dyn Policy>, String> {
    match name.to_ascii_lowercase().as_str() {
        "none" | "no-cache" => Ok(Box::new(NoCache::new(capacity))),
        "lru" => Ok(Box::new(Lru::new(capacity))),
        "s3fifo" | "s3-fifo" => Ok(Box::new(S3Fifo::new(capacity))),
        "wtinylfu" | "w-tinylfu" => Ok(Box::new(WTinyLfu::new(capacity))),
        _ => Err(format!("unknown policy '{name}'; valid: none,lru,s3fifo,wtinylfu")),
    }
}

/// Checked addition avoids overflowing the capacity test on malicious traces.
#[inline]
pub(crate) fn fits(used: u64, incoming: u64, limit: u64) -> bool {
    used.checked_add(incoming).is_some_and(|v| v <= limit)
}
