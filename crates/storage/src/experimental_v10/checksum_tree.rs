//! F5.0b: physical CRC32C checksums for headerless 4 KiB data.
//! The v10 checksum B+tree is immutable and parent-BP-authenticated.
//! This initial version supports count=1 run items and CoW *bulk rebuild*;
//! incremental checksum path-copy is explicitly deferred.
use super::{BlockPointer, Item, MAX_LEVEL, PAGE_HEADER_BYTES, Page, TreeId, read_page};
use crate::volume::{ROOT_PAYLOAD_LAST, crc32c};
use crate::{BLOCK_SIZE, BlockDevice, Error, Result};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_CHECKSUM_PAGES: usize = 8192;
pub const MAX_CHECKSUM_BLOCKS: usize = 131072;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChecksumScan {
    pub checksums: BTreeMap<u64, u32>,
    pub metadata_pages: BTreeSet<u64>,
}
struct Walker<'a, D: BlockDevice> {
    device: &'a mut D,
    volume_id: u128,
    generation: u64,
    pages: BTreeSet<u64>,
    checksums: BTreeMap<u64, u32>,
}
impl<D: BlockDevice> Walker<'_, D> {
    fn visit(&mut self, pointer: BlockPointer, root: bool) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        if self.pages.len() >= MAX_CHECKSUM_PAGES || !self.pages.insert(pointer.address) {
            return Err(Error::CorruptTree);
        }
        let page = read_page(
            self.device, pointer, TreeId::Checksum, self.volume_id, self.generation,
        )?;
        if page.items.is_empty() {
            return if root && page.level == 0 { Ok(None) } else { Err(Error::CorruptTree) };
        }
        let (min, max) = if page.level == 0 {
            for item in &page.items {
                if item.key.len() != 8 || item.value.len() != 8 {
                    return Err(Error::CorruptTree);
                }
                let address = u64::from_be_bytes(item.key[..8].try_into().map_err(|_| Error::CorruptTree)?);
                let count = u16::from_le_bytes(item.value[..2].try_into().map_err(|_| Error::CorruptTree)?);
                if address <= ROOT_PAYLOAD_LAST
                    || address >= self.device.block_count() || count != 1
                    || item.value[2..4] != [0, 0]
                    || self.checksums.len() >= MAX_CHECKSUM_BLOCKS
                {
                    return Err(Error::CorruptTree);
                }
                let crc = u32::from_le_bytes(item.value[4..8].try_into().map_err(|_| Error::CorruptTree)?);
                if self.checksums.insert(address, crc).is_some() {
                    return Err(Error::CorruptTree);
                }
            }
            (page.items[0].key.clone(), page.items.last().ok_or(Error::CorruptTree)?.key.clone())
        } else {
            let mut first = None;
            let mut previous: Option<Vec<u8>> = None;
            for item in &page.items {
                if item.key.len() != 8 {
                    return Err(Error::CorruptTree);
                }
                let child = BlockPointer::decode_required(&item.value)?;
                let (child_min, child_max) = self.visit(child, false)?.ok_or(Error::CorruptTree)?;
                if child_max != item.key || previous.as_ref().is_some_and(|last| last >= &child_min) {
                    return Err(Error::CorruptTree);
                }
                if first.is_none() { first = Some(child_min); }
                previous = Some(child_max);
            }
            (first.ok_or(Error::CorruptTree)?, previous.ok_or(Error::CorruptTree)?)
        };
        Ok(Some((min, max)))
    }
}
/// Complete checksum index scan. A caller must compare physical coverage
/// against the union of live and pinned data; CRC alone never proves reuse.
pub fn read_checksum_tree<D: BlockDevice>(
    device: &mut D,
    root: Option<BlockPointer>,
    volume_id: u128,
    generation: u64,
) -> Result<ChecksumScan> {
    if volume_id == 0 || generation == 0 { return Err(Error::CorruptTree); }
    let Some(root) = root else {
        return Ok(ChecksumScan { checksums: BTreeMap::new(), metadata_pages: BTreeSet::new() });
    };
    let mut walker = Walker {
        device, volume_id, generation,
        pages: BTreeSet::new(), checksums: BTreeMap::new(),
    };
    walker.visit(root, true)?;
    Ok(ChecksumScan { checksums: walker.checksums, metadata_pages: walker.pages })
}

pub struct StagedChecksums {
    pub root: Option<BlockPointer>,
    pub staged_pages: Vec<(u64, [u8; BLOCK_SIZE])>,
    pub consumed_reservations: usize,
}
struct Child { max_key: Vec<u8>, pointer: BlockPointer }
struct Builder<'a> {
    generation: u64,
    volume_id: u128,
    addresses: &'a [u64],
    used: usize,
    pages: Vec<(u64, [u8; BLOCK_SIZE])>,
}
impl Builder<'_> {
    fn write(&mut self, level: u8, items: Vec<Item>) -> Result<Child> {
        if self.used >= self.addresses.len() { return Err(Error::NoSpace); }
        let address = self.addresses[self.used];
        let page = Page {
            tree: TreeId::Checksum, level, address, birth: self.generation,
            volume_id: self.volume_id, items,
        };
        let bytes = page.encode()?;
        let pointer = page.pointer()?;
        let max_key = page.items.last().ok_or(Error::CorruptTree)?.key.clone();
        self.used += 1;
        self.pages.push((address, bytes));
        Ok(Child { max_key, pointer })
    }
    fn make_level(&mut self, level: u8, items: Vec<Item>) -> Result<Vec<Child>> {
        let mut out = Vec::new();
        let mut current: Vec<Item> = Vec::new();
        let mut cost = PAGE_HEADER_BYTES;
        for item in items {
            let size = 8 + item.key.len() + item.value.len();
            if size + PAGE_HEADER_BYTES > BLOCK_SIZE { return Err(Error::TooLarge); }
            if cost + size > BLOCK_SIZE {
                out.push(self.write(level, std::mem::take(&mut current))?);
                cost = PAGE_HEADER_BYTES;
            }
            cost += size;
            current.push(item);
        }
        if !current.is_empty() { out.push(self.write(level, current)?); }
        Ok(out)
    }
}
/// Pure, non-publishing CoW builder from an already-certified virgin arena.
pub fn stage_checksum_tree(
    checksums: &BTreeMap<u64, u32>,
    reservations: &[u64],
    volume_id: u128,
    generation: u64,
    high_water: u64,
    total_blocks: u64,
) -> Result<StagedChecksums> {
    if generation == 0 || volume_id == 0 || high_water > total_blocks
        || high_water <= ROOT_PAYLOAD_LAST || checksums.len() > MAX_CHECKSUM_BLOCKS
        || reservations.iter().any(|&x| x <= ROOT_PAYLOAD_LAST || x >= high_water)
        || reservations.windows(2).any(|w| w[0] <= w[1])
    { return Err(Error::Geometry); }
    if checksums.is_empty() {
        return Ok(StagedChecksums { root: None, staged_pages: Vec::new(), consumed_reservations: 0 });
    }
    if checksums.keys().any(|&x| x <= ROOT_PAYLOAD_LAST || x >= total_blocks) {
        return Err(Error::Geometry);
    }
    let mut builder = Builder {
        generation, volume_id, addresses: reservations, used: 0, pages: Vec::new(),
    };
    let items = checksums.iter().map(|(&addr, &crc)| {
        let mut value = vec![0_u8; 8];
        value[..2].copy_from_slice(&1_u16.to_le_bytes());
        value[4..8].copy_from_slice(&crc.to_le_bytes());
        Item { key: addr.to_be_bytes().to_vec(), value }
    }).collect();
    let mut children = builder.make_level(0, items)?;
    let mut level = 0;
    while children.len() > 1 {
        if level >= MAX_LEVEL { return Err(Error::TooLarge); }
        let branches = children.into_iter().map(|child| Item {
            key: child.max_key, value: child.pointer.encode().to_vec(),
        }).collect();
        level += 1;
        children = builder.make_level(level, branches)?;
    }
    Ok(StagedChecksums {
        root: Some(children.pop().ok_or(Error::CorruptTree)?.pointer),
        staged_pages: builder.pages,
        consumed_reservations: builder.used,
    })
}
/// Verify physical data. The separate immutable extent reference must be
/// checked for birth and address by the owning fs reader.
pub fn read_verified_block<D: BlockDevice>(
    device: &mut D,
    physical: u64,
    checksums: &BTreeMap<u64, u32>,
) -> Result<[u8; BLOCK_SIZE]> {
    let expected = *checksums.get(&physical).ok_or(Error::CorruptTree)?;
    let mut data = [0_u8; BLOCK_SIZE];
    device.read_block(physical, &mut data)?;
    if crc32c(&data) != expected { return Err(Error::CorruptTree); }
    Ok(data)
}
