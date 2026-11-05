//! F5.1 bounded **single-file** sparse extents and partial CoW writes.
//!
//! This is an isolated v10 experiment, not a mounted/general filesystem.
//! Reused physical blocks are forbidden. Every mutation independently audits
//! both recoverable F0 candidates, including retained snapshots and checksum
//! coverage, and reserves only zero-verified virgin space below high-water.
//! Large/multilevel extent trees, multi-inode names, PREALLOC and reflinks are
//! deliberately left to subsequent F5 tranches.
use super::{
    BlockRef, CatalogEdit, FileExtents, Item, Manifest, Page, SnapshotDescriptor, SnapshotState,
    TreeId, VirginSnapshotWriter, MAX_FILE_BYTES, MAX_FILE_BLOCKS, plan_snapshot_edit,
    read_checksum_tree, read_page, read_snapshot_tree, read_verified_block, stage_checksum_tree,
};
use crate::volume::{ROOT_PAYLOAD_LAST, RootInfo, crc32c};
use crate::{BLOCK_SIZE, BlockDevice, Error, Result, Volume};
use std::collections::{BTreeMap, BTreeSet};

const MAX_STAGING_PAGES: usize = 160;

fn fs_file<D: BlockDevice>(
    device: &mut D, pointer: super::BlockPointer, id: u128, generation: u64,
) -> Result<FileExtents> {
    let page = read_page(device, pointer, TreeId::Fs, id, generation)?;
    FileExtents::decode(&page, device.block_count())
}

struct Verified {
    owned: BTreeSet<u64>,
    snapshots: Vec<SnapshotDescriptor>,
    checksums: BTreeMap<u64, u32>,
}

/// One fully reconstructed manifest. Repeated physical data references are
/// accepted only when they denote the *same immutable block birth*, as with
/// a pinned snapshot; metadata/data and distinct tree-domain overlaps fail.
fn verify<D: BlockDevice>(device: &mut D, root: RootInfo, m: Manifest) -> Result<Verified> {
    if m.roots.free.is_some()
        || m.roots.dead_list.is_some()
        || m.roots.attribute_rows.is_some()
        || m.roots.attribute_postings.is_some()
        || m.roots.schema.is_some()
        || m.next_inode != 2
        || m.inode_count != 0
        || m.used_blocks != 0
        || m.free_blocks != 0
        || m.pending_blocks != 0
        || m.sweep_snapshot != 0
        || m.sweep_key != 0
        || m.metadata_reserve != 0
    {
        return Err(Error::CorruptTree);
    }
    let catalog = read_snapshot_tree(
        device, m.roots.snapshots, root.volume_id, root.generation,
    )?;
    let checksums = read_checksum_tree(
        device, m.roots.checksum, root.volume_id, root.generation,
    )?;
    let mut owned = catalog.owned_pages;
    if !owned.is_disjoint(&checksums.metadata_pages) {
        return Err(Error::CorruptTree);
    }
    owned.extend(checksums.metadata_pages);
    let mut roots = BTreeMap::new();
    roots.insert(m.roots.fs.address, m.roots.fs);
    for desc in &catalog.descriptors {
        if desc.attribute_rows_root.is_some()
            || desc.attribute_postings_root.is_some()
            || desc.attribute_schema_root.is_some()
        {
            return Err(Error::CorruptTree);
        }
        if let Some(existing) = roots.insert(desc.fs_root.address, desc.fs_root) {
            if existing != desc.fs_root {
                return Err(Error::CorruptTree);
            }
        }
    }
    let mut referenced = BTreeMap::<u64, u64>::new();
    let mut trailing = BTreeMap::<u64, Vec<(u64, u64)>>::new();
    for pointer in roots.values() {
        if !owned.insert(pointer.address) {
            return Err(Error::CorruptTree);
        }
        let file = fs_file(device, *pointer, root.volume_id, root.generation)?;
        for (&logical, reference) in &file.blocks {
            if let Some(birth) = referenced.insert(reference.physical, reference.birth) {
                if birth != reference.birth {
                    return Err(Error::CorruptTree);
                }
            }
            let logical_end = logical.saturating_add(1) * BLOCK_SIZE as u64;
            if logical_end > file.size {
                trailing.entry(reference.physical).or_default().push((
                    file.size % BLOCK_SIZE as u64,
                    reference.birth,
                ));
            }
        }
    }
    let keys: BTreeSet<u64> = referenced.keys().copied().collect();
    if keys != checksums.checksums.keys().copied().collect() {
        return Err(Error::CorruptTree);
    }
    for physical in keys {
        if !owned.insert(physical) {
            return Err(Error::CorruptTree);
        }
        let bytes = read_verified_block(device, physical, &checksums.checksums)?;
        if let Some(ranges) = trailing.get(&physical) {
            for &(offset, birth) in ranges {
                if referenced.get(&physical) != Some(&birth)
                    || bytes[offset as usize..].iter().any(|&b| b != 0)
                {
                    return Err(Error::CorruptTree);
                }
            }
        }
    }
    if owned.iter().any(|&address| address < m.high_water) {
        return Err(Error::CorruptTree);
    }
    Ok(Verified {
        owned, snapshots: catalog.descriptors, checksums: checksums.checksums,
    })
}

fn audit<D: BlockDevice>(volume: &mut Volume<D>) -> Result<(Manifest, Verified, BTreeSet<u64>)> {
    let latest = volume.info();
    let candidates = volume.candidate_roots()?;
    let mut all = BTreeSet::new();
    let mut selected = None;
    let mut prev: Option<(u64, u64)> = None;
    for (root, payload) in candidates {
        if root.generation == 0 {
            if payload.is_some() {
                return Err(Error::CorruptRoot);
            }
            continue;
        }
        let raw = payload.ok_or(Error::CorruptRoot)?;
        let m = Manifest::decode(&raw, root, volume.block_count())?;
        let verified = volume.inspect(|device| verify(device, root, m))?;
        if let Some((generation, watermark)) = prev {
            if root.generation != generation + 1 || m.high_water > watermark {
                return Err(Error::CorruptTree);
            }
        }
        prev = Some((root.generation, m.high_water));
        all.extend(verified.owned.iter().copied());
        if root.generation == latest.generation {
            selected = Some((m, verified));
        }
    }
    let (m, checked) = selected.ok_or(Error::CorruptRoot)?;
    Ok((m, checked, all))
}

struct Overlay<'a, D: BlockDevice> {
    device: &'a mut D,
    staged: BTreeMap<u64, [u8; BLOCK_SIZE]>,
}
impl<D: BlockDevice> BlockDevice for Overlay<'_, D> {
    fn block_count(&self) -> u64 { self.device.block_count() }
    fn read_block(&mut self, block: u64, out: &mut [u8; BLOCK_SIZE]) -> Result<()> {
        if let Some(contents) = self.staged.get(&block) {
            *out = *contents;
            Ok(())
        } else {
            self.device.read_block(block, out)
        }
    }
    fn write_block(&mut self, _block: u64, _contents: &[u8; BLOCK_SIZE]) -> Result<()> {
        Err(Error::Poisoned)
    }
    fn sync(&mut self) -> Result<()> {
        Err(Error::Poisoned)
    }
}

#[derive(Clone, Copy)]
enum Edit<'a> {
    Write { offset: u64, bytes: &'a [u8] },
    Truncate(u64),
    Pin,
    MarkDropping(u64),
}

/// One bounded sparse file. User-facing names/inodes are not implemented.
pub struct ExtentDataWriter<D: BlockDevice> {
    volume: Volume<D>,
    manifest: Manifest,
}
impl<D: BlockDevice> ExtentDataWriter<D> {
    pub fn format(device: D, volume_id: u128) -> Result<Self> {
        let writer = VirginSnapshotWriter::format(device, volume_id)?;
        Self::open(writer.into_device())
    }
    pub fn open(device: D) -> Result<Self> {
        let mut volume = Volume::open(device)?;
        let (manifest, _, _) = audit(&mut volume)?;
        Ok(Self { volume, manifest })
    }
    pub fn into_device(self) -> D { self.volume.into_device() }
    pub fn manifest(&self) -> &Manifest { &self.manifest }
    pub fn snapshots(&mut self) -> Result<Vec<SnapshotDescriptor>> {
        let (m, checked, _) = audit(&mut self.volume)?;
        if m != self.manifest { return Err(Error::CorruptTree); }
        Ok(checked.snapshots)
    }

    pub fn file_size(&mut self) -> Result<u64> {
        let (m, _, _) = audit(&mut self.volume)?;
        if m != self.manifest { return Err(Error::CorruptTree); }
        let id = self.volume.info().volume_id;
        self.volume.inspect(|device| {
            fs_file(device, m.roots.fs, id, m.generation).map(|f| f.size)
        })
    }

    fn read_range(&mut self, snapshot: Option<u64>, offset: u64, length: usize) -> Result<Vec<u8>> {
        let (m, checked, _) = audit(&mut self.volume)?;
        if m != self.manifest { return Err(Error::CorruptTree); }
        let root = match snapshot {
            None => m.roots.fs,
            Some(generation) => checked.snapshots.iter()
                .find(|desc| desc.generation == generation)
                .ok_or(Error::InvalidSnapshot)?.fs_root,
        };
        let id = self.volume.info().volume_id;
        self.volume.inspect(|device| {
            let file = fs_file(device, root, id, m.generation)?;
            if offset >= file.size || length == 0 { return Ok(Vec::new()); }
            let remaining = file.size - offset;
            let count = remaining.min(length as u64) as usize;
            let mut result = vec![0_u8; count];
            let mut copied = 0;
            while copied < count {
                let position = offset + copied as u64;
                let block = position / BLOCK_SIZE as u64;
                let intra = (position % BLOCK_SIZE as u64) as usize;
                let take = (BLOCK_SIZE - intra).min(count - copied);
                if let Some(reference) = file.blocks.get(&block) {
                    let data = read_verified_block(device, reference.physical, &checked.checksums)?;
                    result[copied..copied + take].copy_from_slice(&data[intra..intra + take]);
                }
                copied += take;
            }
            Ok(result)
        })
    }
    pub fn read_current(&mut self) -> Result<Vec<u8>> {
        self.read_range(None, 0, MAX_FILE_BYTES as usize)
    }
    pub fn read_at(&mut self, offset: u64, length: usize) -> Result<Vec<u8>> {
        self.read_range(None, offset, length)
    }
    pub fn read_snapshot_at(&mut self, generation: u64, offset: u64, length: usize) -> Result<Vec<u8>> {
        self.read_range(Some(generation), offset, length)
    }
    pub fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<u64> {
        let end = offset.checked_add(bytes.len() as u64).ok_or(Error::TooLarge)?;
        if end > MAX_FILE_BYTES { return Err(Error::TooLarge); }
        if bytes.is_empty() { return Ok(self.manifest.generation); }
        self.commit(Edit::Write { offset, bytes })
    }
    pub fn truncate(&mut self, size: u64) -> Result<u64> {
        if size > MAX_FILE_BYTES { return Err(Error::TooLarge); }
        self.commit(Edit::Truncate(size))
    }
    pub fn pin_current(&mut self) -> Result<u64> { self.commit(Edit::Pin) }
    pub fn mark_dropping(&mut self, generation: u64) -> Result<u64> {
        self.commit(Edit::MarkDropping(generation))
    }

    fn commit(&mut self, edit: Edit<'_>) -> Result<u64> {
        let (old, checked, owned) = audit(&mut self.volume)?;
        if old != self.manifest { return Err(Error::CorruptTree); }
        let generation = old.generation.checked_add(1).ok_or(Error::GenerationOverflow)?;
        let id = self.volume.info().volume_id;
        let total = self.volume.block_count();
        let mut pins = checked.snapshots.clone();
        let mut catalog_edit = None;
        let mut new_file = self.volume.inspect(|device| {
            fs_file(device, old.roots.fs, id, old.generation)
        })?;
        let mut dirty = BTreeMap::<u64, [u8; BLOCK_SIZE]>::new();
        match edit {
            Edit::Pin => {
                if pins.iter().any(|d| d.generation == old.generation) {
                    return Err(Error::AlreadyExists);
                }
                let desc = SnapshotDescriptor {
                    generation: old.generation, fs_root: old.roots.fs,
                    attribute_rows_root: None, attribute_postings_root: None,
                    attribute_schema_root: None, state: SnapshotState::Pinned,
                };
                pins.push(desc);
                pins.sort_by_key(|d| d.generation);
                catalog_edit = Some(CatalogEdit::Pin(desc));
            }
            Edit::MarkDropping(generation) => {
                let desc = pins.iter_mut().find(|d| d.generation == generation)
                    .ok_or(Error::InvalidSnapshot)?;
                if desc.state != SnapshotState::Pinned {
                    return Err(Error::InvalidSnapshot);
                }
                desc.state = SnapshotState::Dropping;
                catalog_edit = Some(CatalogEdit::MarkDropping(generation));
            }
            Edit::Write { offset, bytes } => {
                let end = offset.checked_add(bytes.len() as u64).ok_or(Error::TooLarge)?;
                new_file.size = new_file.size.max(end);
                let mut copied = 0;
                while copied < bytes.len() {
                    let position = offset + copied as u64;
                    let logical = position / BLOCK_SIZE as u64;
                    let intra = (position % BLOCK_SIZE as u64) as usize;
                    let count = (BLOCK_SIZE - intra).min(bytes.len() - copied);
                    if !dirty.contains_key(&logical) {
                        let data = if let Some(reference) = new_file.blocks.get(&logical) {
                            self.volume.inspect(|device| {
                                read_verified_block(device, reference.physical, &checked.checksums)
                            })?
                        } else {
                            [0_u8; BLOCK_SIZE]
                        };
                        dirty.insert(logical, data);
                    }
                    let block = dirty.get_mut(&logical).ok_or(Error::CorruptTree)?;
                    block[intra..intra + count].copy_from_slice(&bytes[copied..copied + count]);
                    copied += count;
                }
            }
            Edit::Truncate(size) => {
                if size == new_file.size { return Ok(old.generation); }
                new_file.size = size;
                let blocks = size.div_ceil(BLOCK_SIZE as u64);
                new_file.blocks.retain(|&logical, _| logical < blocks);
                if size % BLOCK_SIZE as u64 != 0 {
                    let last = blocks - 1;
                    if let Some(reference) = new_file.blocks.get(&last) {
                        let mut data = self.volume.inspect(|device| {
                            read_verified_block(device, reference.physical, &checked.checksums)
                        })?;
                        let tail = (size % BLOCK_SIZE as u64) as usize;
                        if data[tail..].iter().any(|&b| b != 0) {
                            data[tail..].fill(0);
                            dirty.insert(last, data);
                        }
                    }
                }
            }
        }
        let needs_file_update = matches!(edit, Edit::Write { .. } | Edit::Truncate(_));
        let mut candidates = Vec::new();
        let mut cursor = old.high_water;
        while cursor > ROOT_PAYLOAD_LAST + 1 && candidates.len() < MAX_STAGING_PAGES {
            cursor -= 1;
            if owned.contains(&cursor) { return Err(Error::CorruptTree); }
            let mut bytes = [0_u8; BLOCK_SIZE];
            self.volume.inspect(|device| device.read_block(cursor, &mut bytes))?;
            if bytes.iter().all(|&byte| byte == 0) {
                candidates.push(cursor);
            }
        }
        let mut used = 0;
        let mut staged: Vec<(u64, [u8; BLOCK_SIZE])> = Vec::new();
        let mut added_checksums = BTreeMap::new();
        let mut next = old;
        next.generation = generation;
        if needs_file_update {
            // Reverse logical order makes consecutively allocated descending
            // virgin physical blocks map to ascending logical extents.
            for (logical, bytes) in dirty.iter().rev() {
                let physical = *candidates.get(used).ok_or(Error::NoSpace)?;
                used += 1;
                new_file.blocks.insert(*logical, BlockRef { physical, birth: generation });
                staged.push((physical, *bytes));
                added_checksums.insert(physical, crc32c(bytes));
            }
            let address = *candidates.get(used).ok_or(Error::NoSpace)?;
            used += 1;
            let fs = Page {
                tree: TreeId::Fs, level: 0, birth: generation,
                address, volume_id: id,
                items: new_file.encode_items(generation, total)?,
            };
            staged.push((address, fs.encode()?));
            next.roots.fs = fs.pointer()?;
            // The new manifest's checksum tree describes exactly its live
            // and pinned data, never the old fallback's separate lifetime.
            let mut required = BTreeMap::new();
            self.volume.inspect(|device| {
                for desc in &pins {
                    let file = fs_file(device, desc.fs_root, id, old.generation)?;
                    for reference in file.blocks.values() {
                        let checksum = *checked.checksums.get(&reference.physical)
                            .ok_or(Error::CorruptTree)?;
                        required.insert(reference.physical, checksum);
                    }
                }
                Ok(())
            })?;
            for reference in new_file.blocks.values() {
                let checksum = added_checksums.get(&reference.physical)
                    .or_else(|| checked.checksums.get(&reference.physical))
                    .ok_or(Error::CorruptTree)?;
                required.insert(reference.physical, *checksum);
            }
            let cs = stage_checksum_tree(
                &required, &candidates[used..], id, generation, old.high_water, total,
            )?;
            used += cs.consumed_reservations;
            next.roots.checksum = cs.root;
            staged.extend(cs.staged_pages);
        }
        if let Some(change) = catalog_edit {
            let plan = self.volume.inspect(|device| {
                plan_snapshot_edit(
                    device, old.roots.snapshots, change, id, generation,
                    old.high_water, &candidates[used..],
                )
            })?;
            used += plan.consumed_reservations;
            next.roots.snapshots = Some(plan.root);
            staged.extend(plan.staged_pages);
        }
        if used == 0 { return Err(Error::NoSpace); }
        next.high_water = candidates[used - 1];
        let payload = next.encode(total)?;
        self.volume.inspect(|device| {
            let mut overlay = Overlay {
                device, staged: staged.iter().copied().collect(),
            };
            let root = RootInfo {
                volume_id: id, generation, root_block: Some(2 + (generation - 1) % 3),
            };
            let validated = verify(&mut overlay, root, next)?;
            if validated.snapshots != pins {
                return Err(Error::CorruptTree);
            }
            // No metadata or data block from any recoverable root may be
            // touched. This checks all staged pages including data payloads.
            let written: BTreeSet<u64> = staged.iter().map(|(n, _)| *n).collect();
            if written.len() != staged.len() || !written.is_disjoint(&owned) {
                return Err(Error::CorruptTree);
            }
            Ok(())
        })?;
        self.volume.commit_root_with_blocks(&staged, &payload)?;
        self.manifest = next;
        Ok(generation)
    }
}
