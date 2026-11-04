//! F2.R2: immutable, incrementally path-copied physical-reference B+ tree.
//!
//! Counts are per distinct live/pinned namespace root within ONE F0 candidate.
//! Index pages themselves are owned directly by the publishing manifest, not
//! counted in their own postings. Each CoW page has a birth generation and CRC.
//! Allocation MUST still protect both F0 candidates and verify their F3 graph.
use super::{
    BLOCK_SIZE, BlockDevice, Builder, Error, Result, StagedReader, Volume, seal, u16_at, u64_at,
    valid_checksum,
};
use std::collections::{BTreeMap, BTreeSet};

const LEAF_MAGIC: &[u8; 8] = b"BFSRLF6\0";
const BRANCH_MAGIC: &[u8; 8] = b"BFSRBR6\0";
const HEADER: usize = 32;
const LEAF_ENTRY: usize = 10;
const BRANCH_ENTRY: usize = 16;
const LEAF_CAPACITY: usize = (BLOCK_SIZE - HEADER) / LEAF_ENTRY;
const BRANCH_CAPACITY: usize = (BLOCK_SIZE - HEADER) / BRANCH_ENTRY;
const MAX_PAGES: usize = 2048;
const MAX_LEVEL: u8 = 8;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RefHeader {
    pub first: u64,
    pub pages: u32,
    pub entries: u32,
}

#[derive(Clone, Copy)]
struct Child {
    max: u64,
    address: u64,
    level: u8,
}

enum Page {
    Leaf(Vec<(u64, u16)>),
    Branch { level: u8, children: Vec<Child> },
}

fn valid_address(address: u64, generation: u64, cursor: u64, total: u64) -> bool {
    address > generation.saturating_add(1) && address >= cursor && address < total
}

/// Decode exactly one checked immutable page; child subtree integrity is
/// checked separately by the recursive reader.
fn decode(
    b: &[u8; BLOCK_SIZE],
    generation: u64,
    cursor: u64,
    total: u64,
) -> Result<Page> {
    let leaf = &b[..8] == LEAF_MAGIC;
    let branch = &b[..8] == BRANCH_MAGIC;
    let birth = u64_at(b, 8);
    let level = b[30];
    let count = u16_at(b, 28) as usize;
    if (!leaf && !branch)
        || birth == 0
        || birth > generation
        || !valid_checksum(b)
        || b[16..24].iter().any(|v| *v != 0)
        || b[31] != 0
        || level > MAX_LEVEL
        || (leaf && level != 0)
        || (branch && (level == 0 || count == 0))
    {
        return Err(Error::CorruptTree);
    }
    let mut last = None;
    if leaf {
        if count > LEAF_CAPACITY || b[HEADER + count * LEAF_ENTRY..].iter().any(|v| *v != 0) {
            return Err(Error::CorruptTree);
        }
        let mut rows = Vec::with_capacity(count);
        for i in 0..count {
            let at = HEADER + i * LEAF_ENTRY;
            let key = u64_at(b, at);
            let refs = u16_at(b, at + 8);
            if !valid_address(key, generation, cursor, total)
                || refs == 0
                || last.is_some_and(|prev| key <= prev)
            {
                return Err(Error::CorruptTree);
            }
            last = Some(key);
            rows.push((key, refs));
        }
        Ok(Page::Leaf(rows))
    } else {
        if count > BRANCH_CAPACITY || b[HEADER + count * BRANCH_ENTRY..].iter().any(|v| *v != 0) {
            return Err(Error::CorruptTree);
        }
        let mut children = Vec::with_capacity(count);
        for i in 0..count {
            let at = HEADER + i * BRANCH_ENTRY;
            let max = u64_at(b, at);
            let address = u64_at(b, at + 8);
            if !valid_address(max, generation, cursor, total)
                || !valid_address(address, generation, cursor, total)
                || last.is_some_and(|prev| max <= prev)
            {
                return Err(Error::CorruptTree);
            }
            last = Some(max);
            children.push(Child {
                max,
                address,
                level: level - 1,
            });
        }
        Ok(Page::Branch { level, children })
    }
}

fn encode_leaf(items: &[(u64, u16)], generation: u64) -> Result<[u8; BLOCK_SIZE]> {
    if items.len() > LEAF_CAPACITY || generation == 0 {
        return Err(Error::TooLarge);
    }
    let mut b = [0; BLOCK_SIZE];
    b[..8].copy_from_slice(LEAF_MAGIC);
    b[8..16].copy_from_slice(&generation.to_le_bytes());
    b[28..30].copy_from_slice(&(items.len() as u16).to_le_bytes());
    let mut last = None;
    for (i, &(address, refs)) in items.iter().enumerate() {
        if refs == 0 || last.is_some_and(|prev| address <= prev) {
            return Err(Error::CorruptTree);
        }
        let at = HEADER + i * LEAF_ENTRY;
        b[at..at + 8].copy_from_slice(&address.to_le_bytes());
        b[at + 8..at + 10].copy_from_slice(&refs.to_le_bytes());
        last = Some(address);
    }
    Ok(seal(b))
}

fn encode_branch(children: &[Child], generation: u64, level: u8) -> Result<[u8; BLOCK_SIZE]> {
    if children.is_empty() || children.len() > BRANCH_CAPACITY || level == 0 || level > MAX_LEVEL {
        return Err(Error::TooLarge);
    }
    let mut b = [0; BLOCK_SIZE];
    b[..8].copy_from_slice(BRANCH_MAGIC);
    b[8..16].copy_from_slice(&generation.to_le_bytes());
    b[28..30].copy_from_slice(&(children.len() as u16).to_le_bytes());
    b[30] = level;
    let mut last = None;
    for (i, child) in children.iter().enumerate() {
        if child.level + 1 != level || last.is_some_and(|prev| child.max <= prev) {
            return Err(Error::CorruptTree);
        }
        let at = HEADER + i * BRANCH_ENTRY;
        b[at..at + 8].copy_from_slice(&child.max.to_le_bytes());
        b[at + 8..at + 16].copy_from_slice(&child.address.to_le_bytes());
        last = Some(child.max);
    }
    Ok(seal(b))
}

fn staged_or_durable<D: BlockDevice>(
    volume: &mut Volume<D>,
    builder: &Builder,
    address: u64,
    generation: u64,
) -> Result<Page> {
    let block = match builder.staged.iter().rev().find(|(id, _)| *id == address) {
        Some((_, data)) => *data,
        None => volume.read_data_block(address)?,
    };
    decode(&block, generation, builder.cursor, volume.block_count())
}

fn store_leaves(
    builder: &mut Builder,
    items: &[(u64, u16)],
    generation: u64,
) -> Result<Vec<Child>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let chunks: Vec<_> = if items.len() > LEAF_CAPACITY {
        vec![&items[..items.len() / 2], &items[items.len() / 2..]]
    } else {
        vec![items]
    };
    let mut result = Vec::new();
    for chunk in chunks {
        result.push(Child {
            address: builder.push(encode_leaf(chunk, generation)?)?,
            max: chunk.last().ok_or(Error::CorruptTree)?.0,
            level: 0,
        });
    }
    Ok(result)
}

fn store_branches(
    builder: &mut Builder,
    children: &[Child],
    level: u8,
    generation: u64,
) -> Result<Vec<Child>> {
    if children.is_empty() {
        return Ok(Vec::new());
    }
    let chunks: Vec<_> = if children.len() > BRANCH_CAPACITY {
        vec![&children[..children.len() / 2], &children[children.len() / 2..]]
    } else {
        vec![children]
    };
    let mut result = Vec::new();
    for chunk in chunks {
        result.push(Child {
            address: builder.push(encode_branch(chunk, generation, level)?)?,
            max: chunk.last().ok_or(Error::CorruptTree)?.max,
            level,
        });
    }
    Ok(result)
}

/// An unchanged subtree keeps its physical address. A changed leaf and its
/// ancestors are replaced. Underfilled pages are permitted; empty leaves
/// are removed and a one-child root is collapsed.
fn patch_node<D: BlockDevice>(
    volume: &mut Volume<D>,
    builder: &mut Builder,
    address: u64,
    key: u64,
    new_count: Option<u16>,
    generation: u64,
    root: bool,
) -> Result<Vec<Child>> {
    match staged_or_durable(volume, builder, address, generation)? {
        Page::Leaf(mut rows) => {
            match rows.binary_search_by_key(&key, |(id, _)| *id) {
                Ok(i) => match new_count {
                    Some(refs) => rows[i].1 = refs,
                    None => { rows.remove(i); }
                },
                Err(i) => {
                    let refs = new_count.ok_or(Error::CorruptTree)?;
                    rows.insert(i, (key, refs));
                }
            }
            store_leaves(builder, &rows, generation)
        }
        Page::Branch { level, mut children } => {
            let i = children.partition_point(|child| child.max < key).min(children.len() - 1);
            let replacement = patch_node(
                volume, builder, children[i].address, key, new_count, generation, false,
            )?;
            children.splice(i..=i, replacement);
            if children.is_empty() {
                return Ok(Vec::new());
            }
            if root && children.len() == 1 {
                return Ok(children);
            }
            store_branches(builder, &children, level, generation)
        }
    }
}

fn patch_one<D: BlockDevice>(
    volume: &mut Volume<D>,
    builder: &mut Builder,
    root: u64,
    key: u64,
    refs: Option<u16>,
    generation: u64,
) -> Result<u64> {
    let mut roots = if root == 0 {
        match refs {
            Some(count) => store_leaves(builder, &[(key, count)], generation)?,
            None => return Err(Error::CorruptTree),
        }
    } else {
        patch_node(volume, builder, root, key, refs, generation, true)?
    };
    if roots.is_empty() {
        return Ok(0);
    }
    while roots.len() > 1 {
        let level = roots[0].level.checked_add(1).ok_or(Error::TooLarge)?;
        roots = store_branches(builder, &roots, level, generation)?;
    }
    Ok(roots[0].address)
}

/// Traverse a reference tree once, verifying all separators against the
/// actual subtree maxima, global ordering, cycles, and per-block CRCs.
fn inspect<D: BlockDevice>(
    device: &mut D,
    root: u64,
    generation: u64,
    cursor: u64,
) -> Result<(BTreeMap<u64, u16>, BTreeSet<u64>)> {
    if generation == 0 || !valid_address(root, generation, cursor, device.block_count()) {
        return Err(Error::CorruptTree);
    }
    struct Walker<'a, D: BlockDevice> {
        device: &'a mut D,
        generation: u64,
        cursor: u64,
        visited: BTreeSet<u64>,
        counts: BTreeMap<u64, u16>,
        last: Option<u64>,
    }
    impl<D: BlockDevice> Walker<'_, D> {
        fn walk(
            &mut self,
            address: u64,
            expected_level: Option<u8>,
            is_root: bool,
        ) -> Result<Option<u64>> {
            if !valid_address(address, self.generation, self.cursor, self.device.block_count())
                || !self.visited.insert(address)
                || self.visited.len() > MAX_PAGES
            {
                return Err(Error::CorruptTree);
            }
            let mut block = [0; BLOCK_SIZE];
            self.device.read_block(address, &mut block)?;
            let page = decode(
                &block, self.generation, self.cursor, self.device.block_count(),
            )?;
            match page {
                Page::Leaf(rows) => {
                    if expected_level.is_some_and(|level| level != 0)
                        || (rows.is_empty() && !is_root)
                    {
                        return Err(Error::CorruptTree);
                    }
                    for (key, refs) in rows {
                        if self.last.is_some_and(|prev| key <= prev)
                            || self.counts.insert(key, refs).is_some()
                        {
                            return Err(Error::CorruptTree);
                        }
                        self.last = Some(key);
                    }
                    Ok(self.last)
                }
                Page::Branch { level, children } => {
                    if expected_level.is_some_and(|expected| expected != level) {
                        return Err(Error::CorruptTree);
                    }
                    for child in children {
                        let max = self.walk(child.address, Some(child.level), false)?;
                        if max != Some(child.max) {
                            return Err(Error::CorruptTree);
                        }
                    }
                    Ok(self.last)
                }
            }
        }
    }
    let mut walker = Walker {
        device,
        generation,
        cursor,
        visited: BTreeSet::new(),
        counts: BTreeMap::new(),
        last: None,
    };
    walker.walk(root, None, true)?;
    if walker.visited.iter().any(|page| walker.counts.contains_key(page)) {
        return Err(Error::CorruptTree);
    }
    Ok((walker.counts, walker.visited))
}

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
    {
        return Err(Error::CorruptTree);
    }
    let (counts, pages) = inspect(device, header.first, generation, cursor)?;
    if pages.len() != header.pages as usize || counts.len() != header.entries as usize {
        return Err(Error::CorruptTree);
    }
    Ok((counts, pages))
}

fn discard_intermediate(builder: &mut Builder, root: u64) -> Result<()> {
    let staged: BTreeMap<u64, [u8; BLOCK_SIZE]> = builder.staged
        .iter()
        .filter(|(_, b)| &b[..8] == LEAF_MAGIC || &b[..8] == BRANCH_MAGIC)
        .map(|(id, data)| (*id, *data))
        .collect();
    let mut visited = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(addr) = stack.pop() {
        let Some(page) = staged.get(&addr) else { continue };
        if !visited.insert(addr) {
            return Err(Error::CorruptTree);
        }
        if &page[..8] == BRANCH_MAGIC {
            let len = u16_at(page, 28) as usize;
            for i in 0..len {
                stack.push(u64_at(page, HEADER + i * BRANCH_ENTRY + 8));
            }
        }
    }
    builder.staged.retain(|(id, page)| {
        (&page[..8] != LEAF_MAGIC && &page[..8] != BRANCH_MAGIC) || visited.contains(id)
    });
    Ok(())
}

fn seal_root<D: BlockDevice>(
    volume: &mut Volume<D>,
    builder: &mut Builder,
    root: u64,
    generation: u64,
    expected: &BTreeMap<u64, u16>,
) -> Result<RefHeader> {
    let root = if root == 0 {
        builder.push(encode_leaf(&[], generation)?)?
    } else {
        root
    };
    discard_intermediate(builder, root)?;
    let overlay = &mut StagedReader { volume, staged: &builder.staged };
    let (actual, pages) = inspect(overlay, root, generation, builder.cursor)?;
    if &actual != expected || pages.len() > MAX_PAGES || actual.len() > u32::MAX as usize {
        return Err(Error::CorruptTree);
    }
    Ok(RefHeader {
        first: root,
        pages: pages.len() as u32,
        entries: actual.len() as u32,
    })
}

pub(super) fn persist(
    volume: &mut Volume<impl BlockDevice>,
    builder: &mut Builder,
    generation: u64,
    counts: &BTreeMap<u64, u16>,
) -> Result<RefHeader> {
    let rows: Vec<_> = counts.iter().map(|(&k, &v)| (k, v)).collect();
    let mut level = 0_u8;
    let mut children = Vec::new();
    if rows.is_empty() {
        return seal_root(volume, builder, 0, generation, counts);
    }
    for chunk in rows.chunks(LEAF_CAPACITY) {
        children.extend(store_leaves(builder, chunk, generation)?);
    }
    while children.len() > 1 {
        level = level.checked_add(1).ok_or(Error::TooLarge)?;
        let mut parents = Vec::new();
        for chunk in children.chunks(BRANCH_CAPACITY) {
            parents.extend(store_branches(builder, chunk, level, generation)?);
        }
        children = parents;
    }
    seal_root(volume, builder, children[0].address, generation, counts)
}

/// Apply only changed logical reference records. Unchanged leaves/branches
/// retain their addresses across generations. The full logical map remains an
/// independent cross-check, not a trusted allocator oracle.
pub(super) fn persist_incremental<D: BlockDevice>(
    volume: &mut Volume<D>,
    builder: &mut Builder,
    generation: u64,
    previous: RefHeader,
    prior_counts: &BTreeMap<u64, u16>,
    current: &BTreeMap<u64, u16>,
) -> Result<RefHeader> {
    if previous.first == 0 {
        return persist(volume, builder, generation, current);
    }
    let mut deltas = Vec::<(u64, Option<u16>)>::new();
    for (&key, &refs) in current {
        if prior_counts.get(&key) != Some(&refs) {
            deltas.push((key, Some(refs)));
        }
    }
    for &key in prior_counts.keys() {
        if !current.contains_key(&key) {
            deltas.push((key, None));
        }
    }
    if deltas.is_empty() {
        return Ok(previous);
    }
    deltas.sort_by_key(|(key, _)| *key);
    let mut root = previous.first;
    for (key, refs) in deltas {
        root = patch_one(volume, builder, root, key, refs, generation)?;
    }
    seal_root(volume, builder, root, generation, current)
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
