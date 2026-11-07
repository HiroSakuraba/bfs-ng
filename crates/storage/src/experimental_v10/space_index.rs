//! Persistent CoW free-space and delayed-retirement index for experimental v10.
//!
//! FREE contains absolute-epoch runs; DEAD contains pending retires with an
//! ownership domain. Both are independently BP/CRC-authenticated B+trees.
//! No tree entry grants allocation authority without an independently checked
//! union of all recoverable F0 roots. This module stages data only: it never
//! writes or publishes roots.
use super::{BlockPointer, Item, MAX_LEVEL, PAGE_HEADER_BYTES, Page, TreeId, read_page};
use crate::volume::ROOT_PAYLOAD_LAST;
use crate::{BLOCK_SIZE, BlockDevice, Error, Result};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_SPACE_PAGES: usize = 8192;
pub const MAX_SPACE_RUNS: usize = 131072;
pub const MAX_RETIRE_STEP: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpaceDomain {
    Snapshot = 1,
    Global = 2,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FreeRun {
    pub start: u64,
    pub blocks: u32,
    pub eligible_after: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeadRun {
    pub dead_generation: u64,
    pub start: u64,
    pub blocks: u32,
    pub eligible_after: u64,
    pub domain: SpaceDomain,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceScan {
    pub free: BTreeMap<u64, FreeRun>,
    pub dead: BTreeMap<(u64, u64), DeadRun>,
    pub pages: BTreeSet<u64>,
}
impl SpaceScan {
    fn empty() -> Self {
        Self {
            free: BTreeMap::new(),
            dead: BTreeMap::new(),
            pages: BTreeSet::new(),
        }
    }
}
fn range_ok(start: u64, blocks: u32, total: u64) -> bool {
    start > ROOT_PAYLOAD_LAST
        && blocks > 0
        && start
            .checked_add(u64::from(blocks))
            .is_some_and(|end| end <= total)
}
/// A delayed retirement must preserve a *full* two-generation fence.
/// Saturating arithmetic would incorrectly accept MAX-1 or MAX as a death
/// generation, even though no representable eligible epoch can follow.
fn valid_retirement_epoch(death: u64, eligible_after: u64) -> bool {
    death != 0
        && death
            .checked_add(2)
            .is_some_and(|first_safe| eligible_after >= first_safe)
}
pub fn free_item(run: FreeRun, total: u64) -> Result<(Vec<u8>, Vec<u8>)> {
    if !range_ok(run.start, run.blocks, total) || run.eligible_after == 0 {
        return Err(Error::Geometry);
    }
    let mut value = vec![0; 16];
    value[..4].copy_from_slice(&run.blocks.to_le_bytes());
    value[8..16].copy_from_slice(&run.eligible_after.to_le_bytes());
    Ok((run.start.to_be_bytes().to_vec(), value))
}
pub fn dead_item(run: DeadRun, total: u64) -> Result<(Vec<u8>, Vec<u8>)> {
    if !range_ok(run.start, run.blocks, total)
        || !valid_retirement_epoch(run.dead_generation, run.eligible_after)
    {
        return Err(Error::Geometry);
    }
    let mut key = Vec::new();
    key.extend_from_slice(&run.dead_generation.to_be_bytes());
    key.extend_from_slice(&run.start.to_be_bytes());
    let mut value = vec![0; 16];
    value[..4].copy_from_slice(&run.blocks.to_le_bytes());
    value[4] = run.domain as u8;
    value[8..16].copy_from_slice(&run.eligible_after.to_le_bytes());
    Ok((key, value))
}
fn free_decode(key: &[u8], value: &[u8], total: u64) -> Result<FreeRun> {
    if key.len() != 8 || value.len() != 16 || value[4..8] != [0; 4] {
        return Err(Error::CorruptTree);
    }
    let run = FreeRun {
        start: u64::from_be_bytes(key.try_into().map_err(|_| Error::CorruptTree)?),
        blocks: u32::from_le_bytes(value[..4].try_into().map_err(|_| Error::CorruptTree)?),
        eligible_after: u64::from_le_bytes(
            value[8..16].try_into().map_err(|_| Error::CorruptTree)?,
        ),
    };
    if !range_ok(run.start, run.blocks, total) || run.eligible_after == 0 {
        return Err(Error::CorruptTree);
    }
    Ok(run)
}
fn dead_decode(key: &[u8], value: &[u8], total: u64, generation: u64) -> Result<DeadRun> {
    if key.len() != 16 || value.len() != 16 || value[5..8] != [0; 3] {
        return Err(Error::CorruptTree);
    }
    let domain = match value[4] {
        1 => SpaceDomain::Snapshot,
        2 => SpaceDomain::Global,
        _ => return Err(Error::CorruptTree),
    };
    let run = DeadRun {
        dead_generation: u64::from_be_bytes(key[..8].try_into().map_err(|_| Error::CorruptTree)?),
        start: u64::from_be_bytes(key[8..16].try_into().map_err(|_| Error::CorruptTree)?),
        blocks: u32::from_le_bytes(value[..4].try_into().map_err(|_| Error::CorruptTree)?),
        eligible_after: u64::from_le_bytes(
            value[8..16].try_into().map_err(|_| Error::CorruptTree)?,
        ),
        domain,
    };
    if !range_ok(run.start, run.blocks, total)
        || run.dead_generation > generation
        || !valid_retirement_epoch(run.dead_generation, run.eligible_after)
    {
        return Err(Error::CorruptTree);
    }
    Ok(run)
}
fn no_overlaps(mut runs: Vec<(u64, u32, u64)>, free: bool) -> Result<()> {
    runs.sort_unstable_by_key(|r| r.0);
    let mut last: Option<(u64, u64)> = None;
    for (start, blocks, epoch) in runs {
        if let Some((end, previous_epoch)) = last {
            if start < end || (free && start == end && epoch == previous_epoch) {
                return Err(Error::CorruptTree);
            }
        }
        last = Some((start + u64::from(blocks), epoch));
    }
    Ok(())
}
struct Walker<'a, D: BlockDevice> {
    device: &'a mut D,
    tree: TreeId,
    id: u128,
    generation: u64,
    pages: BTreeSet<u64>,
    free: BTreeMap<u64, FreeRun>,
    dead: BTreeMap<(u64, u64), DeadRun>,
}
impl<D: BlockDevice> Walker<'_, D> {
    fn visit(&mut self, pointer: BlockPointer, root: bool) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        if self.pages.len() >= MAX_SPACE_PAGES || !self.pages.insert(pointer.address) {
            return Err(Error::CorruptTree);
        }
        let page = read_page(self.device, pointer, self.tree, self.id, self.generation)?;
        if page.items.is_empty() {
            return if root && page.level == 0 {
                Ok(None)
            } else {
                Err(Error::CorruptTree)
            };
        }
        let mut min = None;
        let mut last: Option<Vec<u8>> = None;
        if page.level == 0 {
            for item in &page.items {
                if last.as_ref().is_some_and(|k| k >= &item.key) {
                    return Err(Error::CorruptTree);
                }
                match self.tree {
                    TreeId::Free => {
                        let run = free_decode(&item.key, &item.value, self.device.block_count())?;
                        if self.free.len() >= MAX_SPACE_RUNS
                            || self.free.insert(run.start, run).is_some()
                        {
                            return Err(Error::CorruptTree);
                        }
                    }
                    TreeId::DeadList => {
                        let run = dead_decode(
                            &item.key,
                            &item.value,
                            self.device.block_count(),
                            self.generation,
                        )?;
                        if self.dead.len() >= MAX_SPACE_RUNS
                            || self
                                .dead
                                .insert((run.dead_generation, run.start), run)
                                .is_some()
                        {
                            return Err(Error::CorruptTree);
                        }
                    }
                    _ => return Err(Error::CorruptTree),
                }
                if min.is_none() {
                    min = Some(item.key.clone());
                }
                last = Some(item.key.clone());
            }
        } else {
            for item in &page.items {
                let pointer = BlockPointer::decode_required(&item.value)?;
                let (first, max) = self.visit(pointer, false)?.ok_or(Error::CorruptTree)?;
                if max != item.key || last.as_ref().is_some_and(|k| k >= &first) {
                    return Err(Error::CorruptTree);
                }
                if min.is_none() {
                    min = Some(first);
                }
                last = Some(max);
            }
        }
        Ok(Some((
            min.ok_or(Error::CorruptTree)?,
            last.ok_or(Error::CorruptTree)?,
        )))
    }
}
pub fn read_space_tree<D: BlockDevice>(
    device: &mut D,
    tree: TreeId,
    root: Option<BlockPointer>,
    id: u128,
    generation: u64,
) -> Result<SpaceScan> {
    if !matches!(tree, TreeId::Free | TreeId::DeadList) || id == 0 || generation == 0 {
        return Err(Error::CorruptTree);
    }
    let Some(pointer) = root else {
        return Ok(SpaceScan::empty());
    };
    let mut walker = Walker {
        device,
        tree,
        id,
        generation,
        pages: BTreeSet::new(),
        free: BTreeMap::new(),
        dead: BTreeMap::new(),
    };
    walker.visit(pointer, true)?;
    no_overlaps(
        walker
            .free
            .values()
            .map(|r| (r.start, r.blocks, r.eligible_after))
            .collect(),
        true,
    )?;
    no_overlaps(
        walker
            .dead
            .values()
            .map(|r| (r.start, r.blocks, r.eligible_after))
            .collect(),
        false,
    )?;
    Ok(SpaceScan {
        free: walker.free,
        dead: walker.dead,
        pages: walker.pages,
    })
}

pub struct StagedSpace {
    pub root: Option<BlockPointer>,
    pub staged_pages: Vec<(u64, [u8; BLOCK_SIZE])>,
    pub consumed_reservations: usize,
}
struct Child {
    max_key: Vec<u8>,
    pointer: BlockPointer,
}
struct Planner<'a, D: BlockDevice> {
    device: &'a mut D,
    tree: TreeId,
    id: u128,
    generation: u64,
    slots: &'a [u64],
    staged: Vec<(u64, [u8; BLOCK_SIZE])>,
}
impl<D: BlockDevice> Planner<'_, D> {
    fn page(&mut self, level: u8, items: Vec<Item>) -> Result<Child> {
        let address = *self.slots.get(self.staged.len()).ok_or(Error::NoSpace)?;
        let page = Page {
            tree: self.tree,
            level,
            address,
            birth: self.generation,
            volume_id: self.id,
            items,
        };
        let data = page.encode()?;
        let pointer = page.pointer()?;
        let max_key = page.items.last().ok_or(Error::CorruptTree)?.key.clone();
        self.staged.push((address, data));
        Ok(Child { max_key, pointer })
    }
    fn pack(&mut self, level: u8, items: Vec<Item>) -> Result<Vec<Child>> {
        let mut out = Vec::new();
        let mut current = Vec::new();
        let mut size = PAGE_HEADER_BYTES;
        for item in items {
            let cost = 8 + item.key.len() + item.value.len();
            if cost + PAGE_HEADER_BYTES > BLOCK_SIZE {
                return Err(Error::TooLarge);
            }
            if size + cost > BLOCK_SIZE {
                out.push(self.page(level, std::mem::take(&mut current))?);
                size = PAGE_HEADER_BYTES;
            }
            size += cost;
            current.push(item);
        }
        if !current.is_empty() {
            out.push(self.page(level, current)?);
        }
        Ok(out)
    }
    fn edit(
        &mut self,
        pointer: BlockPointer,
        changes: &[(Vec<u8>, Option<Vec<u8>>)],
    ) -> Result<Vec<Child>> {
        let page = read_page(
            self.device,
            pointer,
            self.tree,
            self.id,
            self.generation - 1,
        )?;
        if page.level == 0 {
            let mut map = BTreeMap::new();
            for item in page.items {
                if map.insert(item.key, item.value).is_some() {
                    return Err(Error::CorruptTree);
                }
            }
            for (key, value) in changes {
                if let Some(value) = value {
                    map.insert(key.clone(), value.clone());
                } else if map.remove(key).is_none() {
                    return Err(Error::CorruptTree);
                }
            }
            return self.pack(
                0,
                map.into_iter()
                    .map(|(key, value)| Item { key, value })
                    .collect(),
            );
        }
        if page.items.is_empty() {
            return Err(Error::CorruptTree);
        }
        let mut groups = vec![Vec::new(); page.items.len()];
        for (key, value) in changes {
            let k = match page.items.binary_search_by(|item| item.key.cmp(key)) {
                Ok(i) | Err(i) if i < page.items.len() => i,
                Ok(_) | Err(_) => page.items.len() - 1,
            };
            groups[k].push((key.clone(), value.clone()));
        }
        let mut children = Vec::new();
        for (item, deltas) in page.items.into_iter().zip(groups) {
            let old = BlockPointer::decode_required(&item.value)?;
            if deltas.is_empty() {
                children.push(Child {
                    max_key: item.key,
                    pointer: old,
                });
            } else {
                children.extend(self.edit(old, &deltas)?);
            }
        }
        let items = children
            .into_iter()
            .map(|c| Item {
                key: c.max_key,
                value: c.pointer.encode().to_vec(),
            })
            .collect();
        self.pack(page.level, items)
    }
}
/// Path-copy exact key changes. The caller certifies every reservation as
/// unowned by both F0 candidates, then independently validates the result.
#[allow(clippy::too_many_arguments)]
pub fn stage_space_delta<D: BlockDevice>(
    device: &mut D,
    tree: TreeId,
    previous: Option<BlockPointer>,
    changes: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    reservations: &[u64],
    id: u128,
    generation: u64,
    high_water: u64,
) -> Result<StagedSpace> {
    if !matches!(tree, TreeId::Free | TreeId::DeadList | TreeId::Ownership | TreeId::Fs)
        || id == 0
        || generation <= 1
        || high_water <= ROOT_PAYLOAD_LAST
        || high_water > device.block_count()
        || reservations
            .iter()
            .any(|&n| n <= ROOT_PAYLOAD_LAST || n >= device.block_count())
        || reservations.windows(2).any(|w| w[0] <= w[1])
    {
        return Err(Error::Geometry);
    }
    for (key, value) in changes {
        if let Some(value) = value {
            match tree {
                TreeId::Free => {
                    free_decode(key, value, device.block_count())?;
                }
                TreeId::DeadList => {
                    dead_decode(key, value, device.block_count(), generation)?;
                }
                TreeId::Ownership => {
                    super::owner_tree::validate_owner_item(key, value, device.block_count())?;
                }
                TreeId::Fs => {
                    super::extent_map::validate_large_item(key, value, device.block_count())?;
                }
                _ => unreachable!(),
            }
        } else if key.len() != if tree == TreeId::DeadList { 16 } else { 8 } {
            return Err(Error::Geometry);
        }
    }
    if changes.is_empty() {
        return Ok(StagedSpace {
            root: previous,
            staged_pages: Vec::new(),
            consumed_reservations: 0,
        });
    }
    let mut planner = Planner {
        device,
        tree,
        id,
        generation,
        slots: reservations,
        staged: Vec::new(),
    };
    let mut children = if let Some(old) = previous {
        old.validate(planner.device.block_count(), generation - 1, high_water)?;
        let pairs = changes
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>();
        planner.edit(old, &pairs)?
    } else {
        let mut items = Vec::new();
        for (key, value) in changes {
            items.push(Item {
                key: key.clone(),
                value: value.clone().ok_or(Error::CorruptTree)?,
            });
        }
        planner.pack(0, items)?
    };
    if children.is_empty() {
        return Ok(StagedSpace {
            root: None,
            consumed_reservations: planner.staged.len(),
            staged_pages: planner.staged,
        });
    }
    let mut level = children[0].pointer.level;
    while children.len() > 1 {
        if level >= MAX_LEVEL {
            return Err(Error::TooLarge);
        }
        level += 1;
        let items = children
            .into_iter()
            .map(|child| Item {
                key: child.max_key,
                value: child.pointer.encode().to_vec(),
            })
            .collect();
        children = planner.pack(level, items)?;
    }
    Ok(StagedSpace {
        root: Some(children.pop().ok_or(Error::CorruptTree)?.pointer),
        consumed_reservations: planner.staged.len(),
        staged_pages: planner.staged,
    })
}
/// Authenticated-tree contents are still not an ownership proof. Check the
/// independent two-root owner set before any allocation from free runs,
/// and prove free/dead intervals do not contradict one another.
pub fn validate_space_pair(
    free: &BTreeMap<u64, FreeRun>,
    dead: &BTreeMap<(u64, u64), DeadRun>,
    both_root_owners: &BTreeSet<u64>,
    published_generation: u64,
    total: u64,
) -> Result<()> {
    if published_generation == 0 {
        return Err(Error::CorruptTree);
    }
    let mut all = Vec::new();
    for run in free.values() {
        free_item(*run, total).map_err(|_| Error::CorruptTree)?;
        // A Free entry must be unowned *now*, not merely when its
        // eligibility epoch arrives. Pages still owned by either F0 root
        // belong in delayed retirement, never in the Free tree.
        let end = run.start + u64::from(run.blocks);
        if both_root_owners.range(run.start..end).next().is_some() {
            return Err(Error::CorruptTree);
        }
        all.push((run.start, run.blocks, run.eligible_after));
    }
    for run in dead.values() {
        dead_item(*run, total).map_err(|_| Error::CorruptTree)?;
        all.push((run.start, run.blocks, run.eligible_after));
    }
    no_overlaps(all, false)
}

/// Fast index lookup against a previously certified two-root owner set.
/// Returns descending physical reservations and a new exact free-run map.
/// The caller must stage/publish the new Free root atomically with writes.
pub struct FreeReservation {
    pub selected: Vec<u64>,
    pub remaining: BTreeMap<u64, FreeRun>,
}
pub fn reserve_free(
    free: &BTreeMap<u64, FreeRun>,
    both_root_owners: &BTreeSet<u64>,
    current_generation: u64,
    requested: usize,
) -> Result<FreeReservation> {
    if current_generation == 0 {
        return Err(Error::Geometry);
    }
    // Validate the *entire* advertised free map, not just the highest
    // addresses this particular request happens to consume. Otherwise a
    // stale, future-eligible ownership claim could go unnoticed until a
    // later allocation transaction.
    for run in free.values() {
        if run.blocks == 0 || run.start <= ROOT_PAYLOAD_LAST || run.eligible_after == 0 {
            return Err(Error::CorruptTree);
        }
        let end = run
            .start
            .checked_add(u64::from(run.blocks))
            .ok_or(Error::CorruptTree)?;
        // Check the ordered owner set without visiting every block of a
        // potentially enormous contiguous free extent.
        if both_root_owners.range(run.start..end).next().is_some() {
            return Err(Error::CorruptTree);
        }
    }
    no_overlaps(
        free.values()
            .map(|run| (run.start, run.blocks, run.eligible_after))
            .collect(),
        true,
    )?;
    if requested == 0 {
        return Ok(FreeReservation {
            selected: Vec::new(),
            remaining: free.clone(),
        });
    }
    let mut remaining = free.clone();
    let mut selected = Vec::new();
    for (&start, &run) in free.iter().rev() {
        if run.eligible_after > current_generation {
            continue;
        }
        let mut taken = 0_u32;
        for address in (run.start..run.start + u64::from(run.blocks)).rev() {
            if selected.len() >= requested {
                break;
            }
            if both_root_owners.contains(&address) {
                return Err(Error::CorruptTree);
            }
            selected.push(address);
            taken += 1;
        }
        if taken > 0 {
            let rest = run.blocks - taken;
            remaining.remove(&start);
            if rest > 0 {
                remaining.insert(
                    start,
                    FreeRun {
                        blocks: rest,
                        ..run
                    },
                );
            }
        }
        if selected.len() == requested {
            break;
        }
    }
    if selected.len() != requested {
        return Err(Error::NoSpace);
    }
    Ok(FreeReservation {
        selected,
        remaining,
    })
}

/// Bounded, deliberately non-authoritative incremental retirement planner.
/// The caller supplies a FULL independently certified set of both-F0 owners,
/// and must atomically publish resulting Free/Dead tree deltas with F0.
pub struct RetireStep {
    pub free: BTreeMap<u64, FreeRun>,
    pub dead: BTreeMap<(u64, u64), DeadRun>,
    pub scanned: usize,
    pub released: u64,
    /// Last visited (retirement-generation, physical-start). Persist this
    /// in the two manifest sweep fields atomically with the space-tree roots.
    pub cursor: Option<(u64, u64)>,
}
pub fn retire_step(
    free: &BTreeMap<u64, FreeRun>,
    dead: &BTreeMap<(u64, u64), DeadRun>,
    protected: &BTreeSet<u64>,
    current_generation: u64,
    budget: usize,
) -> Result<RetireStep> {
    retire_step_cursor(free, dead, protected, current_generation, budget, None)
}
/// Cursor-advanced bounded sweep. The previous scanned key (including a
/// still-owned entry) is persisted in the manifest; on the next transaction
/// the sweep resumes after it and wraps once. This prevents a permanently
/// protected oldest entry from starving younger eligible retirements.
pub fn retire_step_cursor(
    free: &BTreeMap<u64, FreeRun>,
    dead: &BTreeMap<(u64, u64), DeadRun>,
    protected: &BTreeSet<u64>,
    current_generation: u64,
    budget: usize,
    after: Option<(u64, u64)>,
) -> Result<RetireStep> {
    if current_generation == 0 || budget > MAX_RETIRE_STEP {
        return Err(Error::Geometry);
    }
    let mut out_free = free.clone();
    let mut out_dead = dead.clone();
    let mut scanned = 0;
    let mut released = 0;
    let mut cursor = if dead.is_empty() { None } else { after };
    // BTreeMap cursor ranges avoid collecting/sorting the entire retirement
    // index on each bounded step. Resume strictly after the last visited key,
    // then wrap once; even future-ineligible records consume the work budget.
    let anchor = after.unwrap_or((0, 0));
    for (&key, &item) in dead
        .range((
            std::ops::Bound::Excluded(anchor),
            std::ops::Bound::Unbounded,
        ))
        .chain(dead.range(..=anchor))
    {
        if scanned >= budget {
            break;
        }
        scanned += 1;
        cursor = Some(key);
        // Defend the bounded public planner against malformed in-memory
        // records as well as the on-disk decoder. In particular, an
        // overflowing epoch can never mature into a reusable run.
        if key != (item.dead_generation, item.start)
            || item.dead_generation > current_generation
            || !valid_retirement_epoch(item.dead_generation, item.eligible_after)
            || item.start <= ROOT_PAYLOAD_LAST
            || item.blocks == 0
        {
            return Err(Error::CorruptTree);
        }
        let end = item
            .start
            .checked_add(u64::from(item.blocks))
            .ok_or(Error::CorruptTree)?;
        if item.eligible_after > current_generation {
            continue;
        }
        if protected.range(item.start..end).next().is_some() {
            continue;
        }
        if out_free.values().any(|r| {
            let limit = r.start + u64::from(r.blocks);
            r.start < end && item.start < limit
        }) {
            return Err(Error::CorruptTree);
        }
        out_free.insert(
            item.start,
            FreeRun {
                start: item.start,
                blocks: item.blocks,
                eligible_after: item.eligible_after,
            },
        );
        out_dead.remove(&key);
        released += u64::from(item.blocks);
    }
    // Coalesce only contiguous runs with the *same absolute epoch*.
    // Never merge different epochs into an earlier-ready range.
    let mut runs: Vec<FreeRun> = Vec::new();
    for run in out_free.values().copied() {
        if let Some(last) = runs.last_mut() {
            let adjoining = last.start + u64::from(last.blocks) == run.start;
            let total = u64::from(last.blocks) + u64::from(run.blocks);
            if adjoining
                && last.eligible_after == run.eligible_after
                && total <= u64::from(u32::MAX)
            {
                last.blocks += run.blocks;
                continue;
            }
        }
        runs.push(run);
    }
    out_free = runs.into_iter().map(|run| (run.start, run)).collect();
    Ok(RetireStep {
        free: out_free,
        dead: out_dead,
        scanned,
        released,
        cursor,
    })
}
