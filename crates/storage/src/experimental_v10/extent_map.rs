//! F5.1 experimental single-file, block-granular extent map.
//!
//! A 4KiB v10 Fs leaf describes one sparse file: each extent key is its
//! logical block start in big-endian order; value stores physical start,
//! block count, and allocation birth. The last 8-byte key (u64::MAX) stores
//! logical byte length. This deliberately avoids claiming a general inode
//! table or multilevel fs B+tree: MAX_FILE_BLOCKS bounds the prototype.
use super::{Item, Page, TreeId};
use crate::volume::ROOT_PAYLOAD_LAST;
use crate::{BLOCK_SIZE, Error, Result};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_FILE_BLOCKS: u64 = 64;
pub const MAX_FILE_BYTES: u64 = MAX_FILE_BLOCKS * BLOCK_SIZE as u64;
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
        if key.len() != 8 || key == LENGTH_KEY || bytes.len() != EXTENT_BYTES
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
        if e.logical >= MAX_FILE_BLOCKS || e.blocks == 0
            || e.blocks as u64 > MAX_FILE_BLOCKS
            || e.logical.checked_add(u64::from(e.blocks)).is_none_or(|end| end > MAX_FILE_BLOCKS)
            || e.physical <= ROOT_PAYLOAD_LAST
            || e.physical.checked_add(u64::from(e.blocks)).is_none_or(|end| end > total)
            || e.birth == 0 || e.birth > page_birth
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
                file.size = u64::from_le_bytes(
                    item.value[..8].try_into().map_err(|_| Error::CorruptTree)?,
                );
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
                if file.blocks.insert(logical, BlockRef { physical, birth: e.birth }).is_some()
                    || !physical_seen.insert(physical)
                {
                    return Err(Error::CorruptTree);
                }
            }
        }
        Ok(file)
    }

    pub fn encode_items(&self, generation: u64, total: u64) -> Result<Vec<Item>> {
        if self.size > MAX_FILE_BYTES {
            return Err(Error::TooLarge);
        }
        let logical_limit = self.size.div_ceil(BLOCK_SIZE as u64);
        let mut previous: Option<Extent> = None;
        let mut extents = Vec::new();
        let mut physical_seen = BTreeSet::new();
        for (&logical, &reference) in &self.blocks {
            if logical >= logical_limit
                || reference.physical <= ROOT_PAYLOAD_LAST
                || reference.physical >= total
                || reference.birth == 0 || reference.birth > generation
                || !physical_seen.insert(reference.physical)
            {
                return Err(Error::CorruptTree);
            }
            if let Some(e) = &mut previous {
                if e.logical + u64::from(e.blocks) == logical
                    && e.physical + u64::from(e.blocks) == reference.physical
                    && e.birth == reference.birth
                    && u64::from(e.blocks) < MAX_FILE_BLOCKS
                {
                    e.blocks += 1;
                    continue;
                }
                extents.push(*e);
            }
            previous = Some(Extent {
                logical, physical: reference.physical, blocks: 1, birth: reference.birth,
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
