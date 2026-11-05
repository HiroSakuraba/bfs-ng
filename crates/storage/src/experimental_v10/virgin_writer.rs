//! Conservative, experimental **snapshot-metadata-only** v10 writer.
//!
//! Every mutation first verifies both recoverable F0 candidates and the
//! entire supported v10 graph. Only previously-zero, descending virgin pages
//! may be written; a nonzero crash orphan is skipped forever. This is not a
//! free-tree allocator, a general v10 filesystem, or support for arbitrary
//! inode/file-data/checksum/attribute trees. Production allocation is absent.
use super::{
    BlockPointer, CatalogEdit, Manifest, Page, SnapshotDescriptor, SnapshotState,
    TreeId, TreeRoots, read_page, read_snapshot_tree, plan_snapshot_edit,
    MAX_LEVEL,
};
use crate::volume::{ROOT_PAYLOAD_LAST, RootInfo};
use crate::{BLOCK_SIZE, BlockDevice, Error, Result, Volume};
use std::collections::{BTreeMap, BTreeSet};

/// Bounded metadata reservations for one path-copy operation (splits and root
/// growth). Actual page count can be lower; unused candidates remain untouched.
const MAX_STAGING_PAGES: usize = 2 * (MAX_LEVEL as usize + 1) + 1;

/// Reject all unsupported roots, data, or catalog shapes. We do not yet have
/// an F5 data/attribute/checksum tree allocator. An empty Fs leaf is the only
/// admissible namespace state in the test-only writer profile.
fn check_profile<D: BlockDevice>(
    device: &mut D,
    root: RootInfo,
    manifest: &Manifest,
) -> Result<BTreeSet<u64>> {
    if manifest.roots.checksum.is_some()
        || manifest.roots.free.is_some()
        || manifest.roots.dead_list.is_some()
        || manifest.roots.attribute_rows.is_some()
        || manifest.roots.attribute_postings.is_some()
        || manifest.roots.schema.is_some()
        || manifest.sweep_snapshot != 0
        || manifest.sweep_key != 0
        || manifest.metadata_reserve != 0
        || manifest.next_inode != 2
        || manifest.inode_count != 0
        || manifest.used_blocks != 0
        || manifest.free_blocks != 0
        || manifest.pending_blocks != 0
    {
        return Err(Error::CorruptTree);
    }
    let fs = read_page(
        device, manifest.roots.fs, TreeId::Fs,
        root.volume_id, root.generation,
    )?;
    if fs.level != 0 || !fs.items.is_empty() {
        return Err(Error::CorruptTree);
    }
    let mut owned = BTreeSet::from([manifest.roots.fs.address]);
    let catalog = read_snapshot_tree(
        device, manifest.roots.snapshots, root.volume_id, root.generation,
    )?;
    owned.extend(catalog.owned_pages);
    // Snapshot descriptors are immutable references, even if Dropping.
    // No descriptor may refer to a data/attribute tree in this restricted
    // writer profile. This check prevents unverified ownership.
    for desc in catalog.descriptors {
        if desc.fs_root != manifest.roots.fs
            || desc.attribute_rows_root.is_some()
            || desc.attribute_postings_root.is_some()
            || desc.attribute_schema_root.is_some()
        {
            return Err(Error::CorruptTree);
        }
    }
    Ok(owned)
}

/// The strict audit is re-run BEFORE EVERY MUTATION. An invalid but still
/// checksummed fallback root is **not** silently ignored. This is separate
/// from the read-only, lazy v10 open semantics.
fn audit<D: BlockDevice>(volume: &mut Volume<D>) -> Result<(Manifest, BTreeSet<u64>)> {
    let candidates = volume.candidate_roots()?;
    let latest = volume.info();
    if candidates.is_empty() {
        return Err(Error::CorruptRoot);
    }
    let mut all_owned = BTreeSet::new();
    let mut current = None;
    let mut previous: Option<(u64, u64)> = None;
    for (root, payload) in candidates {
        if root.generation == 0 {
            if payload.is_some() {
                return Err(Error::CorruptRoot);
            }
            continue;
        }
        let bytes = payload.ok_or(Error::CorruptRoot)?;
        let manifest = Manifest::decode(&bytes, root, volume.block_count())?;
        let owned = volume.inspect(|device| check_profile(device, root, &manifest))?;
        if owned.iter().any(|&block| block < manifest.high_water) {
            return Err(Error::CorruptTree);
        }
        all_owned.extend(owned);
        if let Some((older_generation, older_watermark)) = previous {
            if root.generation != older_generation + 1
                || manifest.high_water > older_watermark
            {
                return Err(Error::CorruptTree);
            }
        }
        previous = Some((root.generation, manifest.high_water));
        if root.generation == latest.generation {
            current = Some(manifest);
        }
    }
    let current = current.ok_or(Error::CorruptRoot)?;
    // Check both physical F0 candidate graphs, *including* any more recent
    // candidate that a read-only mount could have elected to ignore.
    Ok((current, all_owned))
}

/// In-memory overlay used to verify the *entire candidate snapshot catalog*
/// before the staged pages are written. This is a read-only verifier; no
/// unfinished staged metadata can be published on validation failure.
struct Overlay<'a, D: BlockDevice> {
    device: &'a mut D,
    staged: BTreeMap<u64, [u8; BLOCK_SIZE]>,
}
impl<D: BlockDevice> BlockDevice for Overlay<'_, D> {
    fn block_count(&self) -> u64 {
        self.device.block_count()
    }
    fn read_block(&mut self, block: u64, out: &mut [u8; BLOCK_SIZE]) -> Result<()> {
        if let Some(bytes) = self.staged.get(&block) {
            *out = *bytes;
            Ok(())
        } else {
            self.device.read_block(block, out)
        }
    }
    fn write_block(&mut self, _block: u64, _bytes: &[u8; BLOCK_SIZE]) -> Result<()> {
        Err(Error::Poisoned)
    }
    fn sync(&mut self) -> Result<()> {
        Err(Error::Poisoned)
    }
}

/// Experimental on-disk root publication for a **static, empty fs root**
/// with an incrementally updated snapshot catalog. All other v10 trees are
/// deliberately unsupported. Writer calls require an exclusive device.
pub struct VirginSnapshotWriter<D: BlockDevice> {
    volume: Volume<D>,
    manifest: Manifest,
}
impl<D: BlockDevice> VirginSnapshotWriter<D> {
    /// Create an initial v10 metadata-only image on a **fully blank**
    /// disposable device. F0's format checks every physical block first.
    pub fn format(device: D, volume_id: u128) -> Result<Self> {
        if device.block_count() < ROOT_PAYLOAD_LAST + 3 {
            return Err(Error::Geometry);
        }
        let mut volume = Volume::format(device, volume_id)?;
        let total = volume.block_count();
        let fs = Page {
            tree: TreeId::Fs,
            level: 0,
            birth: 1,
            address: total - 1,
            volume_id,
            items: Vec::new(),
        };
        let manifest = Manifest {
            generation: 1,
            high_water: fs.address,
            next_inode: 2,
            roots: TreeRoots {
                fs: fs.pointer()?,
                checksum: None,
                free: None,
                dead_list: None,
                snapshots: None,
                attribute_rows: None,
                attribute_postings: None,
                schema: None,
            },
            sweep_snapshot: 0,
            sweep_key: 0,
            metadata_reserve: 0,
            inode_count: 0,
            used_blocks: 0,
            free_blocks: 0,
            pending_blocks: 0,
        };
        volume.commit_root_with_blocks(
            &[(fs.address, fs.encode()?)],
            &manifest.encode(total)?,
        )?;
        Ok(Self { volume, manifest })
    }

    /// A writer must verify both F0 candidates *completely* in its supported
    /// profile before considering any future virgin metadata allocation.
    pub fn open(device: D) -> Result<Self> {
        let mut volume = Volume::open(device)?;
        let (manifest, _) = audit(&mut volume)?;
        Ok(Self { volume, manifest })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn snapshots(&mut self) -> Result<Vec<SnapshotDescriptor>> {
        let current = self.manifest;
        let volume_id = self.volume_id();
        self.volume.inspect(|device| {
            read_snapshot_tree(
                device, current.roots.snapshots, volume_id, current.generation,
            )
            .map(|result| result.descriptors)
        })
    }

    fn volume_id(&self) -> u128 {
        self.volume.info().volume_id
    }

    /// Atomic F0 publication with metadata-only, **zero-verified** virgin
    /// pages. A nonzero block below the published watermark is an orphan or
    /// unknown owner and is permanently quarantined by descending past it.
    fn commit_edit(&mut self, edit: CatalogEdit) -> Result<u64> {
        let (current, owned) = audit(&mut self.volume)?;
        if current != self.manifest {
            return Err(Error::CorruptTree);
        }
        let next_generation = current.generation.checked_add(1).ok_or(Error::GenerationOverflow)?;
        let volume_id = self.volume_id();
        let total = self.volume.block_count();
        let before = self.volume.inspect(|device| {
            read_snapshot_tree(device, current.roots.snapshots, volume_id, current.generation)
                .map(|scan| scan.descriptors)
        })?;
        let mut expected = before.clone();
        match edit {
            CatalogEdit::Pin(desc) => {
                if expected.iter().any(|d| d.generation == desc.generation) {
                    return Err(Error::AlreadyExists);
                }
                expected.push(desc);
                expected.sort_by_key(|d| d.generation);
            }
            CatalogEdit::MarkDropping(generation) => {
                let desc = expected.iter_mut().find(|d| d.generation == generation)
                    .ok_or(Error::InvalidSnapshot)?;
                if desc.state != SnapshotState::Pinned {
                    return Err(Error::InvalidSnapshot);
                }
                desc.state = SnapshotState::Dropping;
            }
        }
        let mut candidates = Vec::new();
        let mut cursor = current.high_water;
        while cursor > ROOT_PAYLOAD_LAST + 1 && candidates.len() < MAX_STAGING_PAGES {
            cursor -= 1;
            // Fail closed on any *actual* reference regardless of whether
            // a corrupted referenced block happens to contain zeros.
            if owned.contains(&cursor) {
                return Err(Error::CorruptTree);
            }
            let mut block = [0; BLOCK_SIZE];
            self.volume.inspect(|device| device.read_block(cursor, &mut block))?;
            if block.iter().all(|&b| b == 0) {
                candidates.push(cursor);
            }
            // Nonzero orphans are never added to the free pool; the eventual
            // high-water drops past them when a lower block is published.
        }
        if candidates.is_empty() {
            return Err(Error::NoSpace);
        }
        let staged = self.volume.inspect(|device| {
            plan_snapshot_edit(
                device,
                current.roots.snapshots,
                edit,
                volume_id,
                next_generation,
                current.high_water,
                &candidates,
            )
        })?;
        let mut next = current;
        next.generation = next_generation;
        next.high_water = staged.high_water;
        next.roots.snapshots = Some(staged.root);
        let payload = next.encode(total)?;

        // Independently traverse the intended *new* graph in a read-only
        // overlay, while all old/fallback graphs are still untouched.
        self.volume.inspect(|device| {
            let mut overlay = Overlay {
                device,
                staged: staged.staged_pages.iter().copied().collect(),
            };
            let scan = read_snapshot_tree(
                &mut overlay, Some(staged.root), volume_id, next_generation,
            )?;
            if scan.descriptors != expected {
                return Err(Error::CorruptTree);
            }
            for desc in &scan.descriptors {
                if desc.fs_root != current.roots.fs
                    || desc.attribute_rows_root.is_some()
                    || desc.attribute_postings_root.is_some()
                    || desc.attribute_schema_root.is_some()
                {
                    return Err(Error::CorruptTree);
                }
            }
            Ok(())
        })?;
        self.volume.commit_root_with_blocks(&staged.staged_pages, &payload)?;
        self.manifest = next;
        Ok(next_generation)
    }

    /// Pin the last *published* generation, including its fs root. This API
    /// does not permit arbitrary caller-chosen physical block references.
    pub fn pin_current(&mut self) -> Result<u64> {
        let generation = self.manifest.generation;
        let desc = SnapshotDescriptor {
            generation,
            fs_root: self.manifest.roots.fs,
            attribute_rows_root: None,
            attribute_postings_root: None,
            attribute_schema_root: None,
            state: SnapshotState::Pinned,
        };
        self.commit_edit(CatalogEdit::Pin(desc))
    }

    /// Does NOT physically drop or reclaim a snapshot; history remains
    /// protected in the catalog until F5.3 is proven.
    pub fn mark_dropping(&mut self, generation: u64) -> Result<u64> {
        self.commit_edit(CatalogEdit::MarkDropping(generation))
    }

    pub fn into_device(self) -> D {
        self.volume.into_device()
    }
}
