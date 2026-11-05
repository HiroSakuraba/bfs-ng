//! F5 integration: real v10 namespace + file extents + physical checksums
//! under *one* F0 publication boundary. This is still an experimental,
//! single-writer, bounded namespace, not an OS-mountable filesystem.
//!
//! This first integrated tranche supports directories, stable inodes,
//! multiple files, byte-addressed CoW I/O and historical snapshot reads.
//! Unlike the earlier one-file writers, its Fs root is a namespace tree
//! whose file records reference separate v10 extent roots. All live/pinned
//! data and metadata are independently verified across *both* recoverable
//! F0 candidates before any allocation. Certified full two-F0-graph scans now allow
//! conservative recycling, but no incremental free-space index, F4 attribute
//! index, or hard links are enabled yet.
use super::attribute_sections::{AttributeRoots, read_attribute_roots, stage_attribute_roots};
use super::checksum_delta::stage_checksum_delta_reusing;
use super::namespace::stage_namespace_tree_reusing;
use super::snapshot_tree::plan_snapshot_edit_reusing;
use super::space_index::{DeadRun, FreeRun, SpaceDomain, dead_item, free_item,
    read_space_tree, reserve_free, retire_step_cursor, stage_space_delta,
    validate_space_pair, MAX_RETIRE_STEP};
use super::{
    BlockPointer, BlockRef, CatalogEdit, FileExtents, MAX_FILE_BYTES, Manifest, NamespaceRecord,
    Page, SnapshotDescriptor, SnapshotState, TreeId, VirginSnapshotWriter, read_checksum_tree,
    read_namespace_tree, read_page, read_snapshot_tree, read_verified_block, validate_namespace,
    validate_namespace_path,
};
use crate::fs::Catalog;
use crate::volume::{ROOT_PAYLOAD_LAST, RootInfo, crc32c};
use crate::{BLOCK_SIZE, BlockDevice, Error, Result, Volume};
use bfs_ng_attribute_index::{Predicate, QueryResult, Schema, Value};
use std::collections::{BTreeMap, BTreeSet};

const STAGING_LIMIT: usize = 512;
// F5.2 metadata pages are never part of their own persistent free tree.
// This fixed first arena prevents recursive allocator self-ownership.
const INDEX_ARENA_BLOCKS: u64 = 64;
const INDEX_ARENA_END: u64 = ROOT_PAYLOAD_LAST + 1 + INDEX_ARENA_BLOCKS;

struct Reachability {
    records: BTreeMap<String, NamespaceRecord>,
    snapshots: Vec<SnapshotDescriptor>,
    owned_metadata: BTreeSet<u64>,
    data_births: BTreeMap<u64, u64>,
    tail_restrictions: BTreeMap<u64, Vec<usize>>,
    catalog: Option<Catalog>,
}
/// All-or-none historical F4 roots. A live and a pinned snapshot use their
/// own row/posting/schema triplets; never inherit the current generation's.
fn attribute_roots(
    rows: Option<BlockPointer>,
    postings: Option<BlockPointer>,
    schema: Option<BlockPointer>,
) -> Result<Option<AttributeRoots>> {
    match (rows, postings, schema) {
        (None, None, None) => Ok(None),
        (Some(rows), Some(postings), Some(schema)) => Ok(Some(AttributeRoots {
            rows,
            postings,
            schema,
        })),
        _ => Err(Error::CorruptTree),
    }
}
fn validate_attributes(
    catalog: &Catalog,
    records: &BTreeMap<String, NamespaceRecord>,
) -> Result<()> {
    let regular_files: BTreeSet<u64> = records
        .values()
        .filter(|record| !record.directory)
        .map(|record| record.inode)
        .collect();
    catalog.validate_inode_ids(&regular_files)
}

fn collect<D: BlockDevice>(device: &mut D, id: u128, m: Manifest) -> Result<Reachability> {
    if m.used_blocks != 0 {

        return Err(Error::CorruptTree);
    }
    let snapshots = read_snapshot_tree(device, m.roots.snapshots, id, m.generation)?;
    let mut all_ns_pages = BTreeSet::new();
    let mut all_ns_roots: BTreeMap<u64, BlockPointer> = BTreeMap::new();
    // A shared physical file root across snapshots must belong to the SAME
    // stable inode. The first v10 profile has no reflinks or hard links.
    let mut all_files: BTreeMap<u64, (BlockPointer, u64)> = BTreeMap::new();
    let mut records = None;
    let mut namespace_versions = BTreeMap::new();
    let current_root = m.roots.fs;
    all_ns_roots.insert(current_root.address, current_root);
    for desc in &snapshots.descriptors {
        attribute_roots(
            desc.attribute_rows_root,
            desc.attribute_postings_root,
            desc.attribute_schema_root,
        )?;
        match all_ns_roots.insert(desc.fs_root.address, desc.fs_root) {
            Some(previous) if previous != desc.fs_root => return Err(Error::CorruptTree),
            _ => {}
        }
    }
    let mut data_births = BTreeMap::new();
    let mut tails: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for root in all_ns_roots.values() {
        let scan = read_namespace_tree(device, *root, id, m.generation, m.next_inode)?;
        all_ns_pages.extend(scan.metadata_pages);
        namespace_versions.insert(root.address, scan.records.clone());
        if *root == current_root {
            records = Some(scan.records.clone());
        }
        for entry in scan.records.values() {
            if let Some(file_root) = entry.file_root {
                match all_files.insert(file_root.address, (file_root, entry.inode)) {
                    Some(previous) if previous != (file_root, entry.inode) => {
                        return Err(Error::CorruptTree);
                    }
                    _ => {}
                }
            }
        }
    }
    // Every historical descriptor owns its own attribute/index/schema
    // generation. Identical physical pages can be shared by snapshots, but
    // role aliases or overlaps with namespace/snapshot metadata are invalid.
    let mut all_attribute_pages = BTreeSet::new();
    let mut live_catalog = None;
    let mut attribute_versions = Vec::new();
    attribute_versions.push((
        current_root.address,
        attribute_roots(
            m.roots.attribute_rows,
            m.roots.attribute_postings,
            m.roots.schema,
        )?,
        true,
    ));
    for desc in &snapshots.descriptors {
        attribute_versions.push((
            desc.fs_root.address,
            attribute_roots(
                desc.attribute_rows_root,
                desc.attribute_postings_root,
                desc.attribute_schema_root,
            )?,
            false,
        ));
    }
    for (namespace_address, roots, is_live) in attribute_versions {
        let checked = read_attribute_roots(device, roots, id, m.generation)?;
        if let Some(attributes) = checked {
            let names = namespace_versions
                .get(&namespace_address)
                .ok_or(Error::CorruptTree)?;
            validate_attributes(&attributes.catalog, names)?;
            all_attribute_pages.extend(attributes.pages);
            if is_live {
                live_catalog = Some(attributes.catalog);
            }
        }
    }
    if !all_ns_pages.is_disjoint(&all_attribute_pages)
        || !snapshots.owned_pages.is_disjoint(&all_attribute_pages)
    {
        return Err(Error::CorruptTree);
    }
    all_ns_pages.extend(all_attribute_pages);
    let live_records = records.ok_or(Error::CorruptTree)?;
    if live_records.len() as u64 != m.inode_count {
        return Err(Error::CorruptTree);
    }
    let mut file_pages = BTreeSet::new();
    let mut data_owner = BTreeMap::<u64, u64>::new();
    for &(file_root, inode) in all_files.values() {
        let page = read_page(device, file_root, TreeId::Fs, id, m.generation)?;
        let file = FileExtents::decode(&page, device.block_count())?;
        if !file_pages.insert(file_root.address) {
            return Err(Error::CorruptTree);
        }
        for (&logical, &reference) in &file.blocks {
            match data_births.insert(reference.physical, reference.birth) {
                Some(previous) if previous != reference.birth => return Err(Error::CorruptTree),
                _ => {}
            }
            match data_owner.insert(reference.physical, inode) {
                Some(previous) if previous != inode => {
                    // An extent from one file cannot alias another file.
                    return Err(Error::CorruptTree);
                }
                _ => {}
            }
            if logical == file.size / BLOCK_SIZE as u64 && file.size % BLOCK_SIZE as u64 != 0 {
                tails
                    .entry(reference.physical)
                    .or_default()
                    .push((file.size % BLOCK_SIZE as u64) as usize);
            }
        }
    }
    if !file_pages.is_disjoint(&all_ns_pages) {
        return Err(Error::CorruptTree);
    }
    all_ns_pages.extend(file_pages);
    if !all_ns_pages.is_disjoint(&snapshots.owned_pages) {
        return Err(Error::CorruptTree);
    }
    all_ns_pages.extend(snapshots.owned_pages);
    Ok(Reachability {
        records: live_records,
        snapshots: snapshots.descriptors,
        owned_metadata: all_ns_pages,
        data_births,
        tail_restrictions: tails,
        catalog: live_catalog,
    })
}
struct Verified {
    reach: Reachability,
    checksums: BTreeMap<u64, u32>,
    owned: BTreeSet<u64>,
    free: BTreeMap<u64, FreeRun>,
    dead: BTreeMap<(u64, u64), DeadRun>,
    global_pages: BTreeSet<u64>,
    retired_global: BTreeSet<u64>,
}
fn verify<D: BlockDevice>(device: &mut D, root: RootInfo, m: Manifest) -> Result<Verified> {
    let reach = collect(device, root.volume_id, m)?;
    let checksums = read_checksum_tree(device, m.roots.checksum, root.volume_id, root.generation)?;
    if !reach.owned_metadata.is_disjoint(&checksums.metadata_pages) {
        return Err(Error::CorruptTree);
    }
    let expected: BTreeSet<u64> = reach.data_births.keys().copied().collect();
    if expected != checksums.checksums.keys().copied().collect() {
        return Err(Error::CorruptTree);
    }
    let mut owned = reach.owned_metadata.clone();
    let mut global_pages = checksums.metadata_pages.clone();
    owned.extend(checksums.metadata_pages);
    for physical in expected {
        if !owned.insert(physical) {
            return Err(Error::CorruptTree);
        }
        let bytes = read_verified_block(device, physical, &checksums.checksums)?;
        if let Some(restrictions) = reach.tail_restrictions.get(&physical) {
            for &tail_start in restrictions {
                if bytes[tail_start..].iter().any(|&byte| byte != 0) {
                    return Err(Error::CorruptTree);
                }
            }
        }
    }
    let (free, dead) = if m.metadata_reserve == 0 {
        if m.roots.free.is_some() || m.roots.dead_list.is_some()
            || m.free_blocks != 0 || m.pending_blocks != 0
            || m.sweep_snapshot != 0 || m.sweep_key != 0
        {
            return Err(Error::CorruptTree);
        }
        (BTreeMap::new(), BTreeMap::new())
    } else {
        if m.metadata_reserve != INDEX_ARENA_BLOCKS
            || device.block_count() < 256
            || (m.sweep_snapshot == 0 && m.sweep_key != 0)
        {
            return Err(Error::CorruptTree);
        }
        let f = read_space_tree(
            device, TreeId::Free, m.roots.free, root.volume_id, root.generation,
        )?;
        let d = read_space_tree(
            device, TreeId::DeadList, m.roots.dead_list,
            root.volume_id, root.generation,
        )?;
        if !f.pages.is_disjoint(&d.pages)
            || f.pages.iter().chain(d.pages.iter())
                .any(|&a| a <= ROOT_PAYLOAD_LAST || a >= INDEX_ARENA_END)
            || !f.pages.is_disjoint(&owned) || !d.pages.is_disjoint(&owned)
            || f.free.values().any(|run| run.start < INDEX_ARENA_END)
            || d.dead.values().any(|run| run.start < INDEX_ARENA_END)
        {
            return Err(Error::CorruptTree);
        }
        let free_count: u64 = f.free.values().map(|run| u64::from(run.blocks)).sum();
        let dead_count: u64 = d.dead.values().map(|run| u64::from(run.blocks)).sum();
        if free_count != m.free_blocks || dead_count != m.pending_blocks {
            return Err(Error::CorruptTree);
        }
        owned.extend(f.pages.iter().copied());
        owned.extend(d.pages.iter().copied());
        global_pages.extend(f.pages);
        global_pages.extend(d.pages);
        validate_space_pair(
            &f.free, &d.dead, &owned, root.generation, device.block_count(),
        )?;
        (f.free, d.dead)
    };
    if owned.iter().any(|&address| address < m.high_water
        && (m.metadata_reserve == 0 || address >= INDEX_ARENA_END))
    {
        return Err(Error::CorruptTree);
    }
    Ok(Verified {
        reach, checksums: checksums.checksums, owned,
        free, dead, global_pages, retired_global: BTreeSet::new(),
    })
}
fn audit<D: BlockDevice>(volume: &mut Volume<D>) -> Result<(Manifest, Verified, BTreeSet<u64>)> {
    let latest = volume.info();
    let mut selected = None;
    let mut owned = BTreeSet::new();
    let mut previous: Option<(u64, u64)> = None;
    let mut fallback_global = BTreeSet::new();
    for (root, payload) in volume.candidate_roots()? {
        if root.generation == 0 {
            if payload.is_some() {
                return Err(Error::CorruptRoot);
            }
            continue;
        }
        let raw = payload.ok_or(Error::CorruptRoot)?;
        let m = Manifest::decode(&raw, root, volume.block_count())?;
        let checked = volume.inspect(|device| verify(device, root, m))?;
        if let Some((generation, watermark)) = previous {
            if root.generation != generation + 1 || m.high_water > watermark {
                return Err(Error::CorruptTree);
            }
        }
        previous = Some((root.generation, m.high_water));
        owned.extend(checked.owned.iter().copied());
        if root.generation == latest.generation {
            selected = Some((m, checked));
        } else {
            fallback_global = checked.global_pages.clone();
        }
    }
    let (m, mut checked) = selected.ok_or(Error::CorruptRoot)?;
    // These pages vanish when the oldest candidate F0 slot is replaced.
    // Label global pages independently of snapshot-referenced metadata.
    checked.retired_global = fallback_global.difference(&checked.owned)
        .copied().collect();
    if m.metadata_reserve != 0 {
        validate_space_pair(
            &checked.free, &checked.dead, &owned, m.generation, volume.block_count(),
        )?;
    }
    Ok((m, checked, owned))
}
struct Overlay<'a, D: BlockDevice> {
    disk: &'a mut D,
    staged: BTreeMap<u64, [u8; BLOCK_SIZE]>,
}
impl<D: BlockDevice> BlockDevice for Overlay<'_, D> {
    fn block_count(&self) -> u64 {
        self.disk.block_count()
    }
    fn read_block(&mut self, n: u64, into: &mut [u8; BLOCK_SIZE]) -> Result<()> {
        if let Some(bytes) = self.staged.get(&n) {
            *into = *bytes;
            Ok(())
        } else {
            self.disk.read_block(n, into)
        }
    }
    fn write_block(&mut self, _n: u64, _bytes: &[u8; BLOCK_SIZE]) -> Result<()> {
        Err(Error::Poisoned)
    }
    fn sync(&mut self) -> Result<()> {
        Err(Error::Poisoned)
    }
}
fn parent_is_directory(records: &BTreeMap<String, NamespaceRecord>, path: &str) -> Result<()> {
    validate_namespace_path(path)?;
    let at = path.rfind('/').ok_or(Error::InvalidPath)?;
    if at == 0 {
        return Ok(());
    }
    match records.get(&path[..at]) {
        Some(record) if record.directory => Ok(()),
        Some(_) => Err(Error::NotDirectory),
        None => Err(Error::NotFound),
    }
}
fn rename_records(
    records: &mut BTreeMap<String, NamespaceRecord>,
    from: &str,
    to: &str,
) -> Result<()> {
    validate_namespace_path(from)?;
    validate_namespace_path(to)?;
    let original = *records.get(from).ok_or(Error::NotFound)?;
    if from == to {
        return Ok(());
    }
    if records.contains_key(to) {
        return Err(Error::AlreadyExists);
    }
    if original.directory && to.starts_with(&format!("{from}/")) {
        return Err(Error::InvalidPath);
    }
    parent_is_directory(records, to)?;
    let prefix = format!("{from}/");
    let moved = records
        .keys()
        .filter(|key| key.as_str() == from || (original.directory && key.starts_with(&prefix)))
        .map(|key| {
            let next = format!("{to}{}", key.strip_prefix(from).ok_or(Error::CorruptTree)?);
            validate_namespace_path(&next)?;
            Ok((key.clone(), next))
        })
        .collect::<Result<Vec<_>>>()?;
    let old_names: BTreeSet<&str> = moved.iter().map(|(name, _)| name.as_str()).collect();
    for (_, name) in &moved {
        if records.contains_key(name) && !old_names.contains(name.as_str()) {
            return Err(Error::AlreadyExists);
        }
    }
    for (old, new) in moved {
        let entry = records.remove(&old).ok_or(Error::CorruptTree)?;
        records.insert(new, entry);
    }
    Ok(())
}
#[derive(Clone, Copy)]
enum Edit<'a> {
    Mkdir(&'a str),
    Write {
        path: &'a str,
        offset: u64,
        bytes: &'a [u8],
    },
    Truncate {
        path: &'a str,
        size: u64,
    },
    Rename {
        from: &'a str,
        to: &'a str,
    },
    Unlink(&'a str),
    InstallSchema(&'a Schema),
    SetAttributes {
        path: &'a str,
        values: &'a [(String, Value)],
    },
    Pin,
    MarkDropping(u64),
}

/// One coherent v10 namespace/data/checksum writer for multiple files.
/// It is single-writer, bounded (4KiB metadata pages; 256KiB per file),
/// and virgin-only. No mount, hard links or incremental retirement yet.
pub struct UnifiedV10<D: BlockDevice> {
    volume: Volume<D>,
    manifest: Manifest,
    last_reused_blocks: usize,
}
impl<D: BlockDevice> UnifiedV10<D> {
    pub fn format(device: D, id: u128) -> Result<Self> {
        let base = VirginSnapshotWriter::format(device, id)?;
        Self::open(base.into_device())
    }
    pub fn open(device: D) -> Result<Self> {
        let mut volume = Volume::open(device)?;
        let (manifest, _, _) = audit(&mut volume)?;
        Ok(Self {
            volume,
            manifest,
            last_reused_blocks: 0,
        })
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    /// Nonzero **unowned** physical blocks overwritten by the last successful
    /// transaction after a complete both-F0-root ownership audit. This is a
    /// diagnostic, not an incremental allocator certificate.
    pub fn last_reused_blocks(&self) -> usize {
        self.last_reused_blocks
    }
    pub fn into_device(self) -> D {
        self.volume.into_device()
    }
    pub fn list_dir(&mut self, path: &str) -> Result<Vec<String>> {
        let (m, verified, _) = audit(&mut self.volume)?;
        if m != self.manifest {
            return Err(Error::CorruptTree);
        }
        if path != "/" {
            validate_namespace_path(path)?;
            if !verified
                .reach
                .records
                .get(path)
                .is_some_and(|r| r.directory)
            {
                return Err(Error::NotDirectory);
            }
        }
        let prefix = if path == "/" {
            "/".to_owned()
        } else {
            format!("{path}/")
        };
        Ok(verified
            .reach
            .records
            .keys()
            .filter(|name| name.starts_with(&prefix))
            .filter(|name| !name[prefix.len()..].contains('/'))
            .cloned()
            .collect())
    }
    pub fn stat(&mut self, path: &str) -> Result<(u64, bool, u64)> {
        validate_namespace_path(path)?;
        let (m, verified, _) = audit(&mut self.volume)?;
        if m != self.manifest {
            return Err(Error::CorruptTree);
        }
        let entry = *verified.reach.records.get(path).ok_or(Error::NotFound)?;
        if entry.directory {
            return Ok((entry.inode, true, 0));
        }
        let id = self.volume.info().volume_id;
        let size = self.volume.inspect(|device| {
            if let Some(root) = entry.file_root {
                let page = read_page(device, root, TreeId::Fs, id, m.generation)?;
                Ok(FileExtents::decode(&page, device.block_count())?.size)
            } else {
                Ok(0)
            }
        })?;
        Ok((entry.inode, false, size))
    }
    fn read(
        &mut self,
        path: &str,
        snapshot: Option<u64>,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        validate_namespace_path(path)?;
        let (m, checked, _) = audit(&mut self.volume)?;
        if m != self.manifest {
            return Err(Error::CorruptTree);
        }
        let id = self.volume.info().volume_id;
        self.volume.inspect(|device| {
            let entry = if let Some(generation) = snapshot {
                let desc = checked
                    .reach
                    .snapshots
                    .iter()
                    .find(|desc| desc.generation == generation)
                    .ok_or(Error::InvalidSnapshot)?;
                read_namespace_tree(device, desc.fs_root, id, m.generation, m.next_inode)?
                    .records
                    .get(path)
                    .copied()
                    .ok_or(Error::NotFound)?
            } else {
                *checked.reach.records.get(path).ok_or(Error::NotFound)?
            };
            if entry.directory {
                return Err(Error::IsDirectory);
            }
            let file = if let Some(root) = entry.file_root {
                let page = read_page(device, root, TreeId::Fs, id, m.generation)?;
                FileExtents::decode(&page, device.block_count())?
            } else {
                FileExtents::default()
            };
            if offset >= file.size || length == 0 {
                return Ok(Vec::new());
            }
            let count = (file.size - offset).min(length as u64) as usize;
            let mut out = vec![0_u8; count];
            let mut copied = 0;
            while copied < count {
                let pos = offset + copied as u64;
                let logical = pos / BLOCK_SIZE as u64;
                let within = (pos % BLOCK_SIZE as u64) as usize;
                let take = (BLOCK_SIZE - within).min(count - copied);
                if let Some(reference) = file.blocks.get(&logical) {
                    let bytes =
                        read_verified_block(device, reference.physical, &checked.checksums)?;
                    out[copied..copied + take].copy_from_slice(&bytes[within..within + take]);
                }
                copied += take;
            }
            Ok(out)
        })
    }
    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>> {
        self.read(path, None, 0, MAX_FILE_BYTES as usize)
    }
    pub fn read_at(&mut self, path: &str, offset: u64, length: usize) -> Result<Vec<u8>> {
        self.read(path, None, offset, length)
    }
    pub fn read_snapshot_file(&mut self, generation: u64, path: &str) -> Result<Vec<u8>> {
        self.read(path, Some(generation), 0, MAX_FILE_BYTES as usize)
    }
    pub fn snapshots(&mut self) -> Result<Vec<SnapshotDescriptor>> {
        let (_, checked, _) = audit(&mut self.volume)?;
        Ok(checked.reach.snapshots)
    }
    /// The F4 schema is immutable once installed, as in v9.
    pub fn install_schema(&mut self, schema: &Schema) -> Result<u64> {
        self.commit(Edit::InstallSchema(schema))
    }
    /// Atomically replace one regular inode's typed F4 attributes.
    pub fn set_attributes(&mut self, path: &str, values: &[(String, Value)]) -> Result<u64> {
        self.commit(Edit::SetAttributes { path, values })
    }
    pub fn file_attributes(&mut self, path: &str) -> Result<BTreeMap<String, Value>> {
        validate_namespace_path(path)?;
        let (m, verified, _) = audit(&mut self.volume)?;
        if m != self.manifest {
            return Err(Error::CorruptTree);
        }
        let record = verified.reach.records.get(path).ok_or(Error::NotFound)?;
        if record.directory {
            return Err(Error::IsDirectory);
        }
        Ok(verified
            .reach
            .catalog
            .as_ref()
            .map(|catalog| catalog.get(record.inode))
            .unwrap_or_default())
    }
    pub fn search_attributes(
        &mut self,
        predicates: &[Predicate],
        limit: usize,
    ) -> Result<QueryResult> {
        let (m, checked, _) = audit(&mut self.volume)?;
        if m != self.manifest {
            return Err(Error::CorruptTree);
        }
        let catalog = checked
            .reach
            .catalog
            .as_ref()
            .ok_or(Error::Attribute("no schema installed".into()))?;
        catalog.query(
            self.volume.info().volume_id,
            m.generation,
            predicates,
            limit,
        )
    }
    pub fn search_snapshot_attributes(
        &mut self,
        generation: u64,
        predicates: &[Predicate],
        limit: usize,
    ) -> Result<QueryResult> {
        let (m, checked, _) = audit(&mut self.volume)?;
        if m != self.manifest {
            return Err(Error::CorruptTree);
        }
        let snapshot = checked
            .reach
            .snapshots
            .iter()
            .find(|descriptor| descriptor.generation == generation)
            .ok_or(Error::InvalidSnapshot)?;
        let roots = attribute_roots(
            snapshot.attribute_rows_root,
            snapshot.attribute_postings_root,
            snapshot.attribute_schema_root,
        )?;
        let id = self.volume.info().volume_id;
        let historical = self
            .volume
            .inspect(|device| read_attribute_roots(device, roots, id, m.generation))?;
        let catalog = historical
            .ok_or(Error::Attribute("snapshot has no schema".into()))?
            .catalog;
        catalog.query(id, generation, predicates, limit)
    }
    pub fn mkdir(&mut self, path: &str) -> Result<u64> {
        self.commit(Edit::Mkdir(path))
    }
    pub fn write_at(&mut self, path: &str, offset: u64, bytes: &[u8]) -> Result<u64> {
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or(Error::TooLarge)?;
        if end > MAX_FILE_BYTES {
            return Err(Error::TooLarge);
        }
        if bytes.is_empty() {
            return Ok(self.manifest.generation);
        }
        self.commit(Edit::Write {
            path,
            offset,
            bytes,
        })
    }
    pub fn truncate(&mut self, path: &str, size: u64) -> Result<u64> {
        if size > MAX_FILE_BYTES {
            return Err(Error::TooLarge);
        }
        self.commit(Edit::Truncate { path, size })
    }
    pub fn rename(&mut self, from: &str, to: &str) -> Result<u64> {
        self.commit(Edit::Rename { from, to })
    }
    pub fn remove_file(&mut self, path: &str) -> Result<u64> {
        self.commit(Edit::Unlink(path))
    }
    pub fn pin_current(&mut self) -> Result<u64> {
        self.commit(Edit::Pin)
    }
    pub fn mark_dropping(&mut self, generation: u64) -> Result<u64> {
        self.commit(Edit::MarkDropping(generation))
    }
    fn commit(&mut self, edit: Edit<'_>) -> Result<u64> {
        let (old, checked, owned) = audit(&mut self.volume)?;
        if old != self.manifest {
            return Err(Error::CorruptTree);
        }
        let generation = old
            .generation
            .checked_add(1)
            .ok_or(Error::GenerationOverflow)?;
        let id = self.volume.info().volume_id;
        let total = self.volume.block_count();
        let mut records = checked.reach.records.clone();
        let mut pins = checked.reach.snapshots.clone();
        let mut attributes = checked.reach.catalog.clone();
        let mut attributes_dirty = false;
        let mut next = old;
        next.generation = generation;
        let mut catalog_edit = None;
        let mut new_file: Option<(String, FileExtents)> = None;
        let mut dirty = BTreeMap::<u64, [u8; BLOCK_SIZE]>::new();
        let mut namespace_changed = false;
        match edit {
            Edit::Mkdir(path) => {
                parent_is_directory(&records, path)?;
                if records.contains_key(path) {
                    return Err(Error::AlreadyExists);
                }
                let inode = next.next_inode;
                next.next_inode = inode.checked_add(1).ok_or(Error::GenerationOverflow)?;
                records.insert(path.to_owned(), NamespaceRecord::directory(inode));
                namespace_changed = true;
            }
            Edit::Write {
                path,
                offset,
                bytes,
            } => {
                parent_is_directory(&records, path)?;
                let entry = if let Some(entry) = records.get(path) {
                    if entry.directory {
                        return Err(Error::IsDirectory);
                    }
                    *entry
                } else {
                    let inode = next.next_inode;
                    next.next_inode = inode.checked_add(1).ok_or(Error::GenerationOverflow)?;
                    NamespaceRecord::file(inode, None)
                };
                let mut file = self.volume.inspect(|device| {
                    if let Some(root) = entry.file_root {
                        let page = read_page(device, root, TreeId::Fs, id, old.generation)?;
                        FileExtents::decode(&page, device.block_count())
                    } else {
                        Ok(FileExtents::default())
                    }
                })?;
                file.size = file.size.max(offset + bytes.len() as u64);
                let mut copied = 0;
                while copied < bytes.len() {
                    let pos = offset + copied as u64;
                    let logical = pos / BLOCK_SIZE as u64;
                    let within = (pos % BLOCK_SIZE as u64) as usize;
                    let count = (BLOCK_SIZE - within).min(bytes.len() - copied);
                    if let std::collections::btree_map::Entry::Vacant(slot) = dirty.entry(logical) {
                        let prior = if let Some(reference) = file.blocks.get(&logical) {
                            self.volume.inspect(|device| {
                                read_verified_block(device, reference.physical, &checked.checksums)
                            })?
                        } else {
                            [0_u8; BLOCK_SIZE]
                        };
                        slot.insert(prior);
                    }
                    dirty.get_mut(&logical).ok_or(Error::CorruptTree)?[within..within + count]
                        .copy_from_slice(&bytes[copied..copied + count]);
                    copied += count;
                }
                records.insert(path.to_owned(), entry);
                new_file = Some((path.to_owned(), file));
                namespace_changed = true;
            }
            Edit::Truncate { path, size } => {
                let entry = *records.get(path).ok_or(Error::NotFound)?;
                if entry.directory {
                    return Err(Error::IsDirectory);
                }
                let mut file = self.volume.inspect(|device| {
                    if let Some(root) = entry.file_root {
                        let page = read_page(device, root, TreeId::Fs, id, old.generation)?;
                        FileExtents::decode(&page, device.block_count())
                    } else {
                        Ok(FileExtents::default())
                    }
                })?;
                if file.size == size {
                    return Ok(old.generation);
                }
                file.size = size;
                let remaining = size.div_ceil(BLOCK_SIZE as u64);
                file.blocks.retain(|&logical, _| logical < remaining);
                if size % BLOCK_SIZE as u64 != 0 {
                    let last = remaining - 1;
                    if let Some(reference) = file.blocks.get(&last) {
                        let mut data = self.volume.inspect(|device| {
                            read_verified_block(device, reference.physical, &checked.checksums)
                        })?;
                        let tail = (size % BLOCK_SIZE as u64) as usize;
                        if data[tail..].iter().any(|&byte| byte != 0) {
                            data[tail..].fill(0);
                            dirty.insert(last, data);
                        }
                    }
                }
                new_file = Some((path.to_owned(), file));
                namespace_changed = true;
            }
            Edit::Rename { from, to } => {
                if from == to {
                    validate_namespace_path(from)?;
                    if !records.contains_key(from) {
                        return Err(Error::NotFound);
                    }
                    return Ok(old.generation);
                }
                rename_records(&mut records, from, to)?;
                namespace_changed = true;
            }
            Edit::Unlink(path) => {
                validate_namespace_path(path)?;
                let entry = *records.get(path).ok_or(Error::NotFound)?;
                if entry.directory {
                    return Err(Error::IsDirectory);
                }
                if let Some(catalog) = &mut attributes {
                    catalog.remove(entry.inode)?;
                    attributes_dirty = true;
                }
                records.remove(path);
                namespace_changed = true;
            }
            Edit::InstallSchema(schema) => {
                if attributes.is_some() {
                    return Err(Error::Attribute("schema already installed".into()));
                }
                attributes = Some(Catalog::empty(schema.clone())?);
                attributes_dirty = true;
            }
            Edit::SetAttributes { path, values } => {
                validate_namespace_path(path)?;
                let entry = *records.get(path).ok_or(Error::NotFound)?;
                if entry.directory {
                    return Err(Error::IsDirectory);
                }
                let catalog = attributes
                    .as_mut()
                    .ok_or(Error::Attribute("no schema installed".into()))?;
                catalog.upsert(entry.inode, values.iter().cloned())?;
                attributes_dirty = true;
            }
            Edit::Pin => {
                if pins.iter().any(|d| d.generation == old.generation) {
                    return Err(Error::AlreadyExists);
                }
                let desc = SnapshotDescriptor {
                    generation: old.generation,
                    fs_root: old.roots.fs,
                    attribute_rows_root: old.roots.attribute_rows,
                    attribute_postings_root: old.roots.attribute_postings,
                    attribute_schema_root: old.roots.schema,
                    state: SnapshotState::Pinned,
                };
                pins.push(desc);
                pins.sort_by_key(|d| d.generation);
                catalog_edit = Some(CatalogEdit::Pin(desc));
            }
            Edit::MarkDropping(snapshot) => {
                let desc = pins
                    .iter_mut()
                    .find(|d| d.generation == snapshot)
                    .ok_or(Error::InvalidSnapshot)?;
                if desc.state != SnapshotState::Pinned {
                    return Err(Error::InvalidSnapshot);
                }
                desc.state = SnapshotState::Dropping;
                catalog_edit = Some(CatalogEdit::MarkDropping(snapshot));
            }
        }
        next.inode_count = records.len() as u64;
        validate_namespace(&records, next.next_inode, generation, total)?;
        if let Some(catalog) = &attributes {
            validate_attributes(catalog, &records)?;
        }
        // Strict reusable-address certificate for this one transaction:
        // the independently reconstructed *union* of both F0 candidate
        // graphs is the authority, NOT a high-water guess, a self CRC, or
        // a free-list bit. Blocks absent from every recoverable root are
        // eligible even if they contain stale data or crash orphans.
        // Persistent free-tree/epoch accounting is deferred to F5.2/F5.3.
        let candidates: Vec<u64> = (ROOT_PAYLOAD_LAST + 1..total)
            .rev()
            .filter(|block| !owned.contains(block))
            .take(STAGING_LIMIT)
            .collect();
        let mut used = 0;
        let mut staged: Vec<(u64, [u8; BLOCK_SIZE])> = Vec::new();
        let mut added_crc = BTreeMap::new();
        if let Some((path, mut file)) = new_file {
            for (logical, bytes) in dirty.iter().rev() {
                let physical = *candidates.get(used).ok_or(Error::NoSpace)?;
                used += 1;
                file.blocks.insert(
                    *logical,
                    BlockRef {
                        physical,
                        birth: generation,
                    },
                );
                staged.push((physical, *bytes));
                added_crc.insert(physical, crc32c(bytes));
            }
            let file_root = if file.size == 0 {
                None
            } else {
                let address = *candidates.get(used).ok_or(Error::NoSpace)?;
                used += 1;
                let page = Page {
                    tree: TreeId::Fs,
                    level: 0,
                    address,
                    birth: generation,
                    volume_id: id,
                    items: file.encode_items(generation, total)?,
                };
                let pointer = page.pointer()?;
                staged.push((address, page.encode()?));
                Some(pointer)
            };
            let inode = records.get(&path).ok_or(Error::CorruptTree)?.inode;
            records.insert(path, NamespaceRecord::file(inode, file_root));
        }
        if namespace_changed {
            let ns = stage_namespace_tree_reusing(
                &records,
                &candidates[used..],
                id,
                generation,
                next.next_inode,
                old.high_water,
                total,
            )?;
            used += ns.used;
            next.roots.fs = ns.root;
            staged.extend(ns.pages);
        }
        if let Some(edit) = catalog_edit {
            let cat = self.volume.inspect(|device| {
                plan_snapshot_edit_reusing(
                    device,
                    old.roots.snapshots,
                    edit,
                    id,
                    generation,
                    old.high_water,
                    &candidates[used..],
                )
            })?;
            used += cat.consumed_reservations;
            next.roots.snapshots = Some(cat.root);
            staged.extend(cat.staged_pages);
        }
        if attributes_dirty {
            let catalog = attributes.as_ref().ok_or(Error::CorruptTree)?;
            let rebuilt =
                stage_attribute_roots(catalog, &candidates[used..], id, generation, total)?;
            used += rebuilt.used;
            next.roots.attribute_rows = Some(rebuilt.roots.rows);
            next.roots.attribute_postings = Some(rebuilt.roots.postings);
            next.roots.schema = Some(rebuilt.roots.schema);
            staged.extend(rebuilt.pages);
        }
        // Collect complete live/pinned extent references from the candidate
        // staged graph before constructing its new global checksum index.
        let required = self.volume.inspect(|device| {
            let mut overlay = Overlay {
                disk: device,
                staged: staged.iter().copied().collect(),
            };
            collect(&mut overlay, id, next).map(|graph| graph.data_births)
        })?;
        let mut expected_crc = BTreeMap::new();
        for physical in required.keys() {
            let checksum = added_crc
                .get(physical)
                .or_else(|| checked.checksums.get(physical))
                .ok_or(Error::CorruptTree)?;
            expected_crc.insert(*physical, *checksum);
        }
        let mut delta = BTreeMap::new();
        for (&physical, &crc) in &checked.checksums {
            match expected_crc.get(&physical) {
                None => {
                    delta.insert(physical, None);
                }
                Some(&new_crc) if new_crc != crc => return Err(Error::CorruptTree),
                _ => {}
            }
        }
        for (&physical, &crc) in &expected_crc {
            if !checked.checksums.contains_key(&physical) {
                delta.insert(physical, Some(crc));
            }
        }
        if !delta.is_empty() {
            let cs = self.volume.inspect(|device| {
                stage_checksum_delta_reusing(
                    device,
                    old.roots.checksum,
                    &delta,
                    &candidates[used..],
                    id,
                    generation,
                    old.high_water,
                )
            })?;
            used += cs.consumed_reservations;
            next.roots.checksum = cs.root;
            staged.extend(cs.staged_pages);
        }
        if used == 0 {
            return Err(Error::CorruptTree);
        }
        // A reclaimed page above the virgin frontier must never advance it.
        // Newly used virgin pages below it still lower the frontier.
        next.high_water = old.high_water.min(candidates[used - 1]);
        let payload = next.encode(total)?;
        self.volume.inspect(|device| {
            let mut overlay = Overlay {
                disk: device,
                staged: staged.iter().copied().collect(),
            };
            let root = RootInfo {
                volume_id: id,
                generation,
                root_block: Some(2 + (generation - 1) % 3),
            };
            let checked = verify(&mut overlay, root, next)?;
            if checked.reach.records != records || checked.reach.snapshots != pins {
                return Err(Error::CorruptTree);
            }
            let actual_attrs = checked
                .reach
                .catalog
                .as_ref()
                .map(Catalog::encode_sections)
                .transpose()?;
            let expected_attrs = attributes
                .as_ref()
                .map(Catalog::encode_sections)
                .transpose()?;
            if actual_attrs != expected_attrs {
                return Err(Error::CorruptTree);
            }
            let staged_addrs: BTreeSet<u64> = staged.iter().map(|(addr, _)| *addr).collect();
            if staged_addrs.len() != staged.len() || !staged_addrs.is_disjoint(&owned) {
                return Err(Error::CorruptTree);
            }
            Ok(())
        })?;
        // Record actual overwrites of nonzero, independently unowned pages.
        // Do this only after the entire candidate graph has passed verification.
        let reused = self.volume.inspect(|device| {
            let mut count = 0;
            for &address in &candidates[..used] {
                let mut bytes = [0; BLOCK_SIZE];
                device.read_block(address, &mut bytes)?;
                count += usize::from(bytes.iter().any(|&byte| byte != 0));
            }
            Ok(count)
        })?;
        self.volume.commit_root_with_blocks(&staged, &payload)?;
        self.last_reused_blocks = reused;
        self.manifest = next;
        Ok(generation)
    }
}
