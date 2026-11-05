//! Authenticated multilevel v10 snapshot catalog and a CoW staging planner.
//! Neither the scan nor the plan certifies allocation ownership or writes disk.
//! Caller must separately prove reserved pages virgin and publish through F0.
use super::{
    BlockPointer, Item, MAX_LEVEL, PAGE_HEADER_BYTES, Page, SnapshotDescriptor, SnapshotState,
    TreeId, decode_snapshot_leaf, read_page,
};
use crate::volume::ROOT_PAYLOAD_LAST;
use crate::{BLOCK_SIZE, BlockDevice, Error, Result};
use std::collections::BTreeSet;

pub const MAX_SNAPSHOT_TREE_PAGES: usize = 8_192;
pub const MAX_SNAPSHOT_DESCRIPTORS: usize = 131_072;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogScan {
    pub descriptors: Vec<SnapshotDescriptor>,
    pub pages_read: usize,
    /// Every referenced snapshot-catalog metadata page, including branches.
    /// This is *not* an entire filesystem reachability map.
    pub owned_pages: BTreeSet<u64>,
}

struct Walker<'a, D: BlockDevice> {
    device: &'a mut D,
    volume_id: u128,
    generation: u64,
    seen: BTreeSet<u64>,
    descriptors: Vec<SnapshotDescriptor>,
}
impl<D: BlockDevice> Walker<'_, D> {
    fn visit(&mut self, pointer: BlockPointer, root: bool) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        if self.seen.len() >= MAX_SNAPSHOT_TREE_PAGES || !self.seen.insert(pointer.address) {
            return Err(Error::CorruptTree);
        }
        let page = read_page(
            self.device,
            pointer,
            TreeId::Snapshots,
            self.volume_id,
            self.generation,
        )?;
        if page.items.is_empty() {
            return if root && page.level == 0 {
                Ok(None)
            } else {
                Err(Error::CorruptTree)
            };
        }
        if page.level == 0 {
            let decoded = decode_snapshot_leaf(&page, self.generation, self.device.block_count())?;
            if decoded.len() > MAX_SNAPSHOT_DESCRIPTORS.saturating_sub(self.descriptors.len()) {
                return Err(Error::CorruptTree);
            }
            self.descriptors.extend(decoded);
            return Ok(Some((
                page.items[0].key.clone(),
                page.items.last().ok_or(Error::CorruptTree)?.key.clone(),
            )));
        }
        let mut low = None;
        let mut previous_max: Option<Vec<u8>> = None;
        for item in &page.items {
            if item.key.len() != 8 {
                return Err(Error::CorruptTree);
            }
            let child = BlockPointer::decode_required(&item.value)?;
            let (min, max) = self.visit(child, false)?.ok_or(Error::CorruptTree)?;
            // High-key B+tree: parent key is exact maximum of child subtree.
            if max != item.key || previous_max.as_ref().is_some_and(|last| last >= &min) {
                return Err(Error::CorruptTree);
            }
            if low.is_none() {
                low = Some(min);
            }
            previous_max = Some(max);
        }
        Ok(Some((
            low.ok_or(Error::CorruptTree)?,
            previous_max.ok_or(Error::CorruptTree)?,
        )))
    }
}

/// Validates all catalog leaves and branch separators, limits traversal,
/// rejects duplicate physical node IDs, and returns globally sorted pins.
/// This does not validate whole namespace/data trees or certify free space.
pub fn read_snapshot_tree<D: BlockDevice>(
    device: &mut D,
    root: Option<BlockPointer>,
    volume_id: u128,
    generation: u64,
) -> Result<CatalogScan> {
    if volume_id == 0 || generation == 0 {
        return Err(Error::CorruptTree);
    }
    let Some(root) = root else {
        return Ok(CatalogScan {
            descriptors: Vec::new(),
            pages_read: 0,
            owned_pages: BTreeSet::new(),
        });
    };
    let mut walker = Walker {
        device,
        volume_id,
        generation,
        seen: BTreeSet::new(),
        descriptors: Vec::new(),
    };
    walker.visit(root, true)?;
    Ok(CatalogScan {
        descriptors: walker.descriptors,
        pages_read: walker.seen.len(),
        owned_pages: walker.seen,
    })
}

/// Dropping a pin entirely remains forbidden until the F5.3 last-owner
/// reclamation proof. MarkDropping retains its historical root pointers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogEdit {
    Pin(SnapshotDescriptor),
    MarkDropping(u64),
}
impl CatalogEdit {
    fn generation(self) -> u64 {
        match self {
            Self::Pin(desc) => desc.generation,
            Self::MarkDropping(generation) => generation,
        }
    }
}

/// Unpublished pages and a candidate root. Does not authorize reusing space.
pub struct StagedCatalog {
    pub root: BlockPointer,
    pub staged_pages: Vec<(u64, [u8; BLOCK_SIZE])>,
    pub high_water: u64,
    pub consumed_reservations: usize,
}
struct Child {
    max_key: Vec<u8>,
    pointer: BlockPointer,
}
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
        if self.used >= self.reservations.len() {
            return Err(Error::NoSpace);
        }
        let address = self.reservations[self.used];
        let page = Page {
            tree: TreeId::Snapshots,
            level,
            birth: self.generation,
            address,
            volume_id: self.volume_id,
            items,
        };
        let bytes = page.encode()?;
        let pointer = page.pointer()?;
        let max_key = page.items.last().ok_or(Error::CorruptTree)?.key.clone();
        self.used += 1;
        self.staged.push((address, bytes));
        Ok(Child { max_key, pointer })
    }

    fn stage_items(&mut self, level: u8, items: Vec<Item>) -> Result<Vec<Child>> {
        if items.is_empty() {
            return Err(Error::CorruptTree);
        }
        let used_bytes = |items: &[Item]| -> usize {
            PAGE_HEADER_BYTES
                + items
                    .iter()
                    .map(|x| 8 + x.key.len() + x.value.len())
                    .sum::<usize>()
        };
        if used_bytes(&items) <= BLOCK_SIZE {
            return Ok(vec![self.write_page(level, items)?]);
        }
        let pivot = items.len() / 2;
        if pivot == 0
            || pivot == items.len()
            || used_bytes(&items[..pivot]) > BLOCK_SIZE
            || used_bytes(&items[pivot..]) > BLOCK_SIZE
        {
            return Err(Error::TooLarge);
        }
        let left = self.write_page(level, items[..pivot].to_vec())?;
        let right = self.write_page(level, items[pivot..].to_vec())?;
        Ok(vec![left, right])
    }

    fn edit_page(&mut self, pointer: BlockPointer, edit: CatalogEdit) -> Result<Vec<Child>> {
        if pointer.birth >= self.generation {
            return Err(Error::CorruptTree);
        }
        let page = read_page(
            self.device,
            pointer,
            TreeId::Snapshots,
            self.volume_id,
            self.generation - 1,
        )?;
        let key = edit.generation().to_be_bytes().to_vec();
        let mut items = page.items;
        if page.level == 0 {
            let slot = items.binary_search_by(|item| item.key.cmp(&key));
            match edit {
                CatalogEdit::Pin(desc) => {
                    if slot.is_ok() {
                        return Err(Error::AlreadyExists);
                    }
                    let at = slot.err().ok_or(Error::CorruptTree)?;
                    items.insert(
                        at,
                        Item {
                            key,
                            value: desc
                                .encode(self.generation, self.device.block_count())?
                                .to_vec(),
                        },
                    );
                }
                CatalogEdit::MarkDropping(_) => {
                    let at = slot.map_err(|_| Error::InvalidSnapshot)?;
                    let mut desc = SnapshotDescriptor::decode(
                        &items[at].value,
                        self.generation,
                        self.device.block_count(),
                    )?;
                    if desc.state != SnapshotState::Pinned {
                        return Err(Error::InvalidSnapshot);
                    }
                    desc.state = SnapshotState::Dropping;
                    items[at].value = desc
                        .encode(self.generation, self.device.block_count())?
                        .to_vec();
                }
            }
            return self.stage_items(0, items);
        }
        if items.is_empty() {
            return Err(Error::CorruptTree);
        }
        let at = match items.binary_search_by(|item| item.key.cmp(&key)) {
            Ok(pos) | Err(pos) if pos < items.len() => pos,
            Ok(_) | Err(_) => items.len() - 1,
        };
        let child = BlockPointer::decode_required(&items[at].value)?;
        let new_children = self.edit_page(child, edit)?;
        items.splice(
            at..=at,
            new_children.into_iter().map(|child| Item {
                key: child.max_key,
                value: child.pointer.encode().to_vec(),
            }),
        );
        self.stage_items(page.level, items)
    }
}

/// Stage an insertion or pending-drop transition by path-copy. The reservation
/// list must contain distinct *descending* physical blocks strictly below
/// the previous published high-water mark and above F0's 0-4 blocks.
/// This is NOT an independently verified allocator; no writes occur here.
pub fn plan_snapshot_edit<D: BlockDevice>(
    device: &mut D,
    old_root: Option<BlockPointer>,
    edit: CatalogEdit,
    volume_id: u128,
    new_generation: u64,
    high_water: u64,
    reserved_virgin_pages: &[u64],
) -> Result<StagedCatalog> {
    if volume_id == 0
        || new_generation <= 1
        || high_water <= ROOT_PAYLOAD_LAST
        || high_water > device.block_count()
        || reserved_virgin_pages.is_empty()
        || reserved_virgin_pages
            .iter()
            .any(|&x| x <= ROOT_PAYLOAD_LAST || x >= high_water)
        || reserved_virgin_pages
            .windows(2)
            .any(|pair| pair[0] <= pair[1])
    {
        return Err(Error::Geometry);
    }
    if edit.generation() == 0 || edit.generation() >= new_generation {
        return Err(Error::InvalidSnapshot);
    }
    if let CatalogEdit::Pin(desc) = edit {
        desc.validate(new_generation - 1, device.block_count())?;
    }
    let mut planner = Planner {
        device,
        volume_id,
        generation: new_generation,
        reservations: reserved_virgin_pages,
        used: 0,
        staged: Vec::new(),
    };
    let mut children = if let Some(root) = old_root {
        root.validate(planner.device.block_count(), new_generation - 1, high_water)?;
        planner.edit_page(root, edit)?
    } else {
        let CatalogEdit::Pin(desc) = edit else {
            return Err(Error::InvalidSnapshot);
        };
        let blocks = planner.device.block_count();
        let value = desc.encode(new_generation, blocks)?.to_vec();
        planner.stage_items(
            0,
            vec![Item {
                key: desc.generation.to_be_bytes().to_vec(),
                value,
            }],
        )?
    };
    let mut level = children[0].pointer.level;
    while children.len() > 1 {
        if level >= MAX_LEVEL {
            return Err(Error::TooLarge);
        }
        let items = children
            .into_iter()
            .map(|child| Item {
                key: child.max_key,
                value: child.pointer.encode().to_vec(),
            })
            .collect();
        children = planner.stage_items(level + 1, items)?;
        level += 1;
    }
    let root = children.pop().ok_or(Error::CorruptTree)?.pointer;
    Ok(StagedCatalog {
        root,
        high_water: reserved_virgin_pages[planner.used - 1],
        consumed_reservations: planner.used,
        staged_pages: planner.staged,
    })
}
