//! Experimental scalable CoW physical-ownership index for v10.
//!
//! An F0 anchor authenticates one B+tree root. Leaves record only non-index,
//! non-owner-tree payload owners; the owner tree's own CoW pages are protected
//! directly by F0 pointers (also from the retained fallback F0). This avoids
//! a recursive fixed-point allocation map.
//!
//! All recovery/use remains subject to an independent full two-F0 audit.
//! These entries are NOT yet authority for freeing physical storage.
use super::space_index::{StagedSpace, stage_space_delta};
use super::{BlockPointer, Item, Manifest, Page, TreeId, MANIFEST_BYTES, read_page};
use crate::volume::{ROOT_PAYLOAD_LAST, RootInfo, crc32c};
use crate::{BlockDevice, Error, Result};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const MAX_OWNER_PAGES: usize = 131_072;
pub(super) const MAX_OWNER_ITEMS: usize = 8_388_608;
// F5.3's fixed Free/Dead arena is *not* an owner-tree node allocator.
pub(super) const MIN_OWNER_ADDRESS: u64 = ROOT_PAYLOAD_LAST + 65;

#[derive(Clone, Debug)]
pub(super) struct OwnerScan {
    pub owners: BTreeSet<u64>,
    pub globals: BTreeSet<u64>,
    pub pages: BTreeSet<u64>,
}
impl OwnerScan {
    pub fn empty() -> Self {
        Self {
            owners: BTreeSet::new(),
            globals: BTreeSet::new(),
            pages: BTreeSet::new(),
        }
    }
}

/// Canonical, versioned 48-byte trailer attached to a v10 F0 manifest.
/// Root metadata is path-copied from normal *certified* Free reservations,
/// never from the fixed 64-block Free/Dead metadata arena.
#[derive(Clone, Copy, Debug)]
pub(super) struct OwnerRootAnchor {
    pub root: BlockPointer,
    pub generation: u64,
}
impl OwnerRootAnchor {
    pub const MAGIC: [u8; 8] = *b"BFSNGOT1";
    pub const BYTES: usize = 48;

    pub fn encode(self) -> [u8; Self::BYTES] {
        let mut bytes = [0_u8; Self::BYTES];
        bytes[0..8].copy_from_slice(&Self::MAGIC);
        bytes[8..32].copy_from_slice(&self.root.encode());
        bytes[32..40].copy_from_slice(&self.generation.to_le_bytes());
        let crc = crc32c(&bytes);
        bytes[40..44].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8], f0: RootInfo, total: u64) -> Result<Self> {
        if bytes.len() != Self::BYTES
            || bytes[..8] != Self::MAGIC
            || bytes[44..48] != [0; 4]
        {
            return Err(Error::CorruptTree);
        }
        let crc = u32::from_le_bytes(
            bytes[40..44].try_into().map_err(|_| Error::CorruptTree)?,
        );
        let mut copy = [0_u8; Self::BYTES];
        copy.copy_from_slice(bytes);
        copy[40..44].fill(0);
        if crc != crc32c(&copy) {
            return Err(Error::CorruptTree);
        }
        let root = BlockPointer::decode_required(&bytes[8..32])?;
        let generation = u64::from_le_bytes(
            bytes[32..40].try_into().map_err(|_| Error::CorruptTree)?,
        );
        if generation != f0.generation || f0.volume_id == 0 {
            return Err(Error::CorruptTree);
        }
        root.validate(total, generation, MIN_OWNER_ADDRESS)?;
        let anchor = Self { root, generation };
        if anchor.encode() != bytes {
            return Err(Error::CorruptTree);
        }
        Ok(anchor)
    }
}

pub(super) fn validate_owner_item(key: &[u8], value: &[u8], total: u64) -> Result<()> {
    if key.len() != 8 || value.len() != 1 || !matches!(value[0], 1 | 2) {
        return Err(Error::CorruptTree);
    }
    let address = u64::from_be_bytes(key.try_into().map_err(|_| Error::CorruptTree)?);
    if address < MIN_OWNER_ADDRESS || address >= total {
        return Err(Error::CorruptTree);
    }
    Ok(())
}

struct Walker<'a, D: BlockDevice> {
    disk: &'a mut D,
    id: u128,
    generation: u64,
    scan: OwnerScan,
}
impl<D: BlockDevice> Walker<'_, D> {
    fn visit(&mut self, pointer: BlockPointer) -> Result<(Vec<u8>, Vec<u8>)> {
        if pointer.address < MIN_OWNER_ADDRESS
            || self.scan.pages.len() >= MAX_OWNER_PAGES
            || !self.scan.pages.insert(pointer.address)
        {
            return Err(Error::CorruptTree);
        }
        let page = read_page(
            self.disk,
            pointer,
            TreeId::Ownership,
            self.id,
            self.generation,
        )?;
        if page.items.is_empty() {
            return Err(Error::CorruptTree);
        }
        let mut first = None;
        let mut last = None;
        if page.level == 0 {
            for item in page.items {
                validate_owner_item(&item.key, &item.value, self.disk.block_count())?;
                let address = u64::from_be_bytes(
                    item.key.as_slice().try_into().map_err(|_| Error::CorruptTree)?,
                );
                if self.scan.owners.len() >= MAX_OWNER_ITEMS
                    || !self.scan.owners.insert(address)
                {
                    return Err(Error::CorruptTree);
                }
                if item.value[0] == 1 {
                    self.scan.globals.insert(address);
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
                if max != item.key || last.as_ref().is_some_and(|last: &Vec<u8>| last >= &min) {
                    return Err(Error::CorruptTree);
                }
                if first.is_none() {
                    first = Some(min);
                }
                last = Some(max);
            }
        }
        Ok((first.ok_or(Error::CorruptTree)?, last.ok_or(Error::CorruptTree)?))
    }
}

pub(super) fn read_owner_tree<D: BlockDevice>(
    disk: &mut D,
    root: BlockPointer,
    id: u128,
    generation: u64,
) -> Result<OwnerScan> {
    if id == 0 || generation == 0 {
        return Err(Error::CorruptTree);
    }
    root.validate(disk.block_count(), generation, MIN_OWNER_ADDRESS)?;
    let mut walker = Walker {
        disk,
        id,
        generation,
        scan: OwnerScan::empty(),
    };
    walker.visit(root)?;
    if !walker.scan.pages.is_disjoint(&walker.scan.owners) {
        return Err(Error::CorruptTree);
    }
    Ok(walker.scan)
}

pub(super) fn changes(
    previous: &OwnerScan,
    new_owners: &BTreeSet<u64>,
    new_globals: &BTreeSet<u64>,
    total: u64,
) -> Result<BTreeMap<Vec<u8>, Option<Vec<u8>>>> {
    if !new_globals.is_subset(new_owners) {
        return Err(Error::CorruptTree);
    }
    if new_owners.len() > MAX_OWNER_ITEMS {
        return Err(Error::TooLarge);
    }
    let mut edits = BTreeMap::new();
    for &address in &previous.owners {
        if !new_owners.contains(&address) {
            edits.insert(address.to_be_bytes().to_vec(), None);
        } else if previous.globals.contains(&address) != new_globals.contains(&address) {
            let value = if new_globals.contains(&address) { 1 } else { 2 };
            edits.insert(address.to_be_bytes().to_vec(), Some(vec![value]));
        }
    }
    for &address in new_owners.difference(&previous.owners) {
        let value = if new_globals.contains(&address) { 1 } else { 2 };
        validate_owner_item(&address.to_be_bytes(), &[value], total)?;
        edits.insert(address.to_be_bytes().to_vec(), Some(vec![value]));
    }
    Ok(edits)
}

/// Path-copy edited leaves and their ancestor paths; unchanged branches
/// retain identical parent pointers and never get rewritten.
#[allow(clippy::too_many_arguments)]
pub(super) fn stage_owner_delta<D: BlockDevice>(
    disk: &mut D,
    prior_root: Option<BlockPointer>,
    edits: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    reservations: &[u64],
    id: u128,
    generation: u64,
    high_water: u64,
) -> Result<StagedSpace> {
    if reservations.iter().any(|&a| a < MIN_OWNER_ADDRESS) {
        return Err(Error::Geometry);
    }
    stage_space_delta(
        disk,
        TreeId::Ownership,
        prior_root,
        edits,
        reservations,
        id,
        generation,
        high_water,
    )
}

/// The owner index omits both Free/Dead arena metadata and its own pages.
/// The latter are F0-root-reachable *anchors*, not self-referential leaves.
pub(super) fn payload_owners(
    verified_owners: &BTreeSet<u64>,
    verified_globals: &BTreeSet<u64>,
) -> (BTreeSet<u64>, BTreeSet<u64>) {
    (
        verified_owners.range(MIN_OWNER_ADDRESS..).copied().collect(),
        verified_globals.range(MIN_OWNER_ADDRESS..).copied().collect(),
    )
}

pub(super) fn parse_anchor(
    raw: &[u8],
    f0: RootInfo,
    total: u64,
) -> Result<Option<OwnerRootAnchor>> {
    if raw.len() <= MANIFEST_BYTES {
        return Ok(None);
    }
    let ext = &raw[MANIFEST_BYTES..];
    if ext.starts_with(&OwnerRootAnchor::MAGIC) {
        Ok(Some(OwnerRootAnchor::decode(ext, f0, total)?))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CrashDevice, BlockDevice};

    const VOLUME: u128 = 0x55aa_1234;

    #[test]
    fn path_copy_owner_tree_grows_beyond_one_page_without_rewriting_all_leaves() {
        let mut disk = CrashDevice::new(16_384);
        let initial: BTreeSet<u64> = (1_000..3_000).collect();
        let global: BTreeSet<u64> = (1_000..3_000).filter(|n| n % 3 == 0).collect();
        let edits = changes(&OwnerScan::empty(), &initial, &global, 16_384).unwrap();
        let free_pages: Vec<u64> = (15_900..16_100).rev().collect();
        let first = stage_owner_delta(
            &mut disk,
            None,
            &edits,
            &free_pages,
            VOLUME,
            5,
            MIN_OWNER_ADDRESS,
        )
        .unwrap();
        assert!(first.consumed_reservations > 1);
        let root = first.root.unwrap();
        for (address, bytes) in first.staged_pages {
            disk.write_block(address, &bytes).unwrap();
        }
        let baseline = read_owner_tree(&mut disk, root, VOLUME, 5).unwrap();
        assert_eq!(baseline.owners, initial);
        assert_eq!(baseline.globals, global);
        let mut changed = initial.clone();
        changed.remove(&1_001);
        changed.insert(3_100);
        let edits = changes(&baseline, &changed, &global, 16_384).unwrap();
        let second = stage_owner_delta(
            &mut disk,
            Some(root),
            &edits,
            &free_pages[first.consumed_reservations..],
            VOLUME,
            6,
            MIN_OWNER_ADDRESS,
        )
        .unwrap();
        assert!(second.consumed_reservations < first.consumed_reservations);
        assert!(!second.staged_pages.is_empty());
        for (address, bytes) in second.staged_pages {
            disk.write_block(address, &bytes).unwrap();
        }
        let after = read_owner_tree(&mut disk, second.root.unwrap(), VOLUME, 6).unwrap();
        assert_eq!(after.owners, changed);
        assert_eq!(after.globals, global);
        assert_eq!(read_owner_tree(&mut disk, root, VOLUME, 5).unwrap().owners, initial);
    }

    #[test]
    fn f0_anchor_binds_root_identity_and_generation() {
        let root = BlockPointer {
            address: 150,
            birth: 5,
            crc32c: 123,
            level: 0,
        };
        let encoded = OwnerRootAnchor { root, generation: 5 }.encode();
        let f0 = RootInfo {
            volume_id: VOLUME,
            generation: 5,
            root_block: Some(3),
        };
        assert_eq!(OwnerRootAnchor::decode(&encoded, f0, 1024).unwrap().root, root);
        assert!(OwnerRootAnchor::decode(
            &encoded,
            RootInfo { generation: 6, ..f0 },
            1024,
        ).is_err());
        let mut corrupt = encoded;
        corrupt[10] ^= 1;
        assert!(OwnerRootAnchor::decode(&corrupt, f0, 1024).is_err());
    }
}
