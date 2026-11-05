//! F5.1 experimental localized CoW updates to the physical checksum B+tree.
//! Changed keys copy only leaf-to-root paths; unchanged subtrees are shared.
//! The planner performs no device writes or space certification.
use super::{
    BlockPointer, Item, MAX_LEVEL, PAGE_HEADER_BYTES, Page, StagedChecksums, TreeId, read_page,
};
use crate::volume::ROOT_PAYLOAD_LAST;
use crate::{BLOCK_SIZE, BlockDevice, Error, Result};
use std::collections::BTreeMap;

struct Child { max_key: Vec<u8>, pointer: BlockPointer }
struct Planner<'a, D: BlockDevice> {
    device: &'a mut D,
    volume_id: u128,
    generation: u64,
    reservations: &'a [u64],
    used: usize,
    staged: Vec<(u64, [u8; BLOCK_SIZE])>,
}
impl<D: BlockDevice> Planner<'_, D> {
    fn write_page(&mut self, level: u8, items: Vec<Item>) -> Result<Child> {
        let address = *self.reservations.get(self.used).ok_or(Error::NoSpace)?;
        let page = Page {
            tree: TreeId::Checksum, level, birth: self.generation,
            address, volume_id: self.volume_id, items,
        };
        let bytes = page.encode()?;
        let pointer = page.pointer()?;
        let max_key = page.items.last().ok_or(Error::CorruptTree)?.key.clone();
        self.staged.push((address, bytes));
        self.used += 1;
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
                out.push(self.write_page(level, std::mem::take(&mut current))?);
                size = PAGE_HEADER_BYTES;
            }
            size += cost;
            current.push(item);
        }
        if !current.is_empty() {
            out.push(self.write_page(level, current)?);
        }
        Ok(out)
    }
    fn edit_node(
        &mut self, pointer: BlockPointer, changes: &[(u64, Option<u32>)],
    ) -> Result<Vec<Child>> {
        if pointer.birth >= self.generation {
            return Err(Error::CorruptTree);
        }
        let page = read_page(
            self.device, pointer, TreeId::Checksum, self.volume_id, self.generation - 1,
        )?;
        if page.level == 0 {
            let mut old = BTreeMap::new();
            for item in page.items {
                if item.key.len() != 8 || item.value.len() != 8
                    || item.value[..2] != [1, 0] || item.value[2..4] != [0; 2]
                {
                    return Err(Error::CorruptTree);
                }
                let key = u64::from_be_bytes(
                    item.key[..8].try_into().map_err(|_| Error::CorruptTree)?,
                );
                let crc = u32::from_le_bytes(
                    item.value[4..8].try_into().map_err(|_| Error::CorruptTree)?,
                );
                if old.insert(key, crc).is_some() {
                    return Err(Error::CorruptTree);
                }
            }
            for &(address, crc) in changes {
                match crc {
                    Some(expected) => {
                        if old.get(&address).is_some_and(|existing| *existing != expected) {
                            return Err(Error::CorruptTree);
                        }
                        old.insert(address, expected);
                    }
                    None => {
                        if old.remove(&address).is_none() {
                            return Err(Error::CorruptTree);
                        }
                    }
                }
            }
            let items = old.into_iter().map(|(address, crc)| {
                let mut value = vec![0_u8; 8];
                value[..2].copy_from_slice(&1_u16.to_le_bytes());
                value[4..8].copy_from_slice(&crc.to_le_bytes());
                Item { key: address.to_be_bytes().to_vec(), value }
            }).collect();
            return self.pack(0, items);
        }
        if page.items.is_empty() {
            return Err(Error::CorruptTree);
        }
        let mut groups = vec![Vec::new(); page.items.len()];
        for &(key, checksum) in changes {
            let target = match page.items.binary_search_by(
                |item| item.key.as_slice().cmp(&key.to_be_bytes()),
            ) {
                Ok(i) | Err(i) if i < page.items.len() => i,
                Ok(_) | Err(_) => page.items.len() - 1,
            };
            groups[target].push((key, checksum));
        }
        let mut nodes = Vec::new();
        for (item, mutations) in page.items.into_iter().zip(groups) {
            let child = BlockPointer::decode_required(&item.value)?;
            if mutations.is_empty() {
                nodes.push(Child { max_key: item.key, pointer: child });
            } else {
                nodes.extend(self.edit_node(child, &mutations)?);
            }
        }
        let items = nodes.into_iter().map(|child| Item {
            key: child.max_key, value: child.pointer.encode().to_vec(),
        }).collect();
        self.pack(page.level, items)
    }
}

/// Applies an exact per-physical-block checksum delta using CoW path copy.
/// Some(crc) is an insertion, None is a deletion. No in-place CRC updates
/// are allowed for a still-referenced physical address.
///
/// Reservations are only geometry-validated; the caller must independently
/// prove virgin ownership, verify resulting tree, and publish through F0.
pub fn stage_checksum_delta<D: BlockDevice>(
    device: &mut D,
    old_root: Option<BlockPointer>,
    changes: &BTreeMap<u64, Option<u32>>,
    reservations: &[u64],
    volume_id: u128,
    new_generation: u64,
    high_water: u64,
) -> Result<StagedChecksums> {
    if volume_id == 0 || new_generation <= 1
        || high_water <= ROOT_PAYLOAD_LAST || high_water > device.block_count()
        || reservations.iter().any(|&n| n <= ROOT_PAYLOAD_LAST || n >= high_water)
        || reservations.windows(2).any(|w| w[0] <= w[1])
        || changes.keys().any(|&n| n <= ROOT_PAYLOAD_LAST || n >= device.block_count())
    {
        return Err(Error::Geometry);
    }
    if changes.is_empty() {
        return Ok(StagedChecksums {
            root: old_root, staged_pages: Vec::new(), consumed_reservations: 0,
        });
    }
    let mut planner = Planner {
        device, volume_id, generation: new_generation,
        reservations, used: 0, staged: Vec::new(),
    };
    let mut children = if let Some(root) = old_root {
        root.validate(planner.device.block_count(), new_generation - 1, high_water)?;
        let patches: Vec<_> = changes.iter().map(|(&k, &v)| (k, v)).collect();
        planner.edit_node(root, &patches)?
    } else {
        let mut items = Vec::new();
        for (&address, &checksum) in changes {
            let crc = checksum.ok_or(Error::CorruptTree)?;
            let mut value = vec![0_u8; 8];
            value[..2].copy_from_slice(&1_u16.to_le_bytes());
            value[4..8].copy_from_slice(&crc.to_le_bytes());
            items.push(Item { key: address.to_be_bytes().to_vec(), value });
        }
        planner.pack(0, items)?
    };
    if children.is_empty() {
        return Ok(StagedChecksums {
            root: None, staged_pages: planner.staged,
            consumed_reservations: planner.used,
        });
    }
    let mut level = children[0].pointer.level;
    while children.len() > 1 {
        if level >= MAX_LEVEL { return Err(Error::TooLarge); }
        level += 1;
        let items = children.into_iter().map(|child| Item {
            key: child.max_key, value: child.pointer.encode().to_vec(),
        }).collect();
        children = planner.pack(level, items)?;
    }
    Ok(StagedChecksums {
        root: Some(children.pop().ok_or(Error::CorruptTree)?.pointer),
        staged_pages: planner.staged,
        consumed_reservations: planner.used,
    })
}
