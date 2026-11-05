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
    head_generation: u64,
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
        head_generation: u64,
        current: OwnerGraph,
        both_root_owners: &BTreeSet<u64>,
        fallback_global_only: BTreeSet<u64>,
    ) -> Result<Self> {
        if head_generation == 0 || !current.pages.is_subset(both_root_owners) {
            return Err(Error::CorruptTree);
        }
        let fallback_pages = both_root_owners
            .difference(&current.pages)
            .copied()
            .collect();
        let fallback_only = OwnerGraph::new(fallback_pages, fallback_global_only)?;
        Ok(Self {
            head_generation,
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


    /// Canonical, bounded external checkpoint of the independently audited
    /// current and fallback-only owner sets. It is a *hint*, never disk
    /// authority: recovery must reconstruct both F0 graphs and compare the
    /// checkpoint exactly before any cached allocation can be enabled.
    pub(super) fn encode_checkpoint(
        &self,
        volume_id: u128,
        manifest_crc: u32,
    ) -> Result<Vec<u8>> {
        const MAGIC: &[u8; 8] = b"BFSNGAC1";
        const MAX_ENTRIES: usize = 1_048_576;
        let sets = [
            &self.current.pages,
            &self.current.global,
            &self.fallback_only.pages,
            &self.fallback_only.global,
        ];
        let entries: usize = sets.iter().map(|s| s.len()).sum();
        if entries > MAX_ENTRIES || volume_id == 0 || self.head_generation == 0 {
            return Err(Error::TooLarge);
        }
        let mut bytes = vec![0_u8; 64];
        bytes[0..8].copy_from_slice(MAGIC);
        bytes[8..12].copy_from_slice(&1_u32.to_le_bytes());
        bytes[16..32].copy_from_slice(&volume_id.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.head_generation.to_le_bytes());
        bytes[40..44].copy_from_slice(&manifest_crc.to_le_bytes());
        for (index, set) in sets.iter().enumerate() {
            let len = u32::try_from(set.len()).map_err(|_| Error::TooLarge)?;
            bytes[48 + index * 4..52 + index * 4].copy_from_slice(&len.to_le_bytes());
            for address in set.iter() {
                bytes.extend_from_slice(&address.to_le_bytes());
            }
        }
        let crc = crate::volume::crc32c(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        Ok(bytes)
    }

    /// Parse a sidecar checkpoint using a canonical, role-aware codec and
    /// compare it to an *independently re-audited* owner graph. Even a valid
    /// CRC and matching F0 generation alone are insufficient for reuse.
    pub(super) fn verify_checkpoint(
        &self,
        bytes: &[u8],
        volume_id: u128,
        manifest_crc: u32,
        total_blocks: u64,
    ) -> Result<()> {
        const MAX_ENTRIES: usize = 1_048_576;
        if bytes.len() < 68
            || &bytes[0..8] != b"BFSNGAC1"
            || bytes[8..12] != 1_u32.to_le_bytes()
            || bytes[12..16] != [0; 4]
            || bytes[16..32] != volume_id.to_le_bytes()
            || bytes[32..40] != self.head_generation.to_le_bytes()
            || bytes[40..44] != manifest_crc.to_le_bytes()
            || bytes[44..48] != [0; 4]
        {
            return Err(Error::CorruptTree);
        }
        let mut lengths = [0_usize; 4];
        for (index, len) in lengths.iter_mut().enumerate() {
            let at = 48 + index * 4;
            *len = u32::from_le_bytes(
                bytes[at..at + 4].try_into().map_err(|_| Error::CorruptTree)?,
            ) as usize;
        }
        let count: usize = lengths.iter().copied().sum();
        if count > MAX_ENTRIES
            || bytes.len() != 64 + count * 8 + 4
            || bytes[bytes.len() - 4..]
                != crate::volume::crc32c(&bytes[..bytes.len() - 4]).to_le_bytes()
        {
            return Err(Error::CorruptTree);
        }
        let expected = [
            &self.current.pages,
            &self.current.global,
            &self.fallback_only.pages,
            &self.fallback_only.global,
        ];
        let mut offset = 64;
        for (length, expected_set) in lengths.iter().zip(expected) {
            let mut previous = None;
            let mut decoded = BTreeSet::new();
            for _ in 0..*length {
                let address = u64::from_le_bytes(
                    bytes[offset..offset + 8].try_into().map_err(|_| Error::CorruptTree)?,
                );
                if address <= crate::volume::ROOT_PAYLOAD_LAST
                    || address >= total_blocks
                    || previous.is_some_and(|last| address <= last)
                {
                    return Err(Error::CorruptTree);
                }
                decoded.insert(address);
                previous = Some(address);
                offset += 8;
            }
            if &decoded != expected_set {
                return Err(Error::CorruptTree);
            }
        }
        Ok(())
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
        if new_generation
            != self
                .head_generation
                .checked_add(1)
                .ok_or(Error::GenerationOverflow)?
            || arena_end > total_blocks
        {
            return Err(Error::CorruptTree);
        }
        let before = self.protected();
        if !staged.is_disjoint(&before)
            || staged.iter().any(|&address| {
                address <= crate::volume::ROOT_PAYLOAD_LAST || address >= total_blocks
            })
        {
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
            head_generation: new_generation,
            current: candidate,
            fallback_only: OwnerGraph::new(prior_current_only, prior_global_only)?,
        };
        let after = next.protected();
        let retired: BTreeSet<u64> = before.difference(&after).copied().collect();

        // First prove that the entire Free/Dead map has valid disjoint runs
        // and that Free contains no owner in the next two-root union.
        // This makes a single physical-start lookup sound for Dead witnesses.
        validate_space_pair(free, dead, &after, new_generation, total_blocks)?;
        let pending_by_start: BTreeMap<u64, &DeadRun> =
            dead.values().map(|run| (run.start, run)).collect();
        // A forgotten kill leaks space; a false Free kill destroys data.
        // Require a persisted DeadList witness for each disappearing block.
        // The 64-block nonrecursive allocator arena has its own lifetime.
        for &address in &retired {
            if address < arena_end {
                continue;
            }
            let run = pending_by_start
                .range(..=address)
                .next_back()
                .map(|(_, run)| *run)
                .ok_or(Error::CorruptTree)?;
            let expected_global = self.fallback_only.global.contains(&address);
            if address >= run.start + u64::from(run.blocks)
                || run.dead_generation > new_generation
                || (run.domain == super::space_index::SpaceDomain::Global) != expected_global
            {
                return Err(Error::CorruptTree);
            }
        }
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
            5,
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
            5,
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
        let old = TwoRootOwners::from_audit(
            5,
            graph(&[80], &[80]),
            &BTreeSet::from([80]),
            BTreeSet::new(),
        )
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
    fn checkpoint_roundtrips_but_rejects_wrong_volume_crc_and_owner_graph() {
        let owner = TwoRootOwners::from_audit(
            5,
            graph(&[80, 81], &[81]),
            &BTreeSet::from([80, 81, 90]),
            BTreeSet::from([90]),
        )
        .unwrap();
        let bytes = owner.encode_checkpoint(55, 0x1234).unwrap();
        owner.verify_checkpoint(&bytes, 55, 0x1234, 128).unwrap();
        assert!(owner.verify_checkpoint(&bytes, 56, 0x1234, 128).is_err());
        assert!(owner.verify_checkpoint(&bytes, 55, 0x4567, 128).is_err());
        assert!(owner.verify_checkpoint(&bytes, 55, 0x1234, 90).is_err());
        let mut corrupted = bytes.clone();
        corrupted[64] ^= 1;
        assert!(owner.verify_checkpoint(&corrupted, 55, 0x1234, 128).is_err());
        let different = TwoRootOwners::from_audit(
            5,
            graph(&[80, 81], &[81]),
            &BTreeSet::from([80, 81, 91]),
            BTreeSet::from([91]),
        )
        .unwrap();
        assert!(different.verify_checkpoint(&bytes, 55, 0x1234, 128).is_err());
    }

    #[test]
    fn stale_or_skipped_f0_generation_refuses_the_certificate() {
        let old =
            TwoRootOwners::from_audit(5, graph(&[80], &[]), &BTreeSet::from([80]), BTreeSet::new())
                .unwrap();
        for incorrect_generation in [5, 7] {
            assert!(
                old.transition(
                    graph(&[80], &[]),
                    &BTreeSet::new(),
                    &BTreeMap::new(),
                    &BTreeMap::new(),
                    incorrect_generation,
                    128,
                    69,
                )
                .is_err()
            );
        }
        assert!(
            old.transition(
                graph(&[80], &[]),
                &BTreeSet::from([4]),
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
    fn rotation_union_equals_full_reference_for_many_generations() {
        let mut previous = graph(&[80, 81, 90], &[]);
        let mut latest = graph(&[80, 81, 100], &[]);
        for i in 0..128_u64 {
            let old_union: BTreeSet<u64> = previous.pages.union(&latest.pages).copied().collect();
            let ledger =
                TwoRootOwners::from_audit(i + 9, latest.clone(), &old_union, BTreeSet::new())
                    .unwrap();
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
