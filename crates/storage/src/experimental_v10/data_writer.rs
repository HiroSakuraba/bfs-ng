//! F5.0b experiment: one logical 4 KiB blob, immutable physical data blocks,
//! global checksum B+tree, and snapshot-consistent historical reads.
//! This is intentionally NOT the F5.1 extent format or a general v10 writer.
//! Every mutation audits both recoverable F0 graphs and consumes only virgin
//! zero pages; no block reuse, final drop, or nonempty namespace is supported.
use super::{
    BlockPointer, CatalogEdit, Item, Manifest, Page, SnapshotDescriptor, SnapshotState,
    TreeId, VirginSnapshotWriter, plan_snapshot_edit, read_page, read_snapshot_tree,
    read_checksum_tree, read_verified_block, stage_checksum_tree,
};
use crate::volume::{ROOT_PAYLOAD_LAST, RootInfo, crc32c};
use crate::{BLOCK_SIZE, BlockDevice, Error, Result, Volume};
use std::collections::{BTreeMap, BTreeSet};

const MAX_STAGE_PAGES: usize = 64;
const BLOB_KEY: &[u8] = b"blob";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DataRef {
    physical: u64,
    birth: u64,
    length: u16,
}
impl DataRef {
    fn encode(self) -> Vec<u8> {
        let mut b = vec![0; 20];
        b[..8].copy_from_slice(&self.physical.to_le_bytes());
        b[8..16].copy_from_slice(&self.birth.to_le_bytes());
        b[16..18].copy_from_slice(&self.length.to_le_bytes());
        b
    }
    fn decode(bytes: &[u8], generation: u64, blocks: u64) -> Result<Self> {
        if bytes.len() != 20 || bytes[18..] != [0, 0] {
            return Err(Error::CorruptTree);
        }
        let r = Self {
            physical: u64::from_le_bytes(bytes[..8].try_into().map_err(|_| Error::CorruptTree)?),
            birth: u64::from_le_bytes(bytes[8..16].try_into().map_err(|_| Error::CorruptTree)?),
            length: u16::from_le_bytes(bytes[16..18].try_into().map_err(|_| Error::CorruptTree)?),
        };
        if r.physical <= ROOT_PAYLOAD_LAST || r.physical >= blocks
            || r.birth == 0 || r.birth > generation
            || r.length == 0 || usize::from(r.length) > BLOCK_SIZE
        {
            return Err(Error::CorruptTree);
        }
        Ok(r)
    }
}
fn fs_blob<D: BlockDevice>(
    device: &mut D,
    root: BlockPointer,
    volume_id: u128,
    generation: u64,
) -> Result<Option<DataRef>> {
    let page = read_page(device, root, TreeId::Fs, volume_id, generation)?;
    if page.level != 0 || page.items.len() > 1 {
        return Err(Error::CorruptTree);
    }
    let Some(item) = page.items.first() else { return Ok(None); };
    if item.key != BLOB_KEY {
        return Err(Error::CorruptTree);
    }
    let r = DataRef::decode(&item.value, root.birth, device.block_count())?;
    Ok(Some(r))
}
struct Verified {
    owned: BTreeSet<u64>,
    snapshots: Vec<SnapshotDescriptor>,
    checksums: BTreeMap<u64, u32>,
}
/// Independently reconstruct the *exact* data+checksum ownership union
/// for one F0 manifest. Retained snapshots include Dropping descriptors.
fn verify<D: BlockDevice>(
    device: &mut D,
    root: RootInfo,
    manifest: Manifest,
) -> Result<Verified> {
    if manifest.roots.free.is_some()
        || manifest.roots.dead_list.is_some()
        || manifest.roots.attribute_rows.is_some()
        || manifest.roots.attribute_postings.is_some()
        || manifest.roots.schema.is_some()
        || manifest.next_inode != 2 || manifest.inode_count != 0
        || manifest.used_blocks != 0 || manifest.free_blocks != 0
        || manifest.pending_blocks != 0 || manifest.sweep_key != 0
        || manifest.sweep_snapshot != 0 || manifest.metadata_reserve != 0
    {
        return Err(Error::CorruptTree);
    }
    let catalog = read_snapshot_tree(
        device, manifest.roots.snapshots, root.volume_id, root.generation,
    )?;
    let csum = read_checksum_tree(
        device, manifest.roots.checksum, root.volume_id, root.generation,
    )?;
    let mut owned = catalog.owned_pages;
    if !owned.is_disjoint(&csum.metadata_pages) { return Err(Error::CorruptTree); }
    owned.extend(csum.metadata_pages);
    let mut fs_roots = BTreeSet::from([(
        manifest.roots.fs.address,
        manifest.roots.fs.birth,
        manifest.roots.fs.crc32c,
        manifest.roots.fs.level,
    )]);
    for desc in &catalog.descriptors {
        if desc.attribute_rows_root.is_some()
            || desc.attribute_postings_root.is_some()
            || desc.attribute_schema_root.is_some()
        {
            return Err(Error::CorruptTree);
        }
        fs_roots.insert((
            desc.fs_root.address, desc.fs_root.birth, desc.fs_root.crc32c, desc.fs_root.level,
        ));
    }
    let mut data: BTreeMap<u64, u64> = BTreeMap::new();
    for (address, birth, crc, level) in fs_roots {
        let ptr = BlockPointer { address, birth, crc32c: crc, level };
        let entry = fs_blob(device, ptr, root.volume_id, root.generation)?;
        if !owned.insert(address) { return Err(Error::CorruptTree); }
        if let Some(d) = entry {
            if let Some(previous) = data.insert(d.physical, d.birth) {
                if previous != d.birth { return Err(Error::CorruptTree); }
            }
        }
    }
    // The selected checksum tree covers exactly the live+snapshot data
    // union. The older F0 root uses *its own* historical checksum tree.
    if data.keys().copied().collect::<BTreeSet<_>>()
        != csum.checksums.keys().copied().collect::<BTreeSet<_>>()
    {
        return Err(Error::CorruptTree);
    }
    for physical in data.keys().copied() {
        if !owned.insert(physical) { return Err(Error::CorruptTree); }
        read_verified_block(device, physical, &csum.checksums)?;
    }
    if owned.iter().any(|&block| block < manifest.high_water) {
        return Err(Error::CorruptTree);
    }
    Ok(Verified {
        owned, snapshots: catalog.descriptors, checksums: csum.checksums,
    })
}
/// Conservative two-candidate audit; any invalid fallback blocks mutation.
/// A lazily readable latest root is not a physical reuse certificate.
fn audit<D: BlockDevice>(volume: &mut Volume<D>) -> Result<(Manifest, Verified, BTreeSet<u64>)> {
    let latest = volume.info();
    let candidates = volume.candidate_roots()?;
    let mut current = None;
    let mut owned_all = BTreeSet::new();
    let mut prev: Option<(u64, u64)> = None;
    for (root, payload) in candidates {
        if root.generation == 0 {
            if payload.is_some() { return Err(Error::CorruptRoot); }
            continue;
        }
        let bytes = payload.ok_or(Error::CorruptRoot)?;
        let m = Manifest::decode(&bytes, root, volume.block_count())?;
        let verified = volume.inspect(|device| verify(device, root, m))?;
        if let Some((last_g, last_water)) = prev {
            if root.generation != last_g + 1 || m.high_water > last_water {
                return Err(Error::CorruptTree);
            }
        }
        prev = Some((root.generation, m.high_water));
        owned_all.extend(verified.owned.iter().copied());
        if root.generation == latest.generation {
            current = Some((m, verified));
        }
    }
    let (m, checked) = current.ok_or(Error::CorruptRoot)?;
    Ok((m, checked, owned_all))
}
/// An overlay validates the entire candidate graph *before any write*.
struct Overlay<'a, D: BlockDevice> {
    device: &'a mut D,
    staged: BTreeMap<u64, [u8; BLOCK_SIZE]>,
}
impl<D: BlockDevice> BlockDevice for Overlay<'_, D> {
    fn block_count(&self) -> u64 { self.device.block_count() }
    fn read_block(&mut self, block: u64, out: &mut [u8; BLOCK_SIZE]) -> Result<()> {
        if let Some(bytes) = self.staged.get(&block) {
            *out = *bytes;
            Ok(())
        } else { self.device.read_block(block, out) }
    }
    fn write_block(&mut self, _block: u64, _bytes: &[u8; BLOCK_SIZE]) -> Result<()> {
        Err(Error::Poisoned)
    }
    fn sync(&mut self) -> Result<()> { Err(Error::Poisoned) }
}
enum Edit<'a> {
    Write(&'a [u8]),
    Pin,
    MarkDropping(u64),
}
/// A **one-blob** experimental writer. It demonstrates F5.0b's checksummed
/// data and historical retention, not F5.1's multi-extent file storage.
pub struct HeaderlessDataWriter<D: BlockDevice> {
    volume: Volume<D>,
    manifest: Manifest,
}
impl<D: BlockDevice> HeaderlessDataWriter<D> {
    pub fn format(device: D, volume_id: u128) -> Result<Self> {
        let writer = VirginSnapshotWriter::format(device, volume_id)?;
        Self::open(writer.into_device())
    }
    pub fn open(device: D) -> Result<Self> {
        let mut volume = Volume::open(device)?;
        let (manifest, _, _) = audit(&mut volume)?;
        Ok(Self { volume, manifest })
    }
    pub fn manifest(&self) -> &Manifest { &self.manifest }
    pub fn into_device(self) -> D { self.volume.into_device() }
    pub fn snapshots(&mut self) -> Result<Vec<SnapshotDescriptor>> {
        let (m, verified, _) = audit(&mut self.volume)?;
        if m != self.manifest { return Err(Error::CorruptTree); }
        Ok(verified.snapshots)
    }
    fn read_blob(&mut self, generation: Option<u64>) -> Result<Vec<u8>> {
        let (m, v, _) = audit(&mut self.volume)?;
        if m != self.manifest { return Err(Error::CorruptTree); }
        let root = match generation {
            None => m.roots.fs,
            Some(g) => v.snapshots.iter().find(|d| d.generation == g)
                .ok_or(Error::InvalidSnapshot)?.fs_root,
        };
        let id = self.volume.info().volume_id;
        self.volume.inspect(|device| {
            let Some(entry) = fs_blob(device, root, id, m.generation)? else {
                return Ok(Vec::new());
            };
            let bytes = read_verified_block(device, entry.physical, &v.checksums)?;
            if bytes[usize::from(entry.length)..].iter().any(|&byte| byte != 0) {
                return Err(Error::CorruptTree);
            }
            Ok(bytes[..usize::from(entry.length)].to_vec())
        })
    }
    pub fn read_current(&mut self) -> Result<Vec<u8>> { self.read_blob(None) }
    pub fn read_snapshot(&mut self, generation: u64) -> Result<Vec<u8>> {
        self.read_blob(Some(generation))
    }
    pub fn write_current(&mut self, data: &[u8]) -> Result<u64> {
        if data.len() > BLOCK_SIZE { return Err(Error::TooLarge); }
        self.commit(Edit::Write(data))
    }
    pub fn pin_current(&mut self) -> Result<u64> { self.commit(Edit::Pin) }
    pub fn mark_dropping(&mut self, generation: u64) -> Result<u64> {
        self.commit(Edit::MarkDropping(generation))
    }
    fn commit(&mut self, edit: Edit<'_>) -> Result<u64> {
        let (old, checked, owned) = audit(&mut self.volume)?;
        if old != self.manifest { return Err(Error::CorruptTree); }
        let next_gen = old.generation.checked_add(1).ok_or(Error::GenerationOverflow)?;
        let id = self.volume.info().volume_id;
        let blocks = self.volume.block_count();
        let mut expected_pins = checked.snapshots.clone();
        let mut pins_edit: Option<CatalogEdit> = None;
        match edit {
            Edit::Pin => {
                if expected_pins.iter().any(|d| d.generation == old.generation) {
                    return Err(Error::AlreadyExists);
                }
                let desc = SnapshotDescriptor {
                    generation: old.generation, fs_root: old.roots.fs,
                    attribute_rows_root: None, attribute_postings_root: None,
                    attribute_schema_root: None, state: SnapshotState::Pinned,
                };
                expected_pins.push(desc);
                expected_pins.sort_by_key(|d| d.generation);
                pins_edit = Some(CatalogEdit::Pin(desc));
            }
            Edit::MarkDropping(g) => {
                let d = expected_pins.iter_mut().find(|d| d.generation == g)
                    .ok_or(Error::InvalidSnapshot)?;
                if d.state != SnapshotState::Pinned { return Err(Error::InvalidSnapshot); }
                d.state = SnapshotState::Dropping;
                pins_edit = Some(CatalogEdit::MarkDropping(g));
            }
            Edit::Write(_) => {}
        }
        let mut candidates = Vec::new();
        let mut cursor = old.high_water;
        while cursor > ROOT_PAYLOAD_LAST + 1 && candidates.len() < MAX_STAGE_PAGES {
            cursor -= 1;
            if owned.contains(&cursor) { return Err(Error::CorruptTree); }
            let mut bytes = [0; BLOCK_SIZE];
            self.volume.inspect(|device| device.read_block(cursor, &mut bytes))?;
            if bytes.iter().all(|&b| b == 0) { candidates.push(cursor); }
        }
        let mut used = 0;
        let mut staged: Vec<(u64, [u8; BLOCK_SIZE])> = Vec::new();
        let mut next = old;
        next.generation = next_gen;
        if let Edit::Write(data) = edit {
            if candidates.len() < 1 { return Err(Error::NoSpace); }
            let mut items = Vec::new();
            let mut new_data = None;
            if !data.is_empty() {
                let phys = candidates[used];
                used += 1;
                let mut block = [0_u8; BLOCK_SIZE];
                block[..data.len()].copy_from_slice(data);
                new_data = Some((phys, block));
                items.push(Item {
                    key: BLOB_KEY.to_vec(),
                    value: DataRef { physical: phys, birth: next_gen, length: data.len() as u16 }.encode(),
                });
            }
            let address = *candidates.get(used).ok_or(Error::NoSpace)?;
            used += 1;
            let fs = Page {
                tree: TreeId::Fs, level: 0, birth: next_gen,
                address, volume_id: id, items,
            };
            next.roots.fs = fs.pointer()?;
            if let Some(page) = new_data { staged.push(page); }
            staged.push((address, fs.encode()?));
            // Keep exactly the snapshots' required checksums plus new live
            // data. An unpinned overwritten block is protected *only* by
            // the earlier F0 manifest's own checksum-tree root.
            let mut required = BTreeMap::new();
            self.volume.inspect(|device| {
                for desc in &expected_pins {
                    if let Some(r) = fs_blob(device, desc.fs_root, id, old.generation)? {
                        required.insert(r.physical, *checked.checksums.get(&r.physical).ok_or(Error::CorruptTree)?);
                    }
                }
                Ok(())
            })?;
            if let Some((phys, bytes)) = new_data {
                required.insert(phys, crc32c(&bytes));
            }
            let cs = stage_checksum_tree(
                &required, &candidates[used..], id, next_gen, old.high_water, blocks,
            )?;
            used += cs.consumed_reservations;
            next.roots.checksum = cs.root;
            staged.extend(cs.staged_pages);
        }
        if let Some(change) = pins_edit {
            let catalog = self.volume.inspect(|device| {
                plan_snapshot_edit(
                    device, old.roots.snapshots, change, id, next_gen, old.high_water,
                    &candidates[used..],
                )
            })?;
            used += catalog.consumed_reservations;
            next.roots.snapshots = Some(catalog.root);
            staged.extend(catalog.staged_pages);
        }
        if used == 0 { return Err(Error::CorruptTree); }
        next.high_water = candidates[used - 1];
        let payload = next.encode(blocks)?;
        self.volume.inspect(|device| {
            let mut overlay = Overlay {
                device, staged: staged.iter().copied().collect(),
            };
            let root = RootInfo { volume_id: id, generation: next_gen, root_block: Some(2 + (next_gen - 1) % 3) };
            let verified = verify(&mut overlay, root, next)?;
            if verified.snapshots != expected_pins {
                return Err(Error::CorruptTree);
            }
            // Prove the newly staged graph contains no reference to an
            // address already held by either old or fallback candidate.
            let reused: BTreeSet<u64> = staged.iter().map(|(addr, _)| *addr).collect();
            if !reused.is_disjoint(&owned) || reused.len() != staged.len() {
                return Err(Error::CorruptTree);
            }
            Ok(())
        })?;
        self.volume.commit_root_with_blocks(&staged, &payload)?;
        self.manifest = next;
        Ok(next_gen)
    }
}
