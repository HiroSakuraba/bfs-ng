//! F5.1 experimental single-file, block-granular extent map.
//!
//! A 4KiB v10 Fs leaf describes one sparse file: each extent key is its
//! logical block start in big-endian order; value stores physical start,
//! block count, and allocation birth. The last 8-byte key (u64::MAX) stores
//! logical byte length. This deliberately avoids claiming a general inode
//! table or multilevel fs B+tree: MAX_FILE_BLOCKS bounds the prototype.
use super::{BlockPointer, Item, Page, TreeId, read_page};
use crate::volume::ROOT_PAYLOAD_LAST;
use crate::{BLOCK_SIZE, BlockDevice, Error, Result};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_FILE_BLOCKS: u64 = 64;
pub const MAX_FILE_BYTES: u64 = MAX_FILE_BLOCKS * BLOCK_SIZE as u64;
/// Larger but still explicitly bounded unified-v10 file limit: 1 GiB.
/// The standalone F5.1 ExtentDataWriter retains its original 256 KiB bound.
pub const MAX_UNIFIED_FILE_BLOCKS: u64 = 262_144;
pub const MAX_UNIFIED_FILE_BYTES: u64 = MAX_UNIFIED_FILE_BLOCKS * BLOCK_SIZE as u64;
pub const MAX_UNIFIED_FILE_PAGES: usize = 32_768;
const LENGTH_KEY: [u8; 8] = [0xff; 8];
const EXTENT_BYTES: usize = 24;

/// One data block's immutable physical incarnation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockRef {
    pub physical: u64,
    pub birth: u64,
}

/// A contiguous run of physical blocks for consecutive logical blocks.
/// No clones, PREALLOC, compression, or sharing outside pinned CoW roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub logical: u64,
    pub physical: u64,
    pub blocks: u32,
    pub birth: u64,
}
impl Extent {
    fn encode(self) -> [u8; EXTENT_BYTES] {
        let mut out = [0_u8; EXTENT_BYTES];
        out[..8].copy_from_slice(&self.physical.to_le_bytes());
        out[8..12].copy_from_slice(&self.blocks.to_le_bytes());
        out[16..24].copy_from_slice(&self.birth.to_le_bytes());
        out
    }
    fn decode(key: &[u8], bytes: &[u8], page_birth: u64, total: u64) -> Result<Self> {
        Self::decode_with_limit(key, bytes, page_birth, total, MAX_FILE_BLOCKS)
    }

    fn decode_with_limit(
        key: &[u8],
        bytes: &[u8],
        page_birth: u64,
        total: u64,
        limit: u64,
    ) -> Result<Self> {
        if key.len() != 8
            || key == LENGTH_KEY
            || bytes.len() != EXTENT_BYTES
            || bytes[12..16] != [0; 4]
        {
            return Err(Error::CorruptTree);
        }
        let e = Self {
            logical: u64::from_be_bytes(key.try_into().map_err(|_| Error::CorruptTree)?),
            physical: u64::from_le_bytes(bytes[..8].try_into().map_err(|_| Error::CorruptTree)?),
            blocks: u32::from_le_bytes(bytes[8..12].try_into().map_err(|_| Error::CorruptTree)?),
            birth: u64::from_le_bytes(bytes[16..24].try_into().map_err(|_| Error::CorruptTree)?),
        };
        if e.logical >= limit
            || e.blocks == 0
            || e.blocks as u64 > limit
            || e.logical
                .checked_add(u64::from(e.blocks))
                .is_none_or(|end| end > limit)
            || e.physical <= ROOT_PAYLOAD_LAST
            || e.physical
                .checked_add(u64::from(e.blocks))
                .is_none_or(|end| end > total)
            || e.birth == 0
            || e.birth > page_birth
        {
            return Err(Error::CorruptTree);
        }
        Ok(e)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileExtents {
    pub size: u64,
    /// Expanded block map; the on-disk format coalesces physically contiguous
    /// blocks with the same birth generation.
    pub blocks: BTreeMap<u64, BlockRef>,
}
impl FileExtents {
    pub fn decode(page: &Page, total: u64) -> Result<Self> {
        if page.tree != TreeId::Fs || page.level != 0 {
            return Err(Error::CorruptTree);
        }
        if page.items.is_empty() {
            return Ok(Self::default());
        }
        let mut file = Self::default();
        let mut found_size = false;
        let mut physical_seen = BTreeSet::new();
        let mut pending = Vec::new();
        for item in &page.items {
            if item.key == LENGTH_KEY {
                if found_size || item.value.len() != 8 {
                    return Err(Error::CorruptTree);
                }
                found_size = true;
                file.size =
                    u64::from_le_bytes(item.value[..8].try_into().map_err(|_| Error::CorruptTree)?);
                continue;
            }
            let e = Extent::decode(&item.key, &item.value, page.birth, total)?;
            pending.push(e);
        }
        if !found_size || file.size > MAX_FILE_BYTES {
            return Err(Error::CorruptTree);
        }
        let logical_limit = file.size.div_ceil(BLOCK_SIZE as u64);
        for e in pending {
            if e.logical + u64::from(e.blocks) > logical_limit {
                return Err(Error::CorruptTree);
            }
            for i in 0..u64::from(e.blocks) {
                let logical = e.logical + i;
                let physical = e.physical + i;
                if file
                    .blocks
                    .insert(
                        logical,
                        BlockRef {
                            physical,
                            birth: e.birth,
                        },
                    )
                    .is_some()
                    || !physical_seen.insert(physical)
                {
                    return Err(Error::CorruptTree);
                }
            }
        }
        Ok(file)
    }

    pub fn encode_items(&self, generation: u64, total: u64) -> Result<Vec<Item>> {
        self.encode_items_with_limit(generation, total, MAX_FILE_BLOCKS)
    }

    /// Canonical extent records for the unified v10 multilevel file tree.
    /// This method never stages bytes: the caller path-copies only changed
    /// leaf entries using independently certified CoW reservations.
    pub fn encode_large_items(&self, generation: u64, total: u64) -> Result<Vec<Item>> {
        self.encode_items_with_limit(generation, total, MAX_UNIFIED_FILE_BLOCKS)
    }

    fn encode_items_with_limit(
        &self,
        generation: u64,
        total: u64,
        limit: u64,
    ) -> Result<Vec<Item>> {
        if self.size > limit * BLOCK_SIZE as u64 {
            return Err(Error::TooLarge);
        }
        let logical_limit = self.size.div_ceil(BLOCK_SIZE as u64);
        let mut previous: Option<Extent> = None;
        let mut extents = Vec::new();
        let mut physical_seen = BTreeSet::new();
        for (&logical, &reference) in &self.blocks {
            if logical >= logical_limit
                || logical >= limit
                || reference.physical <= ROOT_PAYLOAD_LAST
                || reference.physical >= total
                || reference.birth == 0
                || reference.birth > generation
                || !physical_seen.insert(reference.physical)
            {
                return Err(Error::CorruptTree);
            }
            if let Some(e) = &mut previous {
                if e.logical + u64::from(e.blocks) == logical
                    && e.physical + u64::from(e.blocks) == reference.physical
                    && e.birth == reference.birth
                    && u64::from(e.blocks) < limit
                {
                    e.blocks += 1;
                    continue;
                }
                extents.push(*e);
            }
            previous = Some(Extent {
                logical,
                physical: reference.physical,
                blocks: 1,
                birth: reference.birth,
            });
        }
        if let Some(e) = previous {
            extents.push(e);
        }
        let mut items = Vec::new();
        for extent in extents {
            items.push(Item {
                key: extent.logical.to_be_bytes().to_vec(),
                value: extent.encode().to_vec(),
            });
        }
        // A separate size item preserves trailing sparse holes.
        items.push(Item {
            key: LENGTH_KEY.to_vec(),
            value: self.size.to_le_bytes().to_vec(),
        });
        Ok(items)
    }
}

/// All authenticated metadata pages of a single v10 file extent tree.
/// The legacy 4 KiB file leaf is a valid one-page instance. A multilevel
/// root contains separator-max keys and 24-byte checksummed child pointers.
#[derive(Clone, Debug)]
pub struct FileTreeScan {
    pub file: FileExtents,
    pub metadata_pages: BTreeSet<u64>,
    pub leaf_items: BTreeMap<Vec<u8>, Vec<u8>>,
}

struct FileWalker<'a, D: BlockDevice> {
    disk: &'a mut D,
    id: u128,
    generation: u64,
    seen: BTreeSet<u64>,
    items: BTreeMap<Vec<u8>, Vec<u8>>,
    extents: Vec<Extent>,
    size: Option<u64>,
}
impl<D: BlockDevice> FileWalker<'_, D> {
    fn visit(&mut self, root: BlockPointer) -> Result<(Vec<u8>, Vec<u8>)> {
        if self.seen.len() >= MAX_UNIFIED_FILE_PAGES || !self.seen.insert(root.address) {
            return Err(Error::CorruptTree);
        }
        let page = read_page(self.disk, root, TreeId::Fs, self.id, self.generation)?;
        if page.items.is_empty() {
            return Err(Error::CorruptTree);
        }
        let mut first = None;
        let mut last: Option<Vec<u8>> = None;
        if page.level == 0 {
            for item in page.items {
                validate_large_item(&item.key, &item.value, self.disk.block_count())?;
                if item.key == LENGTH_KEY {
                    if self
                        .size
                        .replace(u64::from_le_bytes(
                            item.value[..8].try_into().map_err(|_| Error::CorruptTree)?,
                        ))
                        .is_some()
                    {
                        return Err(Error::CorruptTree);
                    }
                } else {
                    self.extents.push(Extent::decode_with_limit(
                        &item.key,
                        &item.value,
                        page.birth,
                        self.disk.block_count(),
                        MAX_UNIFIED_FILE_BLOCKS,
                    )?);
                }
                if self.items.len() > MAX_UNIFIED_FILE_BLOCKS as usize
                    || self.items.insert(item.key.clone(), item.value).is_some()
                {
                    return Err(Error::CorruptTree);
                }
                if first.is_none() {
                    first = Some(item.key.clone());
                }
                last = Some(item.key);
            }
        } else {
            for item in page.items {
                let child = BlockPointer::decode_required(&item.value)?;
                let (min, max) = self.visit(child)?;
                if max != item.key || last.as_ref().is_some_and(|p| p >= &min) {
                    return Err(Error::CorruptTree);
                }
                if first.is_none() {
                    first = Some(min);
                }
                last = Some(max);
            }
        }
        Ok((
            first.ok_or(Error::CorruptTree)?,
            last.ok_or(Error::CorruptTree)?,
        ))
    }
}

/// Only one reserved LENGTH_KEY may occur in the complete file B+tree.
/// All ordinary extent keys are big-endian logical starts and are validated
/// against volume geometry and the independent per-file byte-size record.
pub(crate) fn validate_large_item(key: &[u8], value: &[u8], total: u64) -> Result<()> {
    if key == LENGTH_KEY {
        if value.len() != 8
            || u64::from_le_bytes(value.try_into().map_err(|_| Error::CorruptTree)?)
                > MAX_UNIFIED_FILE_BYTES
        {
            return Err(Error::CorruptTree);
        }
    } else {
        Extent::decode_with_limit(key, value, u64::MAX, total, MAX_UNIFIED_FILE_BLOCKS)?;
    }
    Ok(())
}

/// Read all authenticated extent paths and independently reject duplicate
/// physical references, overlapping logical ranges, wrong separator-max keys,
/// absent/duplicate size items, cycles and malformed page/header identities.
/// This compatibility scanner intentionally does not use a cached Free map.
pub fn read_file_tree<D: BlockDevice>(
    disk: &mut D,
    root: BlockPointer,
    id: u128,
    generation: u64,
) -> Result<FileTreeScan> {
    let mut walker = FileWalker {
        disk,
        id,
        generation,
        seen: BTreeSet::new(),
        items: BTreeMap::new(),
        extents: Vec::new(),
        size: None,
    };
    walker.visit(root)?;
    let size = walker.size.ok_or(Error::CorruptTree)?;
    if size > MAX_UNIFIED_FILE_BYTES {
        return Err(Error::CorruptTree);
    }
    let logical_limit = size.div_ceil(BLOCK_SIZE as u64);
    let mut file = FileExtents {
        size,
        blocks: BTreeMap::new(),
    };
    let mut physically_seen = BTreeSet::new();
    for e in walker.extents {
        if e.logical
            .checked_add(u64::from(e.blocks))
            .is_none_or(|end| end > logical_limit)
        {
            return Err(Error::CorruptTree);
        }
        for delta in 0..u64::from(e.blocks) {
            let logical = e.logical + delta;
            let physical = e.physical + delta;
            if file
                .blocks
                .insert(
                    logical,
                    BlockRef {
                        physical,
                        birth: e.birth,
                    },
                )
                .is_some()
                || !physically_seen.insert(physical)
            {
                return Err(Error::CorruptTree);
            }
        }
    }
    if !walker.seen.is_disjoint(&physically_seen) {
        return Err(Error::CorruptTree);
    }
    Ok(FileTreeScan {
        file,
        metadata_pages: walker.seen,
        leaf_items: walker.items,
    })
}

/// Exact key deltas between the pre-commit file leaf set and the new encoded
/// extent set. Unchanged B+tree child pointers may be shared by the planner.
pub fn file_item_deltas(
    old: &BTreeMap<Vec<u8>, Vec<u8>>,
    new: &[Item],
) -> Result<BTreeMap<Vec<u8>, Option<Vec<u8>>>> {
    let mut target = BTreeMap::new();
    for item in new {
        if target
            .insert(item.key.clone(), item.value.clone())
            .is_some()
        {
            return Err(Error::CorruptTree);
        }
    }
    let mut changes = BTreeMap::new();
    for (key, previous) in old {
        if target.get(key) != Some(previous) {
            changes.insert(key.clone(), target.get(key).cloned());
        }
    }
    for (key, value) in target {
        if !old.contains_key(&key) {
            changes.insert(key, Some(value));
        }
    }
    Ok(changes)
}
