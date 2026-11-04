//! F2.R1: durable checksummed page-chained physical block reference index.
//!
//! Each root publishes one immutable index via the same F0 checksummed
//! manifest that publishes its namespace. Each entry records the number of
//! *distinct roots within that manifest* that reference a physical F3 page or
//! file extent chunk. The index pages themselves are protected separately by
//! the owning F0 root; they are not recursively represented in their index.
//!
//! The persisted entries are untrusted: load_state independently traverses
//! current and pinned trees and rejects an index not exactly matching them.
use super::{
    BLOCK_SIZE, BlockDevice, Builder, Error, Result, seal, u16_at, u64_at, valid_checksum,
};
use std::collections::{BTreeMap, BTreeSet};

const MAGIC: &[u8; 8] = b"BFSREF5\0";
const HEADER: usize = 32;
const ENTRY: usize = 10;
const PAGE_ENTRIES: usize = (BLOCK_SIZE - HEADER) / ENTRY;
const MAX_PAGES: usize = 2048;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RefHeader {
    pub first: u64,
    pub pages: u32,
    pub entries: u32,
}

pub(super) fn persist(
    builder: &mut Builder,
    generation: u64,
    counts: &BTreeMap<u64, u16>,
) -> Result<RefHeader> {
    if generation == 0 || counts.len() > u32::MAX as usize {
        return Err(Error::TooLarge);
    }
    // A single empty page is intentionally committed for an empty namespace,
    // distinguishing "checked empty map" from "missing reference index".
    let rows: Vec<(u64, u16)> = counts.iter().map(|(&id, &n)| (id, n)).collect();
    let page_count = usize::max(1, rows.len().div_ceil(PAGE_ENTRIES));
    if page_count > MAX_PAGES {
        return Err(Error::TooLarge);
    }
    let mut next: u64 = 0;
    for index in (0..page_count).rev() {
        let start = index * PAGE_ENTRIES;
        let end = usize::min(start + PAGE_ENTRIES, rows.len());
        let items = &rows[start..end];
        let mut block = [0_u8; BLOCK_SIZE];
        block[..8].copy_from_slice(MAGIC);
        block[8..16].copy_from_slice(&generation.to_le_bytes());
        block[16..24].copy_from_slice(&next.to_le_bytes());
        block[28..30].copy_from_slice(&(items.len() as u16).to_le_bytes());
        for (n, (id, refs)) in items.iter().enumerate() {
            if *refs == 0 {
                return Err(Error::CorruptTree);
            }
            let at = HEADER + n * ENTRY;
            block[at..at + 8].copy_from_slice(&id.to_le_bytes());
            block[at + 8..at + 10].copy_from_slice(&refs.to_le_bytes());
        }
        next = builder.push(seal(block))?;
    }
    Ok(RefHeader {
        first: next,
        pages: page_count as u32,
        entries: rows.len() as u32,
    })
}

/// Load the index pages and validate checksums, chain shape, ownership bounds,
/// reference-count positivity and **global strict address ordering**.
/// Returns both logical counts and physical index pages; callers must protect
/// the index pages separately in every retained F0 root.
pub(super) fn read<D: BlockDevice>(
    device: &mut D,
    header: RefHeader,
    generation: u64,
    cursor: u64,
) -> Result<(BTreeMap<u64, u16>, BTreeSet<u64>)> {
    if header.first == 0
        || header.pages == 0
        || header.pages as usize > MAX_PAGES
        || header.pages as u64 > device.block_count()
        || header.entries as u64 > device.block_count()
        || header.entries as usize > header.pages as usize * PAGE_ENTRIES
        || generation == 0
    {
        return Err(Error::CorruptTree);
    }
    let mut counts = BTreeMap::new();
    let mut index_pages = BTreeSet::new();
    let mut next = header.first;
    let mut previous = None;
    for page in 0..header.pages {
        if next <= generation.saturating_add(1)
            || next < cursor
            || next >= device.block_count()
            || !index_pages.insert(next)
        {
            return Err(Error::CorruptTree);
        }
        let mut block = [0_u8; BLOCK_SIZE];
        device.read_block(next, &mut block)?;
        if &block[..8] != MAGIC
            || u64_at(&block, 8) != generation
            || !valid_checksum(&block)
            || block[30..32].iter().any(|v| *v != 0)
        {
            return Err(Error::CorruptTree);
        }
        let count = u16_at(&block, 28) as usize;
        if count > PAGE_ENTRIES
            || (page + 1 < header.pages && count != PAGE_ENTRIES)
            || (page + 1 == header.pages && count == 0 && header.entries != 0)
            || block[HEADER + count * ENTRY..].iter().any(|v| *v != 0)
        {
            return Err(Error::CorruptTree);
        }
        for index in 0..count {
            let at = HEADER + index * ENTRY;
            let address = u64_at(&block, at);
            let refs = u16_at(&block, at + 8);
            if address <= generation.saturating_add(1)
                || address < cursor
                || address >= device.block_count()
                || refs == 0
                || previous.is_some_and(|last| address <= last)
            {
                return Err(Error::CorruptTree);
            }
            previous = Some(address);
            counts.insert(address, refs);
        }
        next = u64_at(&block, 16);
    }
    if next != 0
        || counts.len() != header.entries as usize
        || index_pages.iter().any(|block| counts.contains_key(block))
    {
        return Err(Error::CorruptTree);
    }
    // Per-block checksums protect all bytes. A map mismatch is detected by
    // the independent current + pinned snapshot graph walk in load_state.
    Ok((counts, index_pages))
}

pub(super) fn count_roots<D: BlockDevice>(
    device: &mut D,
    live: super::SnapshotRoot,
    pinned: &BTreeMap<u64, super::SnapshotRoot>,
) -> Result<BTreeMap<u64, u16>> {
    let mut dedup = BTreeSet::new();
    let mut counts = BTreeMap::<u64, u16>::new();
    for root in std::iter::once(live).chain(pinned.values().copied()) {
        if !dedup.insert((root.generation, root.tree_root)) {
            continue;
        }
        let (_, blocks) = super::load_tree(device, root)?.ok_or(Error::CorruptTree)?;
        for address in blocks {
            let value = counts.entry(address).or_default();
            *value = value.checked_add(1).ok_or(Error::TooLarge)?;
        }
    }
    Ok(counts)
}
