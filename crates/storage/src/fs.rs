//! F1: bounded, append-only CoW namespace with checksummed metadata/data.
//! This intentionally rewrites the *entire* sorted tree per transaction.
use crate::volume::{RootInfo, crc32c};
use crate::{BLOCK_SIZE, BlockDevice, Error, Result, Volume};
use std::collections::{BTreeMap, BTreeSet};

const META_MAGIC: &[u8; 8] = b"BFSNGM1\0";
const NODE_MAGIC: &[u8; 8] = b"BFSNGN1\0";
const DATA_MAGIC: &[u8; 8] = b"BFSNGD1\0";
const FORMAT: u32 = 1;
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
        }
    }

    fn file(inode: u64) -> Self {
        Self {
            directory: false,
            ..Self::directory(inode)
        }
    }
}

#[derive(Clone)]
struct State {
    cursor: u64,
    next_inode: u64,
    records: BTreeMap<String, Record>,
}

impl State {
    fn empty(blocks: u64) -> Self {
        Self {
            cursor: blocks,
            next_inode: 2,
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
    dirty: bool,
}

impl FsTransaction {
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

fn root_manifest(state: &State, root_node: u64, generation: u64) -> [u8; 56] {
    let mut b = [0_u8; 56];
    b[..8].copy_from_slice(META_MAGIC);
    b[8..12].copy_from_slice(&FORMAT.to_le_bytes());
    b[16..24].copy_from_slice(&state.cursor.to_le_bytes());
    b[24..32].copy_from_slice(&root_node.to_le_bytes());
    b[32..40].copy_from_slice(&state.next_inode.to_le_bytes());
    b[40..48].copy_from_slice(&(state.records.len() as u64).to_le_bytes());
    b[48..56].copy_from_slice(&generation.to_le_bytes());
    b
}

struct Builder {
    cursor: u64,
    root_limit: u64,
    staged: Vec<(u64, [u8; BLOCK_SIZE])>,
}

impl Builder {
    fn new(cursor: u64, next_root: u64) -> Self {
        Self {
            cursor,
            root_limit: next_root,
            staged: Vec::new(),
        }
    }

    fn reserve(&mut self, amount: u64) -> Result<u64> {
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

fn encode_data(inode: u64, index: u32, bytes: &[u8]) -> [u8; BLOCK_SIZE] {
    let mut b = [0; BLOCK_SIZE];
    b[..8].copy_from_slice(DATA_MAGIC);
    b[8..16].copy_from_slice(&inode.to_le_bytes());
    b[16..20].copy_from_slice(&index.to_le_bytes());
    b[20..24].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
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
        return Ok(
            (record.start == 0 && record.count == 0 && record.checksum == crc32c(&[]))
                .then(Vec::new),
        );
    }
    let len = usize::try_from(record.length).map_err(|_| Error::CorruptTree)?;
    if len > MAX_DATA || record.count as usize != len.div_ceil(CHUNK) {
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
            || b[28..32].iter().any(|n| *n != 0)
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
        const LEN: usize = 33;
        if at + LEN > BLOCK_SIZE {
            return Err(Error::TooLarge);
        }
        b[at..at + 8].copy_from_slice(&record.inode.to_le_bytes());
        b[at + 8] = u8::from(record.directory);
        b[at + 9..at + 17].copy_from_slice(&record.start.to_le_bytes());
        b[at + 17..at + 21].copy_from_slice(&record.count.to_le_bytes());
        b[at + 21..at + 29].copy_from_slice(&record.length.to_le_bytes());
        b[at + 29..at + 33].copy_from_slice(&record.checksum.to_le_bytes());
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
            let Some(raw) = rd.take(33) else {
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

fn load_state<D: BlockDevice>(
    device: &mut D,
    info: RootInfo,
    payload: Option<&[u8]>,
) -> Result<Option<State>> {
    let Some(bytes) = payload else {
        return Ok((info.generation == 0).then(|| State::empty(device.block_count())));
    };
    if bytes.len() != 56
        || &bytes[..8] != META_MAGIC
        || u32_at(bytes, 8) != FORMAT
        || bytes[12..16].iter().any(|b| *b != 0)
        || u64_at(bytes, 48) != info.generation
    {
        return Ok(None);
    }
    let cursor = u64_at(bytes, 16);
    let root_node = u64_at(bytes, 24);
    let next_inode = u64_at(bytes, 32);
    let count = u64_at(bytes, 40);
    if info.generation == 0
        || next_inode < 2
        || cursor <= info.generation + 1
        || cursor > device.block_count()
        || count > MAX_RECORDS as u64
        || (root_node == 0) != (count == 0)
    {
        return Ok(None);
    }
    let mut records = BTreeMap::new();
    let mut seen = BTreeSet::new();
    if root_node != 0
        && read_nodes(
            device,
            root_node,
            cursor,
            info.generation,
            None,
            &mut seen,
            &mut records,
        )?
        .is_none()
    {
        return Ok(None);
    }
    if records.len() != count as usize {
        return Ok(None);
    }
    let mut inodes = BTreeSet::new();
    let mut active_blocks = BTreeSet::new();
    for (path, record) in &records {
        if record.inode >= next_inode || !inodes.insert(record.inode) {
            return Ok(None);
        }
        let slash = path.rfind('/').expect("validated absolute path");
        if slash > 0 && !records.get(&path[..slash]).is_some_and(|r| r.directory) {
            return Ok(None);
        }
        if record.directory {
            if record.start != 0 || record.count != 0 || record.length != 0 || record.checksum != 0
            {
                return Ok(None);
            }
        } else {
            if record.length > MAX_DATA as u64 || record.count as usize > MAX_DATA.div_ceil(CHUNK) {
                return Ok(None);
            }
            if record.count > 0 {
                let end = record
                    .start
                    .checked_add(record.count as u64)
                    .ok_or(Error::CorruptTree)?;
                for pos in record.start..end {
                    if !active_blocks.insert(pos) || seen.contains(&pos) {
                        return Ok(None);
                    }
                }
            }
            if data_read(device, record, cursor, info.generation)?.is_none() {
                return Ok(None);
            }
        }
    }
    if seen.iter().any(|n| active_blocks.contains(n)) {
        return Ok(None);
    }
    Ok(Some(State {
        cursor,
        next_inode,
        records,
    }))
}

/// F1 namespace backed by a checksummed, append-only immutable block tree.
/// One writer per image; no production mount, snapshots or reclamation.
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
        let mut builder = Builder::new(self.state.cursor, root_block);
        let mut state = State {
            cursor: self.state.cursor,
            next_inode: tx.next_inode,
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
                    encode_data(entry.inode, i as u32, &bytes[from..to]),
                ));
            }
            entry.start = start;
            entry.count = chunks as u32;
            entry.length = bytes.len() as u64;
            entry.checksum = crc32c(&bytes);
        }
        let root_node = build_nodes(&mut builder, &state.records, generation)?;
        state.cursor = builder.cursor;
        let manifest = root_manifest(&state, root_node, generation);
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
