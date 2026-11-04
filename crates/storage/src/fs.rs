//! F1: bounded, append-only CoW namespace with checksummed metadata/data.
//! This intentionally rewrites the *entire* sorted tree per transaction.
use crate::volume::{RootInfo, crc32c};
use crate::{BLOCK_SIZE, BlockDevice, Error, Result, Volume};
use std::collections::{BTreeMap, BTreeSet};

const META_MAGIC: &[u8; 8] = b"BFSNGM2\0";
const NODE_MAGIC: &[u8; 8] = b"BFSNGN1\0";
const DATA_MAGIC: &[u8; 8] = b"BFSNGD1\0";
const FORMAT: u32 = 2;
const MAX_SNAPSHOTS: usize = 16;
const FANOUT: usize = 12;
const MAX_PATH: usize = 240;
const MAX_RECORDS: usize = 2048;
const MAX_DATA: usize = 256 * 1024;
const CHUNK: usize = BLOCK_SIZE - 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub inode: u64,
    pub directory: bool,
    pub bytes: u64,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    inode: u64,
    directory: bool,
    start: u64,
    count: u32,
    length: u64,
    checksum: u32,
    /// Generation that first wrote this data extent; zero for no extent.
    birth_generation: u64,
}

impl Record {
    fn directory(inode: u64) -> Self {
        Self {
            inode,
            directory: true,
            start: 0,
            count: 0,
            length: 0,
            checksum: 0,
            birth_generation: 0,
        }
    }

    fn file(inode: u64) -> Self {
        Self {
            directory: false,
            ..Self::directory(inode)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnapshotRoot {
    generation: u64,
    tree_root: u64,
    cursor: u64,
    next_inode: u64,
    record_count: u64,
}

#[derive(Clone)]
struct State {
    cursor: u64,
    next_inode: u64,
    tree_root: u64,
    snapshots: BTreeMap<u64, SnapshotRoot>,
    records: BTreeMap<String, Record>,
}

impl State {
    fn empty(blocks: u64) -> Self {
        Self {
            cursor: blocks,
            next_inode: 2,
            tree_root: 0,
            snapshots: BTreeMap::new(),
            records: BTreeMap::new(),
        }
    }
}

/// Mutates a private namespace copy. All changes publish atomically only when
/// FileSystem::commit succeeds. No concurrent image handles or OS file locks.
pub struct FsTransaction {
    volume_id: u128,
    base_generation: u64,
    next_inode: u64,
    records: BTreeMap<String, Record>,
    new_data: BTreeMap<String, Vec<u8>>,
    snapshots: BTreeMap<u64, SnapshotRoot>,
    current_root: SnapshotRoot,
    dirty: bool,
}

impl FsTransaction {
    /// Keep the committed generation on which this transaction began.
    /// A snapshot is an immutable tree root, not a copied set of file bytes.
    pub fn pin_snapshot(&mut self) -> Result<u64> {
        if self.current_root.generation == 0 {
            return Err(Error::InvalidSnapshot);
        }
        let generation = self.current_root.generation;
        if !self.snapshots.contains_key(&generation) && self.snapshots.len() >= MAX_SNAPSHOTS {
            return Err(Error::NoSpace);
        }
        self.snapshots.insert(generation, self.current_root);
        self.dirty = true;
        Ok(generation)
    }

    /// Dropping a pin only makes its blocks *eligible* for reclamation after
    /// the pin removal commits and all fallback roots stop referring to it.
    pub fn drop_snapshot(&mut self, generation: u64) -> Result<()> {
        if self.snapshots.remove(&generation).is_none() {
            return Err(Error::InvalidSnapshot);
        }
        self.dirty = true;
        Ok(())
    }

    fn parent(&self, path: &str) -> Result<()> {
        validate_path(path)?;
        let slash = path.rfind('/').ok_or(Error::InvalidPath)?;
        if slash == 0 {
            return Ok(());
        }
        match self.records.get(&path[..slash]) {
            Some(r) if r.directory => Ok(()),
            Some(_) => Err(Error::NotDirectory),
            None => Err(Error::NotFound),
        }
    }

    fn new_inode(&mut self) -> Result<u64> {
        let id = self.next_inode;
        self.next_inode = id.checked_add(1).ok_or(Error::GenerationOverflow)?;
        Ok(id)
    }

    pub fn mkdir(&mut self, path: &str) -> Result<u64> {
        self.parent(path)?;
        if self.records.contains_key(path) {
            return Err(Error::AlreadyExists);
        }
        if self.records.len() >= MAX_RECORDS {
            return Err(Error::NoSpace);
        }
        let inode = self.new_inode()?;
        self.records.insert(path.into(), Record::directory(inode));
        self.dirty = true;
        Ok(inode)
    }

    pub fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<u64> {
        self.parent(path)?;
        if bytes.len() > MAX_DATA {
            return Err(Error::TooLarge);
        }
        let inode = match self.records.get(path) {
            Some(record) if record.directory => return Err(Error::IsDirectory),
            Some(record) => record.inode,
            None => {
                if self.records.len() >= MAX_RECORDS {
                    return Err(Error::NoSpace);
                }
                self.new_inode()?
            }
        };
        self.records.insert(path.into(), Record::file(inode));
        self.new_data.insert(path.into(), bytes.to_vec());
        self.dirty = true;
        Ok(inode)
    }

    pub fn remove_file(&mut self, path: &str) -> Result<()> {
        validate_path(path)?;
        match self.records.get(path) {
            None => return Err(Error::NotFound),
            Some(record) if record.directory => return Err(Error::IsDirectory),
            _ => {}
        }
        self.records.remove(path);
        self.new_data.remove(path);
        self.dirty = true;
        Ok(())
    }

    pub fn rmdir(&mut self, path: &str) -> Result<()> {
        validate_path(path)?;
        match self.records.get(path) {
            None => return Err(Error::NotFound),
            Some(record) if !record.directory => return Err(Error::NotDirectory),
            _ => {}
        }
        let prefix = format!("{path}/");
        if self.records.keys().any(|key| key.starts_with(&prefix)) {
            return Err(Error::NotEmpty);
        }
        self.records.remove(path);
        self.dirty = true;
        Ok(())
    }
}

fn validate_path(path: &str) -> Result<()> {
    if !path.starts_with('/')
        || path == "/"
        || path.len() > MAX_PATH
        || path.ends_with('/')
        || path.contains('\0')
        || path.contains('\\')
        || path
            .split('/')
            .skip(1)
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(Error::InvalidPath);
    }
    Ok(())
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().expect("fixed length"))
}
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("fixed length"))
}
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("fixed length"))
}

/// Root payload format v2. The root slot CRC covers the complete manifest.
/// Snapshot pins are included in the same commit as the live namespace.
fn root_manifest(state: &State, generation: u64) -> Vec<u8> {
    let mut b = vec![0_u8; 64 + state.snapshots.len() * 40];
    b[..8].copy_from_slice(META_MAGIC);
    b[8..12].copy_from_slice(&FORMAT.to_le_bytes());
    b[16..24].copy_from_slice(&state.cursor.to_le_bytes());
    b[24..32].copy_from_slice(&state.tree_root.to_le_bytes());
    b[32..40].copy_from_slice(&state.next_inode.to_le_bytes());
    b[40..48].copy_from_slice(&(state.records.len() as u64).to_le_bytes());
    b[48..56].copy_from_slice(&generation.to_le_bytes());
    b[56..60].copy_from_slice(&(state.snapshots.len() as u32).to_le_bytes());
    for (i, snap) in state.snapshots.values().enumerate() {
        let at = 64 + i * 40;
        b[at..at + 8].copy_from_slice(&snap.generation.to_le_bytes());
        b[at + 8..at + 16].copy_from_slice(&snap.tree_root.to_le_bytes());
        b[at + 16..at + 24].copy_from_slice(&snap.cursor.to_le_bytes());
        b[at + 24..at + 32].copy_from_slice(&snap.next_inode.to_le_bytes());
        b[at + 32..at + 40].copy_from_slice(&snap.record_count.to_le_bytes());
    }
    b
}

/// Allocates only blocks absent from every recoverable root and pinned tree.
/// Reusable addresses are drawn first; otherwise the high cursor descends.
struct Builder {
    cursor: u64,
    root_limit: u64,
    reusable: BTreeSet<u64>,
    staged: Vec<(u64, [u8; BLOCK_SIZE])>,
}

impl Builder {
    fn new(cursor: u64, next_root: u64, reusable: BTreeSet<u64>) -> Self {
        Self {
            cursor,
            root_limit: next_root,
            reusable,
            staged: Vec::new(),
        }
    }

    fn reserve(&mut self, amount: u64) -> Result<u64> {
        if amount == 0 {
            return Err(Error::Geometry);
        }
        // A file's extent is contiguous. Find a complete run in the
        // recovered free set; never merge unrelated holes into an extent.
        let mut run_start = None;
        let mut last: u64 = 0;
        let mut run_len = 0;
        for &address in &self.reusable {
            if address <= self.root_limit {
                continue;
            }
            if run_start.is_some() && last.checked_add(1) == Some(address) {
                run_len += 1;
            } else {
                run_start = Some(address);
                run_len = 1;
            }
            last = address;
            if run_len == amount {
                let start = run_start.expect("nonempty run");
                for id in start..start + amount {
                    self.reusable.remove(&id);
                }
                return Ok(start);
            }
        }
        let start = self.cursor.checked_sub(amount).ok_or(Error::NoSpace)?;
        if start <= self.root_limit {
            return Err(Error::NoSpace);
        }
        self.cursor = start;
        Ok(start)
    }

    fn push(&mut self, block: [u8; BLOCK_SIZE]) -> Result<u64> {
        let addr = self.reserve(1)?;
        self.staged.push((addr, block));
        Ok(addr)
    }
}

fn seal(mut block: [u8; BLOCK_SIZE]) -> [u8; BLOCK_SIZE] {
    block[24..28].fill(0);
    let crc = crc32c(&block);
    block[24..28].copy_from_slice(&crc.to_le_bytes());
    block
}

fn valid_checksum(block: &[u8; BLOCK_SIZE]) -> bool {
    let stored = u32_at(block, 24);
    let mut bytes = *block;
    bytes[24..28].fill(0);
    crc32c(&bytes) == stored
}

fn encode_data(inode: u64, index: u32, bytes: &[u8], generation: u32) -> [u8; BLOCK_SIZE] {
    let mut b = [0; BLOCK_SIZE];
    b[..8].copy_from_slice(DATA_MAGIC);
    b[8..16].copy_from_slice(&inode.to_le_bytes());
    b[16..20].copy_from_slice(&index.to_le_bytes());
    b[20..24].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
    b[28..32].copy_from_slice(&generation.to_le_bytes());
    b[32..32 + bytes.len()].copy_from_slice(bytes);
    seal(b)
}

fn data_read<D: BlockDevice>(
    device: &mut D,
    record: &Record,
    cursor: u64,
    generation: u64,
) -> Result<Option<Vec<u8>>> {
    if record.directory {
        return Ok((record.start == 0
            && record.count == 0
            && record.length == 0
            && record.checksum == 0)
            .then(Vec::new));
    }
    if record.length == 0 {
        return Ok((record.start == 0
            && record.count == 0
            && record.checksum == crc32c(&[])
            && record.birth_generation == 0)
            .then(Vec::new));
    }
    let len = usize::try_from(record.length).map_err(|_| Error::CorruptTree)?;
    if len > MAX_DATA
        || record.count as usize != len.div_ceil(CHUNK)
        || record.birth_generation == 0
        || record.birth_generation > generation
        || record.birth_generation > u32::MAX as u64
    {
        return Ok(None);
    }
    let end = record.start.checked_add(record.count as u64);
    if record.start < cursor
        || record.start <= generation + 1
        || end.is_none_or(|n| n > device.block_count())
    {
        return Ok(None);
    }
    let mut result = Vec::with_capacity(len);
    for i in 0..record.count {
        let mut b = [0; BLOCK_SIZE];
        device.read_block(record.start + u64::from(i), &mut b)?;
        let chunk_len = usize::min(CHUNK, len - result.len());
        if &b[..8] != DATA_MAGIC
            || u64_at(&b, 8) != record.inode
            || u32_at(&b, 16) != i
            || u32_at(&b, 20) as usize != chunk_len
            || !valid_checksum(&b)
            || u32_at(&b, 28) as u64 != record.birth_generation
            || b[32 + chunk_len..].iter().any(|n| *n != 0)
        {
            return Ok(None);
        }
        result.extend_from_slice(&b[32..32 + chunk_len]);
    }
    Ok((crc32c(&result) == record.checksum).then_some(result))
}

fn encode_node_header(level: u8, count: usize, generation: u64) -> [u8; BLOCK_SIZE] {
    let mut b = [0; BLOCK_SIZE];
    b[..8].copy_from_slice(NODE_MAGIC);
    b[8] = level;
    b[10..12].copy_from_slice(&(count as u16).to_le_bytes());
    b[12..20].copy_from_slice(&generation.to_le_bytes());
    b
}

fn put_key(b: &mut [u8; BLOCK_SIZE], at: &mut usize, key: &str) -> Result<()> {
    let bytes = key.as_bytes();
    if *at + 2 + bytes.len() > BLOCK_SIZE {
        return Err(Error::TooLarge);
    }
    b[*at..*at + 2].copy_from_slice(&(bytes.len() as u16).to_le_bytes());
    *at += 2;
    b[*at..*at + bytes.len()].copy_from_slice(bytes);
    *at += bytes.len();
    Ok(())
}

fn encode_leaf(entries: &[(&String, &Record)], generation: u64) -> Result<[u8; BLOCK_SIZE]> {
    let mut b = encode_node_header(0, entries.len(), generation);
    let mut at = 32;
    for (path, record) in entries {
        put_key(&mut b, &mut at, path)?;
        const LEN: usize = 41;
        if at + LEN > BLOCK_SIZE {
            return Err(Error::TooLarge);
        }
        b[at..at + 8].copy_from_slice(&record.inode.to_le_bytes());
        b[at + 8] = u8::from(record.directory);
        b[at + 9..at + 17].copy_from_slice(&record.start.to_le_bytes());
        b[at + 17..at + 21].copy_from_slice(&record.count.to_le_bytes());
        b[at + 21..at + 29].copy_from_slice(&record.length.to_le_bytes());
        b[at + 29..at + 33].copy_from_slice(&record.checksum.to_le_bytes());
        b[at + 33..at + 41].copy_from_slice(&record.birth_generation.to_le_bytes());
        at += LEN;
    }
    Ok(seal(b))
}

fn encode_branch(
    children: &[(String, u64)],
    level: u8,
    generation: u64,
) -> Result<[u8; BLOCK_SIZE]> {
    let mut b = encode_node_header(level, children.len(), generation);
    let mut at = 32;
    for (key, block) in children {
        put_key(&mut b, &mut at, key)?;
        if at + 8 > BLOCK_SIZE {
            return Err(Error::TooLarge);
        }
        b[at..at + 8].copy_from_slice(&block.to_le_bytes());
        at += 8;
    }
    Ok(seal(b))
}

fn build_nodes(
    builder: &mut Builder,
    records: &BTreeMap<String, Record>,
    generation: u64,
) -> Result<u64> {
    if records.is_empty() {
        return Ok(0);
    }
    let items: Vec<_> = records.iter().collect();
    let mut children: Vec<(String, u64)> = Vec::new();
    for part in items.chunks(FANOUT) {
        let node = encode_leaf(part, generation)?;
        let addr = builder.push(node)?;
        children.push((part.last().expect("nonempty group").0.clone(), addr));
    }
    let mut level = 1u8;
    while children.len() > 1 {
        let mut upper = Vec::new();
        for part in children.chunks(FANOUT) {
            let node = encode_branch(part, level, generation)?;
            let addr = builder.push(node)?;
            upper.push((part.last().expect("nonempty group").0.clone(), addr));
        }
        children = upper;
        level = level.checked_add(1).ok_or(Error::TooLarge)?;
    }
    Ok(children[0].1)
}

struct Reader<'a> {
    block: &'a [u8; BLOCK_SIZE],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.offset.checked_add(len)?;
        if end > BLOCK_SIZE {
            return None;
        }
        let result = &self.block[self.offset..end];
        self.offset = end;
        Some(result)
    }

    fn key(&mut self) -> Option<String> {
        let raw = self.take(2)?;
        let len = u16::from_le_bytes(raw.try_into().ok()?) as usize;
        if len == 0 || len > MAX_PATH {
            return None;
        }
        let raw = self.take(len)?;
        let value = std::str::from_utf8(raw).ok()?.to_owned();
        validate_path(&value).ok()?;
        Some(value)
    }

    fn done(&self) -> bool {
        self.block[self.offset..].iter().all(|b| *b == 0)
    }
}

fn read_nodes<D: BlockDevice>(
    device: &mut D,
    address: u64,
    cursor: u64,
    generation: u64,
    level: Option<u8>,
    visited: &mut BTreeSet<u64>,
    out: &mut BTreeMap<String, Record>,
) -> Result<Option<(String, u8)>> {
    if address < cursor
        || address <= generation + 1
        || address >= device.block_count()
        || !visited.insert(address)
        || visited.len() > 2048
    {
        return Ok(None);
    }
    let mut b = [0; BLOCK_SIZE];
    device.read_block(address, &mut b)?;
    let kind = b[8];
    let count = u16_at(&b, 10) as usize;
    let birth = u64_at(&b, 12);
    if &b[..8] != NODE_MAGIC
        || b[9] != 0
        || count == 0
        || count > FANOUT
        || birth == 0
        || birth > generation
        || !valid_checksum(&b)
        || b[20..24].iter().any(|n| *n != 0)
        || b[28..32].iter().any(|n| *n != 0)
        || level.is_some_and(|expected| expected != kind)
        || kind > 8
    {
        return Ok(None);
    }
    let mut rd = Reader {
        block: &b,
        offset: 32,
    };
    let mut last_key = String::new();
    for _ in 0..count {
        let Some(key) = rd.key() else {
            return Ok(None);
        };
        if key <= last_key {
            return Ok(None);
        }
        if kind == 0 {
            // Depth-first traversal must produce a globally increasing path
            // sequence; child maximum keys alone are not sufficient.
            if out
                .keys()
                .next_back()
                .is_some_and(|previous| key <= *previous)
            {
                return Ok(None);
            }
            let Some(raw) = rd.take(41) else {
                return Ok(None);
            };
            let inode = u64_at(raw, 0);
            let mode = raw[8];
            if inode < 2 || mode > 1 {
                return Ok(None);
            }
            let record = Record {
                inode,
                directory: mode == 1,
                start: u64_at(raw, 9),
                count: u32_at(raw, 17),
                length: u64_at(raw, 21),
                checksum: u32_at(raw, 29),
                birth_generation: u64_at(raw, 33),
            };
            if out.insert(key.clone(), record).is_some() || out.len() > MAX_RECORDS {
                return Ok(None);
            }
        } else {
            let Some(raw) = rd.take(8) else {
                return Ok(None);
            };
            let child_addr = u64_at(raw, 0);
            let Some((max_key, child_level)) = read_nodes(
                device,
                child_addr,
                cursor,
                generation,
                Some(kind - 1),
                visited,
                out,
            )?
            else {
                return Ok(None);
            };
            if child_level != kind - 1 || max_key != key {
                return Ok(None);
            }
        }
        last_key = key;
    }
    if !rd.done() {
        return Ok(None);
    }
    Ok(Some((last_key, kind)))
}

/// Verifies a complete immutable tree and returns its exact data/node reachability.
/// A snapshot uses its original generation and cursor, not the live root's.
type TreeContents = (BTreeMap<String, Record>, BTreeSet<u64>);

fn load_tree<D: BlockDevice>(device: &mut D, root: SnapshotRoot) -> Result<Option<TreeContents>> {
    if root.generation == 0
        || root.cursor <= root.generation.saturating_add(1)
        || root.cursor > device.block_count()
        || root.next_inode < 2
        || root.record_count > MAX_RECORDS as u64
        || (root.tree_root == 0) != (root.record_count == 0)
    {
        return Ok(None);
    }
    let mut records = BTreeMap::new();
    let mut owned = BTreeSet::new();
    if root.tree_root != 0
        && read_nodes(
            device,
            root.tree_root,
            root.cursor,
            root.generation,
            None,
            &mut owned,
            &mut records,
        )?
        .is_none()
    {
        return Ok(None);
    }
    if records.len() != root.record_count as usize {
        return Ok(None);
    }
    let mut inodes = BTreeSet::new();
    for (path, record) in &records {
        if record.inode >= root.next_inode || !inodes.insert(record.inode) {
            return Ok(None);
        }
        let slash = path.rfind('/').expect("validated absolute path");
        if slash > 0 && !records.get(&path[..slash]).is_some_and(|r| r.directory) {
            return Ok(None);
        }
        if record.directory {
            if record.start != 0
                || record.count != 0
                || record.length != 0
                || record.checksum != 0
                || record.birth_generation != 0
            {
                return Ok(None);
            }
        } else {
            if record.length > MAX_DATA as u64 || record.count as usize > MAX_DATA.div_ceil(CHUNK) {
                return Ok(None);
            }
            if record.count > 0 {
                let Some(end) = record.start.checked_add(record.count as u64) else {
                    return Ok(None);
                };
                if record.start < root.cursor
                    || record.start <= root.generation + 1
                    || end > device.block_count()
                {
                    return Ok(None);
                }
                for pos in record.start..end {
                    if !owned.insert(pos) {
                        return Ok(None);
                    }
                }
            }
            if data_read(device, record, root.cursor, root.generation)?.is_none() {
                return Ok(None);
            }
        }
    }
    Ok(Some((records, owned)))
}

fn load_state<D: BlockDevice>(
    device: &mut D,
    info: RootInfo,
    payload: Option<&[u8]>,
) -> Result<Option<State>> {
    let Some(bytes) = payload else {
        return Ok((info.generation == 0).then(|| State::empty(device.block_count())));
    };
    if bytes.len() < 64
        || &bytes[..8] != META_MAGIC
        || u32_at(bytes, 8) != FORMAT
        || bytes[12..16].iter().any(|b| *b != 0)
        || u64_at(bytes, 48) != info.generation
        || bytes[60..64].iter().any(|b| *b != 0)
    {
        return Ok(None);
    }
    let snap_count = u32_at(bytes, 56) as usize;
    if snap_count > MAX_SNAPSHOTS || bytes.len() != 64 + snap_count * 40 {
        return Ok(None);
    }
    let root = SnapshotRoot {
        generation: info.generation,
        cursor: u64_at(bytes, 16),
        tree_root: u64_at(bytes, 24),
        next_inode: u64_at(bytes, 32),
        record_count: u64_at(bytes, 40),
    };
    if root.generation == 0 {
        return Ok(None);
    }
    let Some((records, _)) = load_tree(device, root)? else {
        return Ok(None);
    };
    let mut snapshots = BTreeMap::new();
    let mut last_generation = 0;
    for i in 0..snap_count {
        let at = 64 + i * 40;
        let snap = SnapshotRoot {
            generation: u64_at(bytes, at),
            tree_root: u64_at(bytes, at + 8),
            cursor: u64_at(bytes, at + 16),
            next_inode: u64_at(bytes, at + 24),
            record_count: u64_at(bytes, at + 32),
        };
        if snap.generation <= last_generation
            || snap.generation >= root.generation
            || snap.cursor < root.cursor
            || snap.next_inode > root.next_inode
            || load_tree(device, snap)?.is_none()
        {
            return Ok(None);
        }
        last_generation = snap.generation;
        snapshots.insert(snap.generation, snap);
    }
    Ok(Some(State {
        cursor: root.cursor,
        next_inode: root.next_inode,
        tree_root: root.tree_root,
        snapshots,
        records,
    }))
}

/// F2 image namespace with pinned historical roots and reachability-checked
/// allocation reuse. Still a single-writer disposable test-image prototype.
pub struct FileSystem<D: BlockDevice> {
    volume: Volume<D>,
    state: State,
}

impl<D: BlockDevice> FileSystem<D> {
    pub fn format(device: D, volume_id: u128) -> Result<Self> {
        let volume = Volume::format(device, volume_id)?;
        let state = State::empty(volume.block_count());
        Ok(Self { volume, state })
    }

    pub fn open(device: D) -> Result<Self> {
        let (volume, state) = Volume::open_verified(device, load_state)?;
        Ok(Self { volume, state })
    }

    pub fn generation(&self) -> u64 {
        self.volume.info().generation
    }

    pub fn free_cursor(&self) -> u64 {
        self.state.cursor
    }

    pub fn file_count(&self) -> usize {
        self.state.records.len()
    }

    /// Persistent, read-only snapshot generation IDs.
    pub fn snapshots(&self) -> Vec<u64> {
        self.state.snapshots.keys().copied().collect()
    }

    /// Read a pinned historical file and recheck its recorded content checksum.
    pub fn read_snapshot_file(&mut self, generation: u64, path: &str) -> Result<Vec<u8>> {
        validate_path(path)?;
        let snap = *self
            .state
            .snapshots
            .get(&generation)
            .ok_or(Error::InvalidSnapshot)?;
        self.volume.inspect(|device| {
            let (records, _) = load_tree(device, snap)?.ok_or(Error::CorruptTree)?;
            let record = records.get(path).ok_or(Error::NotFound)?;
            if record.directory {
                return Err(Error::IsDirectory);
            }
            data_read(device, record, snap.cursor, snap.generation)?.ok_or(Error::CorruptTree)
        })
    }

    /// Reconstruct the free set from independently validated F0 root slots
    /// and every retained snapshot in each root's catalog.
    ///
    /// If even one checksummed candidate has damaged F1 metadata, fail
    /// *closed* for reclamation: use fresh blocks only on this commit.
    fn reusable_blocks(&mut self) -> Result<BTreeSet<u64>> {
        let candidates = self.volume.candidate_roots()?;
        let mut protected = BTreeSet::new();
        for (info, payload) in candidates {
            let Some(state) = self
                .volume
                .inspect(|device| load_state(device, info, payload.as_deref()))?
            else {
                return Ok(BTreeSet::new());
            };
            if info.generation == 0 {
                continue;
            }
            let root = SnapshotRoot {
                generation: info.generation,
                cursor: state.cursor,
                tree_root: state.tree_root,
                next_inode: state.next_inode,
                record_count: state.records.len() as u64,
            };
            for held in std::iter::once(root).chain(state.snapshots.values().copied()) {
                let Some((_, owned)) = self.volume.inspect(|device| load_tree(device, held))?
                else {
                    return Ok(BTreeSet::new());
                };
                protected.extend(owned);
            }
        }
        Ok((self.state.cursor..self.volume.block_count())
            .filter(|n| !protected.contains(n))
            .collect())
    }

    pub fn stat(&self, path: &str) -> Result<Stat> {
        if path == "/" {
            return Ok(Stat {
                inode: 1,
                directory: true,
                bytes: 0,
                generation: self.generation(),
            });
        }
        validate_path(path)?;
        let record = self.state.records.get(path).ok_or(Error::NotFound)?;
        Ok(Stat {
            inode: record.inode,
            directory: record.directory,
            bytes: record.length,
            generation: self.generation(),
        })
    }

    pub fn list_dir(&self, path: &str) -> Result<Vec<String>> {
        if path != "/" {
            validate_path(path)?;
            if !self.state.records.get(path).is_some_and(|r| r.directory) {
                return Err(Error::NotDirectory);
            }
        }
        let prefix = if path == "/" {
            "/".to_owned()
        } else {
            format!("{path}/")
        };
        Ok(self
            .state
            .records
            .keys()
            .filter(|entry| entry.starts_with(&prefix))
            .filter(|entry| !entry[prefix.len()..].contains('/'))
            .cloned()
            .collect())
    }

    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>> {
        validate_path(path)?;
        let entry = self.state.records.get(path).ok_or(Error::NotFound)?.clone();
        if entry.directory {
            return Err(Error::IsDirectory);
        }
        // Validation runs at mount too; repeat to catch subsequent media damage.
        let cursor = self.state.cursor;
        let generation = self.generation();
        let mut reader = ReaderDevice {
            volume: &mut self.volume,
        };
        data_read(&mut reader, &entry, cursor, generation)?.ok_or(Error::CorruptTree)
    }

    pub fn begin(&self) -> FsTransaction {
        FsTransaction {
            volume_id: self.volume.info().volume_id,
            base_generation: self.generation(),
            next_inode: self.state.next_inode,
            records: self.state.records.clone(),
            new_data: BTreeMap::new(),
            snapshots: self.state.snapshots.clone(),
            current_root: SnapshotRoot {
                generation: self.generation(),
                tree_root: self.state.tree_root,
                cursor: self.state.cursor,
                next_inode: self.state.next_inode,
                record_count: self.state.records.len() as u64,
            },
            dirty: false,
        }
    }

    pub fn commit(&mut self, tx: FsTransaction) -> Result<u64> {
        if tx.volume_id != self.volume.info().volume_id {
            return Err(Error::InconsistentVolume);
        }
        if tx.base_generation != self.generation() {
            return Err(Error::StaleTransaction);
        }
        if !tx.dirty {
            return Ok(self.generation());
        }
        let generation = self
            .generation()
            .checked_add(1)
            .ok_or(Error::GenerationOverflow)?;
        let root_block = generation.checked_add(1).ok_or(Error::GenerationOverflow)?;
        if generation > u32::MAX as u64 {
            return Err(Error::GenerationOverflow);
        }
        // Even when the reusable pool is nonempty, advancing F0's low-end
        // payload address must never enter the high-watermark F1 region.
        if root_block >= self.state.cursor {
            return Err(Error::NoSpace);
        }
        let reusable = self.reusable_blocks()?;
        let mut builder = Builder::new(self.state.cursor, root_block, reusable);
        let mut state = State {
            cursor: self.state.cursor,
            next_inode: tx.next_inode,
            tree_root: 0,
            snapshots: tx.snapshots,
            records: tx.records,
        };
        for (path, bytes) in tx.new_data {
            let entry = state.records.get_mut(&path).ok_or(Error::CorruptTree)?;
            if bytes.is_empty() {
                *entry = Record::file(entry.inode);
                continue;
            }
            let chunks = bytes.len().div_ceil(CHUNK);
            let start = builder.reserve(chunks as u64)?;
            for i in 0..chunks {
                let from = i * CHUNK;
                let to = usize::min(from + CHUNK, bytes.len());
                builder.staged.push((
                    start + i as u64,
                    encode_data(entry.inode, i as u32, &bytes[from..to], generation as u32),
                ));
            }
            entry.start = start;
            entry.count = chunks as u32;
            entry.length = bytes.len() as u64;
            entry.checksum = crc32c(&bytes);
            entry.birth_generation = generation;
        }
        let root_node = build_nodes(&mut builder, &state.records, generation)?;
        state.cursor = builder.cursor;
        state.tree_root = root_node;
        let manifest = root_manifest(&state, generation);
        self.volume
            .commit_root_with_blocks(&builder.staged, &manifest)?;
        self.state = state;
        Ok(generation)
    }

    pub fn into_device(self) -> D {
        self.volume.into_device()
    }
}

/// Narrow read wrapper so FileSystem never exposes unfenced block writes.
struct ReaderDevice<'a, D: BlockDevice> {
    volume: &'a mut Volume<D>,
}
impl<D: BlockDevice> BlockDevice for ReaderDevice<'_, D> {
    fn block_count(&self) -> u64 {
        self.volume.block_count()
    }
    fn read_block(&mut self, block: u64, out: &mut [u8; BLOCK_SIZE]) -> Result<()> {
        *out = self.volume.read_data_block(block)?;
        Ok(())
    }
    fn write_block(&mut self, _block: u64, _data: &[u8; BLOCK_SIZE]) -> Result<()> {
        Err(Error::Poisoned)
    }
    fn sync(&mut self) -> Result<()> {
        Err(Error::Poisoned)
    }
}
