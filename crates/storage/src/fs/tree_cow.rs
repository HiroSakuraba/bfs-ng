//! Incremental immutable-node path copying for the F2 sorted namespace tree.
//! This is a bounded B+tree-style prototype: leaves may be underfilled after
//! deletion. It is not a production-grade balanced B+tree implementation.
use super::{
    BLOCK_SIZE, BlockDevice, Builder, Error, FANOUT, NODE_MAGIC, ROOT_PAYLOAD_LAST, Reader, Record,
    Result, Volume, encode_branch, encode_leaf, u16_at, u32_at, u64_at, valid_checksum,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
struct Child {
    max: String,
    block: u64,
    level: u8,
}

enum Page {
    Leaf(Vec<(String, Record)>),
    Branch { level: u8, children: Vec<Child> },
}

/// Read an older, persisted node or a newly staged node in the current
/// transaction. CRC and node geometry are rechecked; all device writes remain
/// fenced behind Volume's ordered root publication.
fn read_page<D: BlockDevice>(
    volume: &mut Volume<D>,
    builder: &Builder,
    address: u64,
    generation: u64,
) -> Result<Page> {
    let b = match builder.staged.iter().rev().find(|(n, _)| *n == address) {
        Some((_, b)) => *b,
        None => volume.read_data_block(address)?,
    };
    let level = b[8];
    let count = u16_at(&b, 10) as usize;
    if &b[..8] != NODE_MAGIC
        || !valid_checksum(&b)
        || b[9] != 0
        || b[20..24].iter().any(|n| *n != 0)
        || b[28..32].iter().any(|n| *n != 0)
        || u64_at(&b, 12) == 0
        || u64_at(&b, 12) > generation
        || count == 0
        || count > FANOUT
        || level > 8
    {
        return Err(Error::CorruptTree);
    }
    let mut rd = Reader {
        block: &b,
        offset: 32,
    };
    let mut previous = String::new();
    if level == 0 {
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let key = rd.key().ok_or(Error::CorruptTree)?;
            if key <= previous {
                return Err(Error::CorruptTree);
            }
            let raw = rd.take(41).ok_or(Error::CorruptTree)?;
            let mode = raw[8];
            let inode = u64_at(raw, 0);
            if mode > 1 || inode < 2 {
                return Err(Error::CorruptTree);
            }
            let record = Record {
                inode,
                directory: mode == 1,
                start: u64_at(raw, 9),
                count: u32_at(raw, 17),
                length: u64_at(raw, 21),
                checksum: u32_at(raw, 29),
                birth_generation: u64_at(raw, 33),
            };
            previous = key.clone();
            entries.push((key, record));
        }
        if !rd.done() {
            return Err(Error::CorruptTree);
        }
        Ok(Page::Leaf(entries))
    } else {
        let mut children = Vec::with_capacity(count);
        for _ in 0..count {
            let max = rd.key().ok_or(Error::CorruptTree)?;
            if max <= previous {
                return Err(Error::CorruptTree);
            }
            let raw = rd.take(8).ok_or(Error::CorruptTree)?;
            let address = u64_at(raw, 0);
            if address <= ROOT_PAYLOAD_LAST || address >= volume.block_count() {
                return Err(Error::CorruptTree);
            }
            previous = max.clone();
            children.push(Child {
                max,
                block: address,
                level: level - 1,
            });
        }
        if !rd.done() {
            return Err(Error::CorruptTree);
        }
        Ok(Page::Branch { level, children })
    }
}

fn store_leaf(
    builder: &mut Builder,
    entries: &[(String, Record)],
    generation: u64,
) -> Result<Vec<Child>> {
    let mut children = Vec::new();
    let midpoint = entries.len() / 2;
    let groups: Vec<&[(String, Record)]> = if entries.len() > FANOUT {
        vec![&entries[..midpoint], &entries[midpoint..]]
    } else {
        vec![entries]
    };
    for group in groups {
        let references: Vec<_> = group.iter().map(|(key, val)| (key, val)).collect();
        let page = encode_leaf(&references, generation)?;
        let block = builder.push(page)?;
        children.push(Child {
            max: group.last().expect("nonempty leaf").0.clone(),
            block,
            level: 0,
        });
    }
    Ok(children)
}

fn store_branch(
    builder: &mut Builder,
    entries: &[Child],
    level: u8,
    generation: u64,
) -> Result<Vec<Child>> {
    let mut parents = Vec::new();
    let midpoint = entries.len() / 2;
    let groups: Vec<&[Child]> = if entries.len() > FANOUT {
        vec![&entries[..midpoint], &entries[midpoint..]]
    } else {
        vec![entries]
    };
    for group in groups {
        if group.iter().any(|child| child.level + 1 != level) {
            return Err(Error::CorruptTree);
        }
        let keys: Vec<_> = group
            .iter()
            .map(|child| (child.max.clone(), child.block))
            .collect();
        let page = encode_branch(&keys, level, generation)?;
        let block = builder.push(page)?;
        parents.push(Child {
            max: group.last().expect("nonempty branch").max.clone(),
            block,
            level,
        });
    }
    Ok(parents)
}

const MIN_OCCUPANCY: usize = FANOUT / 2;

fn page_len(page: &Page) -> usize {
    match page {
        Page::Leaf(entries) => entries.len(),
        Page::Branch { children, .. } => children.len(),
    }
}

/// Rebalance adjacent siblings through CoW copies. An underfilled page
/// borrows by redistributing the combined ordered contents; two pages
/// merge when they fit in one. Neither old page is modified in place.
fn redistribute_pair<D: BlockDevice>(
    volume: &mut Volume<D>,
    builder: &mut Builder,
    left: &Child,
    right: &Child,
    generation: u64,
) -> Result<Vec<Child>> {
    if left.level != right.level || left.max >= right.max {
        return Err(Error::CorruptTree);
    }
    let a = read_page(volume, builder, left.block, generation)?;
    let b = read_page(volume, builder, right.block, generation)?;
    match (a, b) {
        (Page::Leaf(mut first), Page::Leaf(second)) => {
            if first.last().is_none_or(|(key, _)| key >= &second[0].0) {
                return Err(Error::CorruptTree);
            }
            first.extend(second);
            store_leaf(builder, &first, generation)
        }
        (
            Page::Branch {
                level: la,
                children: mut first,
            },
            Page::Branch {
                level: lb,
                children: second,
            },
        ) if la == lb => {
            if first
                .last()
                .is_none_or(|child| child.max.as_str() >= second[0].max.as_str())
            {
                return Err(Error::CorruptTree);
            }
            first.extend(second);
            store_branch(builder, &first, la, generation)
        }
        _ => Err(Error::CorruptTree),
    }
}

/// Repair only siblings near the modified path, preserving physical sharing
/// of unrelated branches. The root may have fewer than MIN_OCCUPANCY entries;
/// non-root pages directly affected by deletion are redistributed/merged.
fn rebalance_near<D: BlockDevice>(
    volume: &mut Volume<D>,
    builder: &mut Builder,
    children: &mut Vec<Child>,
    modified_at: usize,
    generation: u64,
) -> Result<()> {
    if children.len() < 2 {
        return Ok(());
    }
    let mut index = modified_at.saturating_sub(1);
    let mut end = (modified_at + 2).min(children.len());
    while index < end && children.len() > 1 {
        let page = read_page(volume, builder, children[index].block, generation)?;
        if page_len(&page) >= MIN_OCCUPANCY {
            index += 1;
            continue;
        }
        let left_index = if index + 1 < children.len() {
            index
        } else {
            index - 1
        };
        let updated = redistribute_pair(
            volume,
            builder,
            &children[left_index],
            &children[left_index + 1],
            generation,
        )?;
        let count = updated.len();
        children.splice(left_index..left_index + 2, updated);
        end = (modified_at + 2).min(children.len());
        if count == 1 {
            index = left_index.saturating_sub(1);
        } else {
            index = left_index + count;
        }
    }
    Ok(())
}

/// Returns replacement siblings at the original node level. Only the leaf
/// containing the changed key and its ancestors are staged; sibling
/// subtrees retain their existing physical addresses and birth generations.
fn patch_node<D: BlockDevice>(
    volume: &mut Volume<D>,
    builder: &mut Builder,
    address: u64,
    key: &str,
    replacement: Option<&Record>,
    generation: u64,
    root: bool,
) -> Result<Vec<Child>> {
    match read_page(volume, builder, address, generation)? {
        Page::Leaf(mut entries) => {
            match entries.binary_search_by(|(path, _)| path.as_str().cmp(key)) {
                Ok(index) => {
                    if let Some(value) = replacement {
                        entries[index].1 = value.clone();
                    } else {
                        entries.remove(index);
                    }
                }
                Err(index) => {
                    let value = replacement.ok_or(Error::CorruptTree)?;
                    entries.insert(index, (key.to_owned(), value.clone()));
                }
            }
            if entries.is_empty() {
                return Ok(Vec::new());
            }
            store_leaf(builder, &entries, generation)
        }
        Page::Branch {
            level,
            mut children,
        } => {
            let index = children
                .partition_point(|child| child.max.as_str() < key)
                .min(children.len() - 1);
            let modified = patch_node(
                volume,
                builder,
                children[index].block,
                key,
                replacement,
                generation,
                false,
            )?;
            children.splice(index..=index, modified);
            if children.is_empty() {
                return Ok(Vec::new());
            }
            let touched = index.min(children.len() - 1);
            rebalance_near(volume, builder, &mut children, touched, generation)?;
            // Deletions may collapse a root with just one child; this avoids
            // keeping tall single-child root chains indefinitely.
            if root && children.len() == 1 {
                return Ok(children);
            }
            store_branch(builder, &children, level, generation)
        }
    }
}

pub(super) fn patch_tree<D: BlockDevice>(
    volume: &mut Volume<D>,
    builder: &mut Builder,
    root: u64,
    key: &str,
    replacement: Option<&Record>,
    generation: u64,
) -> Result<u64> {
    if root == 0 {
        let Some(record) = replacement else {
            return Err(Error::CorruptTree);
        };
        return Ok(store_leaf(builder, &[(key.to_owned(), record.clone())], generation)?[0].block);
    }
    let mut replacement_roots =
        patch_node(volume, builder, root, key, replacement, generation, true)?;
    if replacement_roots.is_empty() {
        return Ok(0);
    }
    while replacement_roots.len() > 1 {
        let next_level = replacement_roots[0]
            .level
            .checked_add(1)
            .ok_or(Error::TooLarge)?;
        if next_level > 8 {
            return Err(Error::TooLarge);
        }
        replacement_roots = store_branch(builder, &replacement_roots, next_level, generation)?;
    }
    Ok(replacement_roots[0].block)
}

/// Read a bounded lexicographic range directly from immutable F3 pages,
/// without consulting FileSystem's materialized in-memory record map.
/// Each leaf and branch is checksum-checked while traversing.
pub(super) fn scan_pages<D: BlockDevice>(
    volume: &mut Volume<D>,
    root: u64,
    generation: u64,
    cursor: u64,
    prefix: &str,
    after: Option<&str>,
    count: usize,
) -> Result<(Vec<(String, Record)>, usize)> {
    if root == 0 {
        return Ok((Vec::new(), 0));
    }
    let empty = Builder::new(cursor, generation.saturating_add(1), BTreeSet::new());
    let mut result = Vec::new();
    let mut visited = BTreeSet::new();
    let mut reads = 0;
    let wanted = count.checked_add(1).ok_or(Error::TooLarge)?;

    struct Walker<'a, D: BlockDevice> {
        volume: &'a mut Volume<D>,
        empty: &'a Builder,
        generation: u64,
        cursor: u64,
        prefix: &'a str,
        after: Option<&'a str>,
        wanted: usize,
        finished_prefix: bool,
        visited: &'a mut BTreeSet<u64>,
        reads: &'a mut usize,
        result: &'a mut Vec<(String, Record)>,
    }
    impl<D: BlockDevice> Walker<'_, D> {
        fn walk(&mut self, address: u64, level: Option<u8>) -> Result<()> {
            if self.result.len() >= self.wanted || self.finished_prefix {
                return Ok(());
            }
            if address < self.cursor
                || address <= self.generation.saturating_add(1)
                || !self.visited.insert(address)
                || self.visited.len() > 4096
            {
                return Err(Error::CorruptTree);
            }
            let page = read_page(self.volume, self.empty, address, self.generation)?;
            *self.reads += 1;
            match page {
                Page::Leaf(entries) => {
                    if level.is_some_and(|expected| expected != 0) {
                        return Err(Error::CorruptTree);
                    }
                    for (key, record) in entries {
                        if key == super::INTERNAL_PATH
                            || self.after.is_some_and(|after| key.as_str() <= after)
                            || key.as_str() < self.prefix
                        {
                            continue;
                        }
                        if !key.starts_with(self.prefix) {
                            // All following keys are greater: prefix results
                            // form one contiguous lexicographic interval.
                            self.finished_prefix = true;
                            break;
                        }
                        self.result.push((key, record));
                        if self.result.len() >= self.wanted {
                            break;
                        }
                    }
                }
                Page::Branch {
                    level: actual,
                    children,
                } => {
                    if level.is_some_and(|expected| expected != actual) {
                        return Err(Error::CorruptTree);
                    }
                    for child in children {
                        if child.max.as_str() < self.prefix
                            || self.after.is_some_and(|after| child.max.as_str() <= after)
                        {
                            continue;
                        }
                        self.walk(child.block, Some(actual - 1))?;
                        if self.result.len() >= self.wanted || self.finished_prefix {
                            break;
                        }
                    }
                }
            }
            Ok(())
        }
    }

    Walker {
        volume,
        empty: &empty,
        generation,
        cursor,
        prefix,
        after,
        wanted,
        finished_prefix: false,
        visited: &mut visited,
        reads: &mut reads,
        result: &mut result,
    }
    .walk(root, None)?;

    Ok((result, reads))
}

/// Drop intermediate nodes superseded by later edits in the same transaction.
/// Newly allocated file-content chunks are never removed by this pass.
pub(super) fn discard_superseded_nodes(builder: &mut Builder, root: u64) -> Result<usize> {
    let staged: BTreeMap<u64, [u8; BLOCK_SIZE]> = builder
        .staged
        .iter()
        .filter(|(_, b)| &b[..8] == NODE_MAGIC)
        .map(|(addr, b)| (*addr, *b))
        .collect();
    let mut reachable = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(addr) = stack.pop() {
        let Some(node) = staged.get(&addr) else {
            continue;
        };
        if !reachable.insert(addr) {
            return Err(Error::CorruptTree);
        }
        if node[8] > 0 {
            let mut rd = Reader {
                block: node,
                offset: 32,
            };
            for _ in 0..u16_at(node, 10) {
                let _key = rd.key().ok_or(Error::CorruptTree)?;
                let child = rd.take(8).ok_or(Error::CorruptTree)?;
                stack.push(u64_at(child, 0));
            }
        }
    }
    builder
        .staged
        .retain(|(addr, page)| &page[..8] != NODE_MAGIC || reachable.contains(addr));
    Ok(reachable.len())
}
