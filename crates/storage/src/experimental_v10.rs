//! F5.0a: **read-only, experimental** v10 wire format and root verification.
//!
//! This does not enable a v10 FileSystem writer or change v9's allocator,
//! reference counts, mount/recovery contract or F0 disk format. In particular,
//! validating a v10 root or catalog is NOT an ownership/free-space certificate.
//! General v10 extents, incremental checksum updates, and reclamation are later
//! milestones; F5.0b adds a restricted one-blob checksum/data experiment. Keep all byte encodings explicit and strictly canonical.
use crate::volume::{ROOT_PAYLOAD_LAST, RootInfo, crc32c};
use crate::{BLOCK_SIZE, BlockDevice, Error, Result, Volume};
use std::collections::BTreeSet;

mod snapshot_tree;
mod virgin_writer;
mod checksum_tree;
mod data_writer;
pub use checksum_tree::{
    ChecksumScan, MAX_CHECKSUM_BLOCKS, MAX_CHECKSUM_PAGES, StagedChecksums,
    read_checksum_tree, read_verified_block, stage_checksum_tree,
};
pub use data_writer::HeaderlessDataWriter;
pub use snapshot_tree::{
    CatalogEdit, CatalogScan, MAX_SNAPSHOT_DESCRIPTORS, MAX_SNAPSHOT_TREE_PAGES, StagedCatalog,
    plan_snapshot_edit, read_snapshot_tree,
};
pub use virgin_writer::VirginSnapshotWriter;

pub const MANIFEST_MAGIC: [u8; 8] = *b"BFSNGM10";
pub const PAGE_MAGIC: [u8; 8] = *b"BFSNGT10";
pub const MANIFEST_VERSION: u32 = 10;
pub const POINTER_BYTES: usize = 24;
pub const SNAPSHOT_BYTES: usize = 112;
pub const PAGE_HEADER_BYTES: usize = 48;
pub const MANIFEST_BYTES: usize = 292;
pub const MAX_LEVEL: u8 = 16;
const MANIFEST_CRC_AT: usize = 288;
const PAGE_CRC_AT: usize = 12;
const ROOT_SLOT_START: usize = 40;
const SLOT_COUNT: usize = 8;

fn bad() -> Error {
    Error::CorruptTree
}
fn get_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().expect("checked fixed offset"))
}
fn get_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("checked fixed offset"))
}
fn get_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("checked fixed offset"))
}
fn get_u128(bytes: &[u8], at: usize) -> u128 {
    u128::from_le_bytes(bytes[at..at + 16].try_into().expect("checked fixed offset"))
}
fn checked_crc(bytes: &[u8], offset: usize) -> u32 {
    let mut copy = bytes.to_vec();
    copy[offset..offset + 4].fill(0);
    crc32c(&copy)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TreeId {
    Fs = 1,
    Checksum = 2,
    Free = 3,
    DeadList = 4,
    AttributeRows = 5,
    AttributePostings = 6,
    Snapshots = 7,
    Schema = 8,
}
impl TreeId {
    fn decode(value: u8) -> Result<Self> {
        Ok(match value {
            1 => Self::Fs,
            2 => Self::Checksum,
            3 => Self::Free,
            4 => Self::DeadList,
            5 => Self::AttributeRows,
            6 => Self::AttributePostings,
            7 => Self::Snapshots,
            8 => Self::Schema,
            _ => return Err(bad()),
        })
    }
}

/// CRC is calculated over the **complete on-disk page**, including its own
/// header CRC. The two CRC fields are therefore intentionally distinct.
/// An all-zero 24-byte slot is the only allowed absent-root representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockPointer {
    pub address: u64,
    pub birth: u64,
    pub crc32c: u32,
    pub level: u8,
}
impl BlockPointer {
    pub fn encode(self) -> [u8; POINTER_BYTES] {
        let mut b = [0; POINTER_BYTES];
        b[0..8].copy_from_slice(&self.address.to_le_bytes());
        b[8..16].copy_from_slice(&self.birth.to_le_bytes());
        b[16..20].copy_from_slice(&self.crc32c.to_le_bytes());
        b[20] = self.level;
        b
    }
    pub fn decode_optional(bytes: &[u8]) -> Result<Option<Self>> {
        if bytes.len() != POINTER_BYTES {
            return Err(bad());
        }
        if bytes.iter().all(|&byte| byte == 0) {
            return Ok(None);
        }
        if bytes[21..24] != [0, 0, 0] {
            return Err(bad());
        }
        let p = Self {
            address: get_u64(bytes, 0),
            birth: get_u64(bytes, 8),
            crc32c: get_u32(bytes, 16),
            level: bytes[20],
        };
        if p.address <= ROOT_PAYLOAD_LAST || p.birth == 0 || p.level > MAX_LEVEL {
            return Err(bad());
        }
        Ok(Some(p))
    }
    pub fn decode_required(bytes: &[u8]) -> Result<Self> {
        Self::decode_optional(bytes)?.ok_or_else(bad)
    }
    pub fn validate(self, total_blocks: u64, generation: u64, low: u64) -> Result<()> {
        if self.address <= ROOT_PAYLOAD_LAST
            || self.address < low
            || self.address >= total_blocks
            || self.birth == 0
            || self.birth > generation
            || self.level > MAX_LEVEL
        {
            return Err(bad());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

/// The same bounded slotted-page codec is used for all eight future tree
/// families. It does not by itself prove parent separator maxima, graph
/// acyclicity, or allocation ownership; those need tree-level validators.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub tree: TreeId,
    pub level: u8,
    pub birth: u64,
    pub address: u64,
    pub volume_id: u128,
    pub items: Vec<Item>,
}
impl Page {
    pub fn encode(&self) -> Result<[u8; BLOCK_SIZE]> {
        if self.volume_id == 0
            || self.address <= ROOT_PAYLOAD_LAST
            || self.birth == 0
            || self.level > MAX_LEVEL
            || (self.items.is_empty() && self.level > 0)
            || self.items.len() > (BLOCK_SIZE - PAGE_HEADER_BYTES) / 8
            || self.items.iter().any(|item| {
                item.key.is_empty()
                    || item.key.len() > u16::MAX as usize
                    || item.value.len() > u16::MAX as usize
            })
            || self.items.windows(2).any(|pair| pair[0].key >= pair[1].key)
        {
            return Err(bad());
        }
        let slots_end = PAGE_HEADER_BYTES + self.items.len() * 8;
        let mut required = slots_end;
        for item in &self.items {
            required = required
                .checked_add(item.key.len())
                .and_then(|n| n.checked_add(item.value.len()))
                .ok_or(Error::TooLarge)?;
            if self.level > 0 {
                let child = BlockPointer::decode_required(&item.value)?;
                if child.level.checked_add(1) != Some(self.level)
                    || child.birth > self.birth
                    || child.address == self.address
                {
                    return Err(bad());
                }
            }
        }
        if required > BLOCK_SIZE {
            return Err(Error::TooLarge);
        }
        let mut b = [0_u8; BLOCK_SIZE];
        b[..8].copy_from_slice(&PAGE_MAGIC);
        b[8] = self.tree as u8;
        b[9] = self.level;
        b[10..12].copy_from_slice(&(self.items.len() as u16).to_le_bytes());
        b[16..24].copy_from_slice(&self.birth.to_le_bytes());
        b[24..32].copy_from_slice(&self.address.to_le_bytes());
        b[32..48].copy_from_slice(&self.volume_id.to_le_bytes());
        let mut end = BLOCK_SIZE;
        for (i, item) in self.items.iter().enumerate() {
            end -= item.value.len();
            let value_at = end;
            b[end..end + item.value.len()].copy_from_slice(&item.value);
            end -= item.key.len();
            let key_at = end;
            b[end..end + item.key.len()].copy_from_slice(&item.key);
            let slot = PAGE_HEADER_BYTES + i * 8;
            b[slot..slot + 2].copy_from_slice(&(key_at as u16).to_le_bytes());
            b[slot + 2..slot + 4].copy_from_slice(&(item.key.len() as u16).to_le_bytes());
            b[slot + 4..slot + 6].copy_from_slice(&(value_at as u16).to_le_bytes());
            b[slot + 6..slot + 8].copy_from_slice(&(item.value.len() as u16).to_le_bytes());
        }
        debug_assert!(end >= slots_end);
        let crc = checked_crc(&b, PAGE_CRC_AT);
        b[PAGE_CRC_AT..PAGE_CRC_AT + 4].copy_from_slice(&crc.to_le_bytes());
        Ok(b)
    }

    pub fn pointer(&self) -> Result<BlockPointer> {
        let b = self.encode()?;
        Ok(BlockPointer {
            address: self.address,
            birth: self.birth,
            crc32c: crc32c(&b),
            level: self.level,
        })
    }

    /// Verify a fetched page against an authenticated parent BP and the
    /// expected volume/tree identity. Reject alternate encodings, not just CRC.
    pub fn decode_verified(
        raw: &[u8; BLOCK_SIZE],
        pointer: BlockPointer,
        tree: TreeId,
        volume_id: u128,
        total_blocks: u64,
        max_generation: u64,
    ) -> Result<Self> {
        pointer.validate(total_blocks, max_generation, ROOT_PAYLOAD_LAST + 1)?;
        if volume_id == 0
            || crc32c(raw) != pointer.crc32c
            || raw[..8] != PAGE_MAGIC
            || get_u32(raw, PAGE_CRC_AT) != checked_crc(raw, PAGE_CRC_AT)
            || TreeId::decode(raw[8])? != tree
            || raw[9] != pointer.level
            || get_u64(raw, 16) != pointer.birth
            || get_u64(raw, 24) != pointer.address
            || get_u128(raw, 32) != volume_id
        {
            return Err(bad());
        }
        let count = get_u16(raw, 10) as usize;
        let slots_end = PAGE_HEADER_BYTES
            .checked_add(count.checked_mul(8).ok_or_else(bad)?)
            .ok_or_else(bad)?;
        if (count == 0 && pointer.level > 0) || slots_end > BLOCK_SIZE {
            return Err(bad());
        }
        let mut items = Vec::with_capacity(count);
        for i in 0..count {
            let slot = PAGE_HEADER_BYTES + i * 8;
            let key_at = get_u16(raw, slot) as usize;
            let key_len = get_u16(raw, slot + 2) as usize;
            let val_at = get_u16(raw, slot + 4) as usize;
            let val_len = get_u16(raw, slot + 6) as usize;
            if key_len == 0
                || key_at < slots_end
                || val_at < slots_end
                || key_at
                    .checked_add(key_len)
                    .is_none_or(|end| end > BLOCK_SIZE)
                || val_at
                    .checked_add(val_len)
                    .is_none_or(|end| end > BLOCK_SIZE)
            {
                return Err(bad());
            }
            let key = raw[key_at..key_at + key_len].to_vec();
            let value = raw[val_at..val_at + val_len].to_vec();
            if pointer.level > 0 {
                let child = BlockPointer::decode_required(&value)?;
                child.validate(total_blocks, pointer.birth, ROOT_PAYLOAD_LAST + 1)?;
                if child.level.checked_add(1) != Some(pointer.level)
                    || child.address == pointer.address
                {
                    return Err(bad());
                }
            }
            items.push(Item { key, value });
        }
        let page = Self {
            tree,
            level: pointer.level,
            birth: pointer.birth,
            address: pointer.address,
            volume_id,
            items,
        };
        // Re-encoding checks sorted keys, disjoint packed offsets, zero
        // reserved bytes and *canonical* slot/payload layout.
        if page.encode().map_err(|_| bad())? != *raw {
            return Err(bad());
        }
        Ok(page)
    }
}

/// Read exactly one metadata page. This verifies the local parent->child
/// boundary but deliberately does NOT traverse enough to certify free space.
pub fn read_page<D: BlockDevice>(
    device: &mut D,
    pointer: BlockPointer,
    tree: TreeId,
    volume_id: u128,
    generation: u64,
) -> Result<Page> {
    pointer.validate(device.block_count(), generation, ROOT_PAYLOAD_LAST + 1)?;
    let mut raw = [0; BLOCK_SIZE];
    device.read_block(pointer.address, &mut raw)?;
    Page::decode_verified(
        &raw,
        pointer,
        tree,
        volume_id,
        device.block_count(),
        generation,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeRoots {
    pub fs: BlockPointer,
    pub checksum: Option<BlockPointer>,
    pub free: Option<BlockPointer>,
    pub dead_list: Option<BlockPointer>,
    pub snapshots: Option<BlockPointer>,
    pub attribute_rows: Option<BlockPointer>,
    pub attribute_postings: Option<BlockPointer>,
    pub schema: Option<BlockPointer>,
}
impl TreeRoots {
    pub fn slots(self) -> [(TreeId, Option<BlockPointer>); SLOT_COUNT] {
        [
            (TreeId::Fs, Some(self.fs)),
            (TreeId::Checksum, self.checksum),
            (TreeId::Free, self.free),
            (TreeId::DeadList, self.dead_list),
            (TreeId::Snapshots, self.snapshots),
            (TreeId::AttributeRows, self.attribute_rows),
            (TreeId::AttributePostings, self.attribute_postings),
            (TreeId::Schema, self.schema),
        ]
    }
    fn from_slots(slots: [Option<BlockPointer>; SLOT_COUNT]) -> Result<Self> {
        Ok(Self {
            fs: slots[0].ok_or_else(bad)?,
            checksum: slots[1],
            free: slots[2],
            dead_list: slots[3],
            snapshots: slots[4],
            attribute_rows: slots[5],
            attribute_postings: slots[6],
            schema: slots[7],
        })
    }
    fn validate(self, total_blocks: u64, generation: u64, low: u64) -> Result<()> {
        let attr_set = [
            self.attribute_rows.is_some(),
            self.attribute_postings.is_some(),
            self.schema.is_some(),
        ];
        if attr_set.iter().any(|&x| x != attr_set[0]) {
            return Err(bad());
        }
        let mut seen = BTreeSet::new();
        for (_, pointer) in self.slots() {
            if let Some(p) = pointer {
                p.validate(total_blocks, generation, low)?;
                if !seen.insert(p.address) {
                    return Err(bad());
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub generation: u64,
    pub high_water: u64,
    pub next_inode: u64,
    pub roots: TreeRoots,
    pub sweep_snapshot: u64,
    pub sweep_key: u64,
    pub metadata_reserve: u64,
    pub inode_count: u64,
    pub used_blocks: u64,
    pub free_blocks: u64,
    pub pending_blocks: u64,
}
impl Manifest {
    pub fn validate(&self, total_blocks: u64) -> Result<()> {
        if self.generation == 0
            || total_blocks <= ROOT_PAYLOAD_LAST
            || self.high_water <= ROOT_PAYLOAD_LAST
            || self.high_water > total_blocks
            || self.next_inode < 2
            || self.metadata_reserve > total_blocks - ROOT_PAYLOAD_LAST - 1
            || self.sweep_snapshot > self.generation
            || [
                self.inode_count,
                self.used_blocks,
                self.free_blocks,
                self.pending_blocks,
            ]
            .iter()
            .any(|&value| value > total_blocks)
        {
            return Err(bad());
        }
        self.roots
            .validate(total_blocks, self.generation, self.high_water)
    }
    pub fn encode(&self, total_blocks: u64) -> Result<[u8; MANIFEST_BYTES]> {
        self.validate(total_blocks)?;
        let mut b = [0_u8; MANIFEST_BYTES];
        b[..8].copy_from_slice(&MANIFEST_MAGIC);
        b[8..12].copy_from_slice(&MANIFEST_VERSION.to_le_bytes());
        b[12..16].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
        b[16..24].copy_from_slice(&self.generation.to_le_bytes());
        b[24..32].copy_from_slice(&self.high_water.to_le_bytes());
        b[32..40].copy_from_slice(&self.next_inode.to_le_bytes());
        for (i, (_, p)) in self.roots.slots().iter().enumerate() {
            if let Some(p) = p {
                let at = ROOT_SLOT_START + i * POINTER_BYTES;
                b[at..at + POINTER_BYTES].copy_from_slice(&p.encode());
            }
        }
        b[232..240].copy_from_slice(&self.sweep_snapshot.to_le_bytes());
        b[240..248].copy_from_slice(&self.sweep_key.to_le_bytes());
        b[248..256].copy_from_slice(&self.metadata_reserve.to_le_bytes());
        for (i, n) in [
            self.inode_count,
            self.used_blocks,
            self.free_blocks,
            self.pending_blocks,
        ]
        .into_iter()
        .enumerate()
        {
            let at = 256 + i * 8;
            b[at..at + 8].copy_from_slice(&n.to_le_bytes());
        }
        let crc = checked_crc(&b, MANIFEST_CRC_AT);
        b[MANIFEST_CRC_AT..MANIFEST_CRC_AT + 4].copy_from_slice(&crc.to_le_bytes());
        Ok(b)
    }
    pub fn decode(bytes: &[u8], root: RootInfo, total_blocks: u64) -> Result<Self> {
        if bytes.len() != MANIFEST_BYTES
            || bytes[..8] != MANIFEST_MAGIC
            || get_u32(bytes, 8) != MANIFEST_VERSION
            || get_u32(bytes, 12) != BLOCK_SIZE as u32
            || get_u32(bytes, MANIFEST_CRC_AT) != checked_crc(bytes, MANIFEST_CRC_AT)
        {
            return Err(bad());
        }
        let mut slots = [None; SLOT_COUNT];
        for (i, slot) in slots.iter_mut().enumerate() {
            let at = ROOT_SLOT_START + i * POINTER_BYTES;
            *slot = BlockPointer::decode_optional(&bytes[at..at + POINTER_BYTES])?;
        }
        let manifest = Self {
            generation: get_u64(bytes, 16),
            high_water: get_u64(bytes, 24),
            next_inode: get_u64(bytes, 32),
            roots: TreeRoots::from_slots(slots)?,
            sweep_snapshot: get_u64(bytes, 232),
            sweep_key: get_u64(bytes, 240),
            metadata_reserve: get_u64(bytes, 248),
            inode_count: get_u64(bytes, 256),
            used_blocks: get_u64(bytes, 264),
            free_blocks: get_u64(bytes, 272),
            pending_blocks: get_u64(bytes, 280),
        };
        if root.volume_id == 0
            || root.root_block.is_none()
            || root.generation != manifest.generation
        {
            return Err(bad());
        }
        manifest.validate(total_blocks)?;
        if manifest.encode(total_blocks)? != bytes {
            return Err(bad());
        }
        Ok(manifest)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum SnapshotState {
    Pinned = 1,
    Dropping = 2,
}
impl SnapshotState {
    fn decode(value: u32) -> Result<Self> {
        match value {
            1 => Ok(Self::Pinned),
            2 => Ok(Self::Dropping),
            _ => Err(bad()),
        }
    }
}

/// Exactly 112 bytes: generation[0..8], four 24-byte BPs[8..104],
/// state[104..108], mandatory zero[108..112]. All attribute roots must
/// be present together or absent together; NEVER inherit today's live roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotDescriptor {
    pub generation: u64,
    pub fs_root: BlockPointer,
    pub attribute_rows_root: Option<BlockPointer>,
    pub attribute_postings_root: Option<BlockPointer>,
    pub attribute_schema_root: Option<BlockPointer>,
    pub state: SnapshotState,
}
impl SnapshotDescriptor {
    pub fn validate(&self, max_generation: u64, total_blocks: u64) -> Result<()> {
        if self.generation == 0 || self.generation > max_generation {
            return Err(bad());
        }
        let roots = [
            self.attribute_rows_root,
            self.attribute_postings_root,
            self.attribute_schema_root,
        ];
        if roots.iter().any(|p| p.is_some() != roots[0].is_some()) {
            return Err(bad());
        }
        self.fs_root
            .validate(total_blocks, self.generation, ROOT_PAYLOAD_LAST + 1)?;
        let mut seen = BTreeSet::from([self.fs_root.address]);
        for p in roots.into_iter().flatten() {
            p.validate(total_blocks, self.generation, ROOT_PAYLOAD_LAST + 1)?;
            if !seen.insert(p.address) {
                return Err(bad());
            }
        }
        Ok(())
    }
    pub fn encode(&self, max_generation: u64, total_blocks: u64) -> Result<[u8; SNAPSHOT_BYTES]> {
        self.validate(max_generation, total_blocks)?;
        let mut b = [0; SNAPSHOT_BYTES];
        b[0..8].copy_from_slice(&self.generation.to_le_bytes());
        b[8..32].copy_from_slice(&self.fs_root.encode());
        for (i, p) in [
            self.attribute_rows_root,
            self.attribute_postings_root,
            self.attribute_schema_root,
        ]
        .into_iter()
        .enumerate()
        {
            if let Some(p) = p {
                let at = 32 + i * POINTER_BYTES;
                b[at..at + POINTER_BYTES].copy_from_slice(&p.encode());
            }
        }
        b[104..108].copy_from_slice(&(self.state as u32).to_le_bytes());
        Ok(b)
    }
    pub fn decode(bytes: &[u8], max_generation: u64, total_blocks: u64) -> Result<Self> {
        if bytes.len() != SNAPSHOT_BYTES || bytes[108..112] != [0; 4] {
            return Err(bad());
        }
        let desc = Self {
            generation: get_u64(bytes, 0),
            fs_root: BlockPointer::decode_required(&bytes[8..32])?,
            attribute_rows_root: BlockPointer::decode_optional(&bytes[32..56])?,
            attribute_postings_root: BlockPointer::decode_optional(&bytes[56..80])?,
            attribute_schema_root: BlockPointer::decode_optional(&bytes[80..104])?,
            state: SnapshotState::decode(get_u32(bytes, 104))?,
        };
        desc.validate(max_generation, total_blocks)?;
        if desc.encode(max_generation, total_blocks)? != bytes {
            return Err(bad());
        }
        Ok(desc)
    }
}

/// Read a bounded, single-leaf snapshot catalog. This is intentionally NOT a
/// full multi-level catalog reader. Supporting branch traversal and a safe
/// catalog updater belongs to the subsequent v10 tree-implementation tranche.
pub fn decode_snapshot_leaf(
    page: &Page,
    current_generation: u64,
    total_blocks: u64,
) -> Result<Vec<SnapshotDescriptor>> {
    if page.tree != TreeId::Snapshots || page.level != 0 {
        return Err(bad());
    }
    let mut descriptors = Vec::with_capacity(page.items.len());
    for item in &page.items {
        if item.key.len() != 8 {
            return Err(bad());
        }
        let desc = SnapshotDescriptor::decode(&item.value, current_generation, total_blocks)?;
        if item.key != desc.generation.to_be_bytes() {
            return Err(bad());
        }
        descriptors.push(desc);
    }
    Ok(descriptors)
}

/// A deliberately **read-only** v10 header/root-page verifier layered over F0.
/// It never exposes the v9 or v10 allocator and cannot authorize physical
/// address reuse. A deep child error discovered later is a read failure.
pub struct ReadOnlyV10<D: BlockDevice> {
    volume: Volume<D>,
    manifest: Manifest,
}
impl<D: BlockDevice> ReadOnlyV10<D> {
    pub fn open(device: D) -> Result<Self> {
        let (volume, manifest) = Volume::open_verified(device, |device, root, payload| {
            let Some(raw) = payload else {
                return Ok(None);
            };
            let Ok(manifest) = Manifest::decode(raw, root, device.block_count()) else {
                return Ok(None);
            };
            // Only verify the immediately referenced root of each global tree.
            // No deep ownership traversal is done or implied here.
            for (tree, pointer) in manifest.roots.slots() {
                if let Some(pointer) = pointer {
                    if read_page(device, pointer, tree, root.volume_id, root.generation).is_err() {
                        return Ok(None);
                    }
                }
            }
            Ok(Some(manifest))
        })?;
        Ok(Self { volume, manifest })
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    pub fn root_info(&self) -> RootInfo {
        self.volume.info()
    }
    pub fn read_root_page(&mut self, tree: TreeId) -> Result<Page> {
        let pointer = self
            .manifest
            .roots
            .slots()
            .iter()
            .find_map(|&(id, p)| (id == tree).then_some(p))
            .flatten()
            .ok_or(Error::NotFound)?;
        let info = self.volume.info();
        self.volume
            .inspect(|device| read_page(device, pointer, tree, info.volume_id, info.generation))
    }
    /// Fully traverse the authenticated multilevel snapshot catalog.
    /// Direct-root open remains lazy; this call does NOT certify reuse.
    pub fn snapshot_descriptors(&mut self) -> Result<Vec<SnapshotDescriptor>> {
        let info = self.volume.info();
        let root = self.manifest.roots.snapshots;
        let descriptors = self.volume.inspect(|device| {
            read_snapshot_tree(device, root, info.volume_id, info.generation)
                .map(|catalog| catalog.descriptors)
        })?;
        // A snapshot's own roots must authenticate to their recorded births,
        // never to the live manifest's attribute roots.
        for desc in &descriptors {
            for (tree, p) in [
                (TreeId::Fs, Some(desc.fs_root)),
                (TreeId::AttributeRows, desc.attribute_rows_root),
                (TreeId::AttributePostings, desc.attribute_postings_root),
                (TreeId::Schema, desc.attribute_schema_root),
            ] {
                if let Some(p) = p {
                    self.volume.inspect(|dev| {
                        read_page(dev, p, tree, info.volume_id, info.generation).map(|_| ())
                    })?;
                }
            }
        }
        Ok(descriptors)
    }
    pub fn into_device(self) -> D {
        self.volume.into_device()
    }
}
