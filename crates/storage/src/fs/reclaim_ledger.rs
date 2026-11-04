//! F2.R4: crash-atomic free-run checkpoint with bounded resumption cursor.
//!
//! Each F0 manifest owns one immutable checked journal chain. "Ready" ranges
//! are absent from BOTH recoverable roots after publication; "pending" ranges
//! are still held by the previous F0 candidate and become ready ONLY after the
//! generation that rotates that candidate out is durably published.
//!
//! These pages are allocated from *pre-commit* independently verified free
//! space, never from newly retired pages. The chain is a durability witness;
//! until a separate proof-carrying incremental allocator exists, F2's full
//! reachability oracle remains the authority for selecting reusable blocks.
use super::{
    BLOCK_SIZE, BlockDevice, Builder, Error, Result, seal, u16_at, u32_at, u64_at, valid_checksum,
};
use std::collections::BTreeSet;

const MAGIC: &[u8; 8] = b"BFSFRJ8\0";
const HEADER: usize = 32;
const ENTRY: usize = 20;
const PAGE_ENTRIES: usize = (BLOCK_SIZE - HEADER) / ENTRY;
const MAX_PAGES: usize = 2048;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct JournalHeader {
    pub first: u64,
    pub pages: u32,
    pub runs: u32,
    /// Physical continuation address for the bounded deferred-free sweep.
    pub sweep_cursor: u64,
}

#[derive(Debug)]
pub(super) struct Journal {
    pub pages: BTreeSet<u64>,
    pub ready: BTreeSet<u64>,
    pub pending: BTreeSet<u64>,
    pub deferred: BTreeSet<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    start: u64,
    length: u32,
    eligible_generation: u64,
}

fn valid_address(address: u64, generation: u64, cursor: u64, total: u64) -> bool {
    address > generation.saturating_add(1) && address >= cursor && address < total
}

fn encode_runs(
    ready: &BTreeSet<u64>,
    pending: &BTreeSet<u64>,
    deferred: &BTreeSet<u64>,
    generation: u64,
) -> Result<Vec<Run>> {
    let mut rows = Vec::with_capacity(
        ready.len().saturating_add(pending.len()).saturating_add(deferred.len()),
    );
    for &address in ready {
        rows.push((address, generation));
    }
    let next = generation.checked_add(1).ok_or(Error::GenerationOverflow)?;
    for &address in pending {
        rows.push((address, next));
    }
    for &address in deferred {
        rows.push((address, 0));
    }
    rows.sort_unstable_by_key(|(address, _)| *address);
    let mut runs = Vec::<Run>::new();
    for (address, eligible_generation) in rows {
        if let Some(last) = runs.last_mut() {
            let end = last.start + u64::from(last.length);
            if address < end {
                return Err(Error::CorruptTree);
            }
            if address == end
                && last.eligible_generation == eligible_generation
                && last.length < u32::MAX
            {
                last.length += 1;
                continue;
            }
        }
        runs.push(Run {
            start: address,
            length: 1,
            eligible_generation,
        });
    }
    Ok(runs)
}

/// Reserve journal blocks from the precommit proven-free pool, then
/// serialize ready+pending extents. Reserving one journal page can split a
/// free run, so iterate until the reserved count covers the final row count.
/// Newly retired ranges are NEVER inserted into builder.reusable.
pub(super) fn persist(
    builder: &mut Builder,
    generation: u64,
    promoted: &BTreeSet<u64>,
    pending: &BTreeSet<u64>,
    deferred: &BTreeSet<u64>,
    sweep_cursor: u64,
) -> Result<JournalHeader> {
    if generation == 0
        || generation == u64::MAX
        || !promoted.is_disjoint(pending)
        || !promoted.is_disjoint(deferred)
        || !pending.is_disjoint(deferred)
        || (sweep_cursor != 0 && sweep_cursor < builder.cursor)
    {
        return Err(Error::CorruptTree);
    }
    let mut addresses = Vec::new();
    let runs = loop {
        let ready: BTreeSet<u64> = builder.reusable.union(promoted).copied().collect();
        if !ready.is_disjoint(pending)
            || !ready.is_disjoint(deferred)
            || ready
                .iter()
                .chain(pending)
                .chain(deferred)
                .any(|&id| !valid_address(id, generation, builder.cursor, u64::MAX))
        {
            return Err(Error::CorruptTree);
        }
        let rows = encode_runs(&ready, pending, deferred, generation)?;
        if rows.len() > (MAX_PAGES * PAGE_ENTRIES) {
            return Err(Error::TooLarge);
        }
        let needed = usize::max(1, rows.len().div_ceil(PAGE_ENTRIES));
        if addresses.len() >= needed {
            break rows;
        }
        if addresses.len() >= MAX_PAGES {
            return Err(Error::TooLarge);
        }
        let block = builder.reserve(1)?;
        addresses.push(block);
    };
    for (index, &address) in addresses.iter().enumerate() {
        let begin = index * PAGE_ENTRIES;
        let end = usize::min(begin + PAGE_ENTRIES, runs.len());
        let mut block = [0_u8; BLOCK_SIZE];
        block[..8].copy_from_slice(MAGIC);
        block[8..16].copy_from_slice(&generation.to_le_bytes());
        let next = addresses.get(index + 1).copied().unwrap_or(0);
        block[16..24].copy_from_slice(&next.to_le_bytes());
        block[28..30].copy_from_slice(&((end - begin) as u16).to_le_bytes());
        for (n, run) in runs[begin..end].iter().enumerate() {
            let at = HEADER + n * ENTRY;
            block[at..at + 8].copy_from_slice(&run.start.to_le_bytes());
            block[at + 8..at + 12].copy_from_slice(&run.length.to_le_bytes());
            block[at + 12..at + 20].copy_from_slice(&run.eligible_generation.to_le_bytes());
        }
        builder.staged.push((address, seal(block)));
    }
    Ok(JournalHeader {
        first: addresses[0],
        pages: addresses.len() as u32,
        runs: runs.len() as u32,
        sweep_cursor,
    })
}

/// Read one complete immutable checkpoint, never following an unchecked
/// pointer. A published journal can always be replayed from its manifest:
/// no partial-progress state exists outside the root slot.
pub(super) fn read<D: BlockDevice>(
    device: &mut D,
    header: JournalHeader,
    generation: u64,
    cursor: u64,
) -> Result<Journal> {
    if header.first == 0
        || header.pages == 0
        || header.pages as usize > MAX_PAGES
        || header.pages as u64 > device.block_count()
        || header.runs as usize > header.pages as usize * PAGE_ENTRIES
        || generation == 0
        || generation == u64::MAX
        || (header.sweep_cursor != 0
            && (header.sweep_cursor < cursor || header.sweep_cursor > device.block_count()))
    {
        return Err(Error::CorruptTree);
    }
    let mut pages = BTreeSet::new();
    let mut ready = BTreeSet::new();
    let mut pending = BTreeSet::new();
    let mut deferred = BTreeSet::new();
    let mut next = header.first;
    let mut previous_end = None;
    let mut total_runs = 0usize;
    let mut total_blocks = 0u64;
    for i in 0..header.pages {
        if !valid_address(next, generation, cursor, device.block_count()) || !pages.insert(next) {
            return Err(Error::CorruptTree);
        }
        let mut block = [0; BLOCK_SIZE];
        device.read_block(next, &mut block)?;
        if &block[..8] != MAGIC
            || u64_at(&block, 8) != generation
            || !valid_checksum(&block)
            || block[30..32].iter().any(|&n| n != 0)
        {
            return Err(Error::CorruptTree);
        }
        let count = u16_at(&block, 28) as usize;
        if count > PAGE_ENTRIES
            || (i + 1 < header.pages && count != PAGE_ENTRIES)
            || (i + 1 == header.pages && header.runs > 0 && count == 0)
            || block[HEADER + count * ENTRY..].iter().any(|&n| n != 0)
        {
            return Err(Error::CorruptTree);
        }
        for index in 0..count {
            let at = HEADER + index * ENTRY;
            let start = u64_at(&block, at);
            let length = u32_at(&block, at + 8);
            let eligible = u64_at(&block, at + 12);
            let Some(end) = start.checked_add(u64::from(length)) else {
                return Err(Error::CorruptTree);
            };
            if length == 0
                || !valid_address(start, generation, cursor, device.block_count())
                || end > device.block_count()
                || previous_end.is_some_and(|last| start < last)
                || (eligible != 0 && eligible != generation && eligible != generation + 1)
            {
                return Err(Error::CorruptTree);
            }
            previous_end = Some(end);
            total_blocks = total_blocks
                .checked_add(u64::from(length))
                .ok_or(Error::CorruptTree)?;
            if total_blocks > device.block_count() {
                return Err(Error::CorruptTree);
            }
            let target = if eligible == generation {
                &mut ready
            } else if eligible == 0 {
                &mut deferred
            } else {
                &mut pending
            };
            target.extend(start..end);
        }
        total_runs += count;
        next = u64_at(&block, 16);
    }
    if next != 0
        || total_runs != header.runs as usize
        || pages
            .iter()
            .any(|id| ready.contains(id) || pending.contains(id))
        || !ready.is_disjoint(&pending)
        || !ready.is_disjoint(&deferred)
        || !pending.is_disjoint(&deferred)
    {
        return Err(Error::CorruptTree);
    }
    Ok(Journal {
        pages,
        ready,
        pending,
        deferred,
    })
}
