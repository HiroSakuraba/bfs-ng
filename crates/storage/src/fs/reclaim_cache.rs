//! F2.R4: in-handle certified allocator state and bounded deferred-free sweeping.
//!
//! The allocator cache is NEVER loaded from untrusted bytes alone. Its first
//! incarnation comes from an independent two-F0-root reachability scrub.
//! Thereafter a single-writer handle advances that certificate only after a
//! successful F0 root publication. Failed/poisoned transactions do not advance
//! it; reopening requires another full certification scan.
//!
//! Safe free-block selection uses only `ready`. `pending` is rooted in the
//! recoverable fallback and is never considered eligible for the current
//! transaction. `deferred` is physically unreferenced, but held until a
//! bounded sweep verifies the pair of new candidate ownership sets.
use super::{Error, Result};
use std::collections::BTreeSet;

/// Maximum number of retired physical blocks promoted per successful
/// generation. Using a small fixed budget deliberately makes checkpoint
/// resumption observable in tests.
pub(super) const SWEEP_BUDGET: usize = 32;

#[derive(Clone, Debug)]
pub(super) struct Certificate {
    pub generation: u64,
    pub current_owned: BTreeSet<u64>,
    pub fallback_owned: BTreeSet<u64>,
    pub ready: BTreeSet<u64>,
    pub pending: BTreeSet<u64>,
    pub deferred: BTreeSet<u64>,
    pub sweep_cursor: u64,
}

pub(super) struct Sweep {
    pub promoted: BTreeSet<u64>,
    pub deferred: BTreeSet<u64>,
    pub pending: BTreeSet<u64>,
    pub sweep_cursor: u64,
}

impl Certificate {
    /// Compute a deterministic, persistent, bounded physical-address sweep.
    /// Freshly released blocks may be promoted in the same generation that
    /// rotates out their fallback F0 root, but only *after publication*, so
    /// they are never inserted into the current Builder's free pool.
    pub fn rotate(
        &self,
        new_owned_without_journal: &BTreeSet<u64>,
        new_cursor: u64,
    ) -> Result<Sweep> {
        let generation = self.generation.checked_add(1).ok_or(Error::GenerationOverflow)?;
        let rotated: BTreeSet<u64> = self
            .fallback_owned
            .difference(&self.current_owned)
            .copied()
            .collect();
        if rotated != self.pending || !self.ready.is_disjoint(&self.deferred) {
            return Err(Error::CorruptTree);
        }
        let mut deferred: BTreeSet<u64> =
            self.deferred.union(&rotated).copied().collect();
        let pending: BTreeSet<u64> = self
            .current_owned
            .difference(new_owned_without_journal)
            .copied()
            .collect();
        if !deferred.is_disjoint(&pending)
            || !deferred.is_disjoint(&self.ready)
            || deferred.iter().any(|&id| {
                id < new_cursor
                    || self.current_owned.contains(&id)
                    || new_owned_without_journal.contains(&id)
            })
            || pending.iter().any(|&id| id < new_cursor)
        {
            return Err(Error::CorruptTree);
        }
        let start = self.sweep_cursor;
        let batch: Vec<u64> = deferred
            .range(start..)
            .chain(deferred.range(..start))
            .take(SWEEP_BUDGET)
            .copied()
            .collect();
        let mut promoted = BTreeSet::new();
        for id in batch {
            deferred.remove(&id);
            promoted.insert(id);
        }
        let sweep_cursor = if deferred.is_empty() {
            0
        } else if let Some(&id) = promoted.last() {
            id.checked_add(1).ok_or(Error::GenerationOverflow)?
        } else {
            start
        };
        // The next root's new pending entries are not reusable at this
        // generation; g+1 is their earliest publication eligibility.
        if generation == u64::MAX {
            return Err(Error::GenerationOverflow);
        }
        Ok(Sweep {
            promoted,
            deferred,
            pending,
            sweep_cursor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_sweep_persists_cursor_and_never_promotes_pending_early() {
        let fallback_owned: BTreeSet<_> = (100..200).collect();
        let current_owned = BTreeSet::new();
        let cache = Certificate {
            generation: 4,
            fallback_owned,
            current_owned,
            ready: BTreeSet::new(),
            pending: (100..200).collect(),
            deferred: BTreeSet::new(),
            sweep_cursor: 0,
        };
        let result = cache.rotate(&BTreeSet::new(), 70).unwrap();
        assert_eq!(result.promoted.len(), SWEEP_BUDGET);
        assert_eq!(result.deferred.len(), 100 - SWEEP_BUDGET);
        assert_eq!(result.sweep_cursor, 132);
        assert!(result.pending.is_empty());
    }

    #[test]
    fn stale_pending_certificate_is_rejected() {
        let cache = Certificate {
            generation: 2,
            fallback_owned: [83].into(),
            current_owned: BTreeSet::new(),
            ready: BTreeSet::new(),
            pending: BTreeSet::new(),
            deferred: BTreeSet::new(),
            sweep_cursor: 0,
        };
        assert!(matches!(
            cache.rotate(&BTreeSet::new(), 70),
            Err(Error::CorruptTree)
        ));
    }
}
