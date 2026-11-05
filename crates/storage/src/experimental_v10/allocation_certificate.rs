//! Experimental in-handle F5.3 allocation certificate.
//!
//! In the single-writer model a successful F0 commit rotates the previously
//! certified head into the fallback slot. Neither old root needs to be
//! re-traversed to compute its ownership: cache an independently audited pair,
//! then advance it using the independently verified, immutable candidate graph.
//! This is *not* a durable Accounted recovery mode: open must still audit both
//! on-disk F0 graphs, and the new candidate is still verified in full.
use super::space_index::{DeadRun, FreeRun, validate_space_pair};
use crate::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub(super) struct OwnerGraph {
    pub pages: BTreeSet<u64>,
    pub global: BTreeSet<u64>,
}

impl OwnerGraph {
    pub fn new(pages: BTreeSet<u64>, global: BTreeSet<u64>) -> Result<Self> {
        if !global.is_subset(&pages) {
            return Err(Error::CorruptTree);
        }
        Ok(Self { pages, global })
    }
}

/// The fallback-only projection is enough: pages also present in the
/// current graph remain protected when the F0 pair rotates.
pub(super) struct TwoRootOwners {
    current: OwnerGraph,
    fallback_only: OwnerGraph,
}

pub(super) struct OwnershipTransition {
    pub next: TwoRootOwners,
    pub retired: BTreeSet<u64>,
    pub allocated: BTreeSet<u64>,
}

impl TwoRootOwners {
    /// One-time seed from the authoritative full audit at open/format.
    pub fn from_audit(
        current: OwnerGraph,
        both_root_owners: &BTreeSet<u64>,
        fallback_global_only: BTreeSet<u64>,
    ) -> Result<Self> {
        if !current.pages.is_subset(both_root_owners) {
            return Err(Error::CorruptTree);
        }
        let fallback_pages = both_root_owners
            .difference(&current.pages)
            .copied()
            .collect();
        let fallback_only = OwnerGraph::new(fallback_pages, fallback_global_only)?;
        Ok(Self {
            current,
            fallback_only,
        })
    }

    pub fn protected(&self) -> BTreeSet<u64> {
        self.current
            .pages
            .union(&self.fallback_only.pages)
            .copied()
            .collect()
    }

    /// Reuse certificate checked before the real F0 publication. The caller
    /// supplies the entire staged address set AND a graph independently
    /// verified against the staged overlay. A newly reachable address must
    /// have been staged, and no staged address may overwrite either old F0.
    /// Every data-region block that loses its last F0 reference needs a
    /// persisted DeadList witness. Index-arena pages have their own fence.
    #[allow(clippy::too_many_arguments)]
    pub fn transition(
        &self,
        candidate: OwnerGraph,
        staged: &BTreeSet<u64>,
        free: &BTreeMap<u64, FreeRun>,
        dead: &BTreeMap<(u64, u64), DeadRun>,
        new_generation: u64,
        total_blocks: u64,
        arena_end: u64,
    ) -> Result<OwnershipTransition> {
        if new_generation == 0 || arena_end > total_blocks {
            return Err(Error::Geometry);
        }
        let before = self.protected();
        if !staged.is_disjoint(&before) || staged.iter().any(|&address| address >= total_blocks) {
            return Err(Error::CorruptTree);
        }
        let allocated: BTreeSet<u64> = candidate.pages.difference(&before).copied().collect();
        if !allocated.is_subset(staged) {
            return Err(Error::CorruptTree);
        }
        // Reinterpreting an immutable page as a different ownership domain
        // would invalidate its retirement rule.
        for address in self.current.pages.intersection(&candidate.pages) {
            if self.current.global.contains(address) != candidate.global.contains(address) {
                return Err(Error::CorruptTree);
            }
        }
        for address in self.fallback_only.pages.intersection(&candidate.pages) {
            if self.fallback_only.global.contains(address) != candidate.global.contains(address) {
                return Err(Error::CorruptTree);
            }
        }

        let prior_current_only: BTreeSet<u64> = self
            .current
            .pages
            .difference(&candidate.pages)
            .copied()
            .collect();
        let prior_global_only: BTreeSet<u64> = self
            .current
            .global
            .difference(&candidate.pages)
            .copied()
            .collect();
        let next = Self {
            current: candidate,
            fallback_only: OwnerGraph::new(prior_current_only, prior_global_only)?,
        };
        let after = next.protected();
        let retired: BTreeSet<u64> = before.difference(&after).copied().collect();

        // A forgotten kill leaks space; a false Free kill destroys data.
        // Compare every disappearing data-region address to the exact,
        // published DeadList. The arena is deliberately out of the index.
        for &address in &retired {
            if address < arena_end {
                continue;
            }
            let global = self.fallback_only.global.contains(&address);
            if !dead.values().any(|run| {
                run.start <= address
                    && address < run.start + u64::from(run.blocks)
                    && (run.domain == super::space_index::SpaceDomain::Global) == global
            }) {
                return Err(Error::CorruptTree);
            }
        }
        // The new Free tree must not alias any new-current or retained-old
        // current owner. Epochs are not authority to release an owned block.
        validate_space_pair(free, dead, &after, new_generation, total_blocks)?;
        Ok(OwnershipTransition {
            next,
            retired,
            allocated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::space_index::SpaceDomain;
    use super::*;

    fn graph(pages: &[u64], globals: &[u64]) -> OwnerGraph {
        OwnerGraph::new(
            pages.iter().copied().collect(),
            globals.iter().copied().collect(),
        )
        .unwrap()
    }

    #[test]
    fn rotates_current_to_fallback_without_losing_snapshot_owners() {
        let old = TwoRootOwners::from_audit(
            graph(&[80, 81, 82], &[]),
            &BTreeSet::from([80, 81, 82, 90]),
            BTreeSet::new(),
        )
        .unwrap();
        let dead = BTreeMap::from([(
            (5, 90),
            DeadRun {
                dead_generation: 5,
                start: 90,
                blocks: 1,
                eligible_after: 7,
                domain: SpaceDomain::Snapshot,
            },
        )]);
        let moved = old
            .transition(
                graph(&[80, 81, 82, 100], &[]),
                &BTreeSet::from([100]),
                &BTreeMap::new(),
                &dead,
                6,
                128,
                69,
            )
            .unwrap();
        assert_eq!(moved.retired, BTreeSet::from([90]));
        assert_eq!(moved.allocated, BTreeSet::from([100]));
        assert_eq!(moved.next.protected(), BTreeSet::from([80, 81, 82, 100]));
    }

    #[test]
    fn rejects_unstaged_new_page_and_missing_retirement() {
        let old = TwoRootOwners::from_audit(
            graph(&[80], &[]),
            &BTreeSet::from([80, 90]),
            BTreeSet::new(),
        )
        .unwrap();
        let candidate = graph(&[80, 100], &[]);
        assert!(
            old.transition(
                candidate.clone(),
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                6,
                128,
                69,
            )
            .is_err()
        );
        assert!(
            old.transition(
                candidate,
                &BTreeSet::from([100]),
                &BTreeMap::new(),
                &BTreeMap::new(),
                6,
                128,
                69,
            )
            .is_err()
        );
    }

    #[test]
    fn refuses_global_role_swap_and_free_claim_on_retained_f0_owner() {
        let old =
            TwoRootOwners::from_audit(graph(&[80], &[80]), &BTreeSet::from([80]), BTreeSet::new())
                .unwrap();
        assert!(
            old.transition(
                graph(&[80], &[]),
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                6,
                128,
                69,
            )
            .is_err()
        );
        let free = BTreeMap::from([(
            80,
            FreeRun {
                start: 80,
                blocks: 1,
                eligible_after: 4,
            },
        )]);
        assert!(
            old.transition(
                graph(&[80], &[80]),
                &BTreeSet::new(),
                &free,
                &BTreeMap::new(),
                6,
                128,
                69,
            )
            .is_err()
        );
    }

    #[test]
    fn rotation_union_equals_full_reference_for_many_generations() {
        let mut previous = graph(&[80, 81, 90], &[]);
        let mut latest = graph(&[80, 81, 100], &[]);
        for i in 0..128_u64 {
            let old_union: BTreeSet<u64> = previous.pages.union(&latest.pages).copied().collect();
            let ledger =
                TwoRootOwners::from_audit(latest.clone(), &old_union, BTreeSet::new()).unwrap();
            let next_address = 200 + i;
            let mut next = latest.clone();
            next.pages.insert(next_address);
            next.pages.remove(&100);
            let expected: BTreeSet<u64> = latest.pages.union(&next.pages).copied().collect();
            let lost: BTreeSet<u64> = old_union.difference(&expected).copied().collect();
            let dead: BTreeMap<(u64, u64), DeadRun> = lost
                .iter()
                .map(|&address| {
                    (
                        (i + 10, address),
                        DeadRun {
                            dead_generation: i + 10,
                            start: address,
                            blocks: 1,
                            eligible_after: i + 12,
                            domain: SpaceDomain::Snapshot,
                        },
                    )
                })
                .collect();
            let delta: BTreeSet<u64> = next.pages.difference(&old_union).copied().collect();
            let result = ledger
                .transition(
                    next.clone(),
                    &delta,
                    &BTreeMap::new(),
                    &dead,
                    i + 10,
                    512,
                    69,
                )
                .unwrap();
            assert_eq!(result.next.protected(), expected);
            assert_eq!(result.retired, lost);
            previous = latest;
            latest = next;
        }
    }
}
