//! F5.4 integration foundation: one authenticated v10 directory/inode namespace.
//!
//! The v10 Fs *root* is a high-key sorted B+tree of pathname entries. Each
//! regular-file entry points to a separate, immutable v10 Fs **extent leaf**.
//! Thus a namespace update and its data/checksum roots can be committed in
//! the *same* F0 manifest, without an independent v9 image or sidecar index.
//! This first profile does not provide hard links, symlinks, permissions, or
//! a multilevel per-file extent tree. Names remain bounded at 240 bytes.
use super::{BlockPointer, Item, MAX_LEVEL, PAGE_HEADER_BYTES, Page, TreeId, read_page};
use crate::volume::ROOT_PAYLOAD_LAST;
use crate::{BLOCK_SIZE, BlockDevice, Error, Result};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_NAMESPACE_RECORDS: usize = 2048;
pub const MAX_NAMESPACE_PAGES: usize = 8192;
const RECORD_BYTES: usize = 40;
const MAX_PATH_BYTES: usize = 240;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamespaceRecord {
    pub inode: u64,
    pub directory: bool,
    /// Historical fs extent-root BP, absent for directories or empty files.
    pub file_root: Option<BlockPointer>,
}
impl NamespaceRecord {
    pub fn directory(inode: u64) -> Self {
        Self {
            inode,
            directory: true,
            file_root: None,
        }
    }
    pub fn file(inode: u64, file_root: Option<BlockPointer>) -> Self {
        Self {
            inode,
            directory: false,
            file_root,
        }
    }
    fn encode(self) -> [u8; RECORD_BYTES] {
        let mut bytes = [0; RECORD_BYTES];
        bytes[..8].copy_from_slice(&self.inode.to_le_bytes());
        bytes[8] = if self.directory { 1 } else { 2 };
        if let Some(root) = self.file_root {
            bytes[16..40].copy_from_slice(&root.encode());
        }
        bytes
    }
    fn decode(bytes: &[u8], generation: u64, total: u64) -> Result<Self> {
        if bytes.len() != RECORD_BYTES || bytes[9..16] != [0; 7] {
            return Err(Error::CorruptTree);
        }
        let inode = u64::from_le_bytes(bytes[..8].try_into().map_err(|_| Error::CorruptTree)?);
        if inode < 2 {
            return Err(Error::CorruptTree);
        }
        let directory = match bytes[8] {
            1 => true,
            2 => false,
            _ => return Err(Error::CorruptTree),
        };
        let file_root = BlockPointer::decode_optional(&bytes[16..40])?;
        if directory && file_root.is_some() {
            return Err(Error::CorruptTree);
        }
        if let Some(root) = file_root {
            root.validate(total, generation, ROOT_PAYLOAD_LAST + 1)?;
            if root.level != 0 {
                // The first integrated format uses one extent leaf per file.
                return Err(Error::CorruptTree);
            }
        }
        Ok(Self {
            inode,
            directory,
            file_root,
        })
    }
}

/// Same portable path restrictions as the existing v9 transactional API.
/// The F4 internal catalog pathname is never a user-visible v10 record.
pub fn validate_namespace_path(path: &str) -> Result<()> {
    if !path.starts_with('/')
        || path == "/"
        || path.len() > MAX_PATH_BYTES
        || path.ends_with('/')
        || path.contains('\0')
        || path.contains('\\')
        || path == "/.bfs_f4_attrs"
        || path
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(Error::InvalidPath);
    }
    Ok(())
}

pub fn validate_namespace(
    records: &BTreeMap<String, NamespaceRecord>,
    next_inode: u64,
    generation: u64,
    total_blocks: u64,
) -> Result<()> {
    if records.len() > MAX_NAMESPACE_RECORDS || next_inode < 2 || generation == 0 {
        return Err(Error::CorruptTree);
    }
    let mut ids = BTreeSet::new();
    for (path, record) in records {
        validate_namespace_path(path).map_err(|_| Error::CorruptTree)?;
        if record.inode >= next_inode || !ids.insert(record.inode) {
            // Hard links are deliberately excluded in the first v10 profile.
            return Err(Error::CorruptTree);
        }
        NamespaceRecord::decode(&record.encode(), generation, total_blocks)?;
        let slash = path.rfind('/').ok_or(Error::CorruptTree)?;
        if slash > 0 {
            let parent = records.get(&path[..slash]).ok_or(Error::CorruptTree)?;
            if !parent.directory {
                return Err(Error::CorruptTree);
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceScan {
    pub records: BTreeMap<String, NamespaceRecord>,
    /// Only the namespace B+tree, not the referenced per-file extent leaves.
    pub metadata_pages: BTreeSet<u64>,
}
struct Walker<'a, D: BlockDevice> {
    device: &'a mut D,
    volume_id: u128,
    generation: u64,
    pages: BTreeSet<u64>,
    records: BTreeMap<String, NamespaceRecord>,
}
impl<D: BlockDevice> Walker<'_, D> {
    fn visit(&mut self, ptr: BlockPointer, is_root: bool) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        if self.pages.len() >= MAX_NAMESPACE_PAGES || !self.pages.insert(ptr.address) {
            return Err(Error::CorruptTree);
        }
        let page = read_page(
            self.device,
            ptr,
            TreeId::Fs,
            self.volume_id,
            self.generation,
        )?;
        if page.items.is_empty() {
            return if is_root && page.level == 0 {
                Ok(None)
            } else {
                Err(Error::CorruptTree)
            };
        }
        if page.level == 0 {
            for item in &page.items {
                let path = std::str::from_utf8(&item.key).map_err(|_| Error::CorruptTree)?;
                validate_namespace_path(path).map_err(|_| Error::CorruptTree)?;
                let record = NamespaceRecord::decode(
                    &item.value,
                    self.generation,
                    self.device.block_count(),
                )?;
                if self.records.len() >= MAX_NAMESPACE_RECORDS
                    || self.records.insert(path.to_owned(), record).is_some()
                {
                    return Err(Error::CorruptTree);
                }
            }
            return Ok(Some((
                page.items[0].key.clone(),
                page.items.last().ok_or(Error::CorruptTree)?.key.clone(),
            )));
        }
        let mut first = None;
        let mut prior_max: Option<Vec<u8>> = None;
        for item in &page.items {
            let child = BlockPointer::decode_required(&item.value)?;
            let (min, max) = self.visit(child, false)?.ok_or(Error::CorruptTree)?;
            if max != item.key || prior_max.as_ref().is_some_and(|last| last >= &min) {
                return Err(Error::CorruptTree);
            }
            if first.is_none() {
                first = Some(min);
            }
            prior_max = Some(max);
        }
        Ok(Some((
            first.ok_or(Error::CorruptTree)?,
            prior_max.ok_or(Error::CorruptTree)?,
        )))
    }
}
pub fn read_namespace_tree<D: BlockDevice>(
    device: &mut D,
    root: BlockPointer,
    volume_id: u128,
    generation: u64,
    next_inode: u64,
) -> Result<NamespaceScan> {
    if volume_id == 0 || generation == 0 {
        return Err(Error::CorruptTree);
    }
    let mut walker = Walker {
        device,
        volume_id,
        generation,
        pages: BTreeSet::new(),
        records: BTreeMap::new(),
    };
    walker.visit(root, true)?;
    validate_namespace(
        &walker.records,
        next_inode,
        generation,
        walker.device.block_count(),
    )?;
    Ok(NamespaceScan {
        records: walker.records,
        metadata_pages: walker.pages,
    })
}

pub struct StagedNamespace {
    pub root: BlockPointer,
    pub pages: Vec<(u64, [u8; BLOCK_SIZE])>,
    pub used: usize,
}
struct Child {
    max_key: Vec<u8>,
    pointer: BlockPointer,
}
struct Builder<'a> {
    volume_id: u128,
    generation: u64,
    reservations: &'a [u64],
    pages: Vec<(u64, [u8; BLOCK_SIZE])>,
}
impl Builder<'_> {
    fn write(&mut self, level: u8, items: Vec<Item>) -> Result<Child> {
        let address = *self
            .reservations
            .get(self.pages.len())
            .ok_or(Error::NoSpace)?;
        let page = Page {
            tree: TreeId::Fs,
            level,
            birth: self.generation,
            address,
            volume_id: self.volume_id,
            items,
        };
        let bytes = page.encode()?;
        let pointer = page.pointer()?;
        let max_key = page
            .items
            .last()
            .map_or(Vec::new(), |item| item.key.clone());
        self.pages.push((address, bytes));
        Ok(Child { max_key, pointer })
    }
    fn pack(&mut self, level: u8, items: Vec<Item>) -> Result<Vec<Child>> {
        if items.is_empty() {
            return Ok(vec![self.write(level, Vec::new())?]);
        }
        let mut out = Vec::new();
        let mut group = Vec::new();
        let mut size = PAGE_HEADER_BYTES;
        for item in items {
            let cost = 8 + item.key.len() + item.value.len();
            if PAGE_HEADER_BYTES + cost > BLOCK_SIZE {
                return Err(Error::TooLarge);
            }
            if size + cost > BLOCK_SIZE {
                out.push(self.write(level, std::mem::take(&mut group))?);
                size = PAGE_HEADER_BYTES;
            }
            size += cost;
            group.push(item);
        }
        if !group.is_empty() {
            out.push(self.write(level, group)?);
        }
        Ok(out)
    }
}

/// Pure, non-publishing bulk CoW builder. Safe allocation, fallback roots,
/// and data/checksum ownership are responsibilities of the caller.
// Wire geometry, generation and the internal reuse gate are explicit here.
#[allow(clippy::too_many_arguments)]
fn stage_namespace_tree_with_mode(
    records: &BTreeMap<String, NamespaceRecord>,
    reservations: &[u64],
    volume_id: u128,
    generation: u64,
    next_inode: u64,
    old_high_water: u64,
    total_blocks: u64,
    audited_reuse: bool,
) -> Result<StagedNamespace> {
    validate_namespace(records, next_inode, generation, total_blocks)?;
    if volume_id == 0
        || old_high_water > total_blocks
        || old_high_water <= ROOT_PAYLOAD_LAST
        || reservations.iter().any(|&block| {
            block <= ROOT_PAYLOAD_LAST
                || block >= total_blocks
                || (!audited_reuse && block >= old_high_water)
        })
        || reservations.windows(2).any(|pair| pair[0] <= pair[1])
    {
        return Err(Error::Geometry);
    }
    let mut builder = Builder {
        volume_id,
        generation,
        reservations,
        pages: Vec::new(),
    };
    let items = records
        .iter()
        .map(|(path, entry)| Item {
            key: path.as_bytes().to_vec(),
            value: entry.encode().to_vec(),
        })
        .collect();
    let mut children = builder.pack(0, items)?;
    let mut level = 0;
    while children.len() > 1 {
        if level >= MAX_LEVEL {
            return Err(Error::TooLarge);
        }
        level += 1;
        let items = children
            .into_iter()
            .map(|child| Item {
                key: child.max_key,
                value: child.pointer.encode().to_vec(),
            })
            .collect();
        children = builder.pack(level, items)?;
    }
    Ok(StagedNamespace {
        root: children.pop().ok_or(Error::CorruptTree)?.pointer,
        used: builder.pages.len(),
        pages: builder.pages,
    })
}

/// Virgin-only public planner. Does not itself authorize physical reuse.
pub fn stage_namespace_tree(
    records: &BTreeMap<String, NamespaceRecord>,
    reservations: &[u64],
    volume_id: u128,
    generation: u64,
    next_inode: u64,
    old_high_water: u64,
    total_blocks: u64,
) -> Result<StagedNamespace> {
    stage_namespace_tree_with_mode(
        records,
        reservations,
        volume_id,
        generation,
        next_inode,
        old_high_water,
        total_blocks,
        false,
    )
}

/// Internal audited-reuse staging. Only the full two-F0-graph verifier
/// may supply these addresses, and the caller must independently prove
/// each is absent from *all* recoverable current/fallback owners.
pub(super) fn stage_namespace_tree_reusing(
    records: &BTreeMap<String, NamespaceRecord>,
    reservations: &[u64],
    volume_id: u128,
    generation: u64,
    next_inode: u64,
    old_high_water: u64,
    total_blocks: u64,
) -> Result<StagedNamespace> {
    stage_namespace_tree_with_mode(
        records,
        reservations,
        volume_id,
        generation,
        next_inode,
        old_high_water,
        total_blocks,
        true,
    )
}
