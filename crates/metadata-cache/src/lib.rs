//! Read-only metadata cache. The allocator must retain blocks reachable by
//! active snapshots; generation-tagged keys do not confer physical retention.
#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};

pub const HEADER_LEN: usize = 48;
const MAGIC: &[u8; 4] = b"BFNM";
const VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockKey {
    pub volume: u128,
    pub physical_block: u64,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub volume: u128,
    pub generation: u64,
}

impl Snapshot {
    pub const fn key(self, block: BlockRef) -> BlockKey {
        BlockKey {
            volume: self.volume,
            physical_block: block.physical_block,
            generation: block.allocation_generation,
        }
    }
}

/// Reference to an immutable CoW metadata block. Its birth generation can
/// precede the committed root generation of a snapshot that references it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockRef {
    pub physical_block: u64,
    pub allocation_generation: u64,
}

/// A complete physical block from a snapshot-compatible, stable backing store.
/// Implementations must not return partial reads disguised as full blocks.
pub trait BlockReader: Send + Sync {
    fn read_block(&self, physical_block: u64, destination: &mut [u8]) -> io::Result<()>;
}

impl<T: BlockReader + ?Sized> BlockReader for Arc<T> {
    fn read_block(&self, physical_block: u64, destination: &mut [u8]) -> io::Result<()> {
        (**self).read_block(physical_block, destination)
    }
}

#[derive(Debug)]
pub enum CacheError {
    InvalidBlockSize,
    Device(io::Error),
    LockPoisoned,
    BadMagic,
    UnsupportedFormat(u16),
    InvalidReservedBits,
    WrongIdentity {
        expected: BlockKey,
        found: BlockKey,
    },
    FutureReference {
        snapshot_generation: u64,
        block_generation: u64,
    },
    ChecksumMismatch {
        stored: u32,
        computed: u32,
    },
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBlockSize => {
                write!(f, "metadata block must be at least {HEADER_LEN} bytes")
            }
            Self::Device(e) => write!(f, "block device: {e}"),
            Self::LockPoisoned => write!(f, "cache lock poisoned"),
            Self::BadMagic => write!(f, "metadata magic mismatch"),
            Self::UnsupportedFormat(v) => write!(f, "unsupported metadata format {v}"),
            Self::InvalidReservedBits => write!(f, "nonzero reserved metadata bits"),
            Self::WrongIdentity { expected, found } => {
                write!(f, "expected metadata {expected:?}, found {found:?}")
            }
            Self::FutureReference {
                snapshot_generation,
                block_generation,
            } => {
                write!(
                    f,
                    "block generation {block_generation} is newer than snapshot {snapshot_generation}"
                )
            }
            Self::ChecksumMismatch { stored, computed } => {
                write!(f, "checksum mismatch {stored:08x} != {computed:08x}")
            }
        }
    }
}

impl std::error::Error for CacheError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if let Self::Device(e) = self {
            Some(e)
        } else {
            None
        }
    }
}

impl From<io::Error> for CacheError {
    fn from(e: io::Error) -> Self {
        Self::Device(e)
    }
}

/// CRC32C Castagnoli, with checksum field at bytes 40..44 treated as zero.
/// Detects accidental corruption; it is not cryptographic authentication.
pub fn crc32c_metadata(block: &[u8]) -> u32 {
    let mut crc = !0u32;
    for (index, &byte) in block.iter().enumerate() {
        let value = if (40..44).contains(&index) { 0 } else { byte };
        crc ^= u32::from(value);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0x82f6_3b78 } else { 0 };
        }
    }
    !crc
}

/// Immutable and fully validated cache entry.
#[derive(Debug)]
pub struct MetadataBlock {
    key: BlockKey,
    kind: u16,
    bytes: Arc<[u8]>,
}

impl MetadataBlock {
    pub fn key(&self) -> BlockKey {
        self.key
    }
    pub fn kind(&self) -> u16 {
        self.kind
    }
    pub fn payload(&self) -> &[u8] {
        &self.bytes[HEADER_LEN..]
    }
    pub fn raw(&self) -> &[u8] {
        &self.bytes
    }
}

fn validate_block(bytes: Vec<u8>, expected: BlockKey) -> Result<MetadataBlock, CacheError> {
    if bytes.len() < HEADER_LEN {
        return Err(CacheError::InvalidBlockSize);
    }
    if &bytes[..4] != MAGIC {
        return Err(CacheError::BadMagic);
    }
    let version = u16::from_le_bytes(bytes[4..6].try_into().expect("fixed-length slice"));
    if version != VERSION {
        return Err(CacheError::UnsupportedFormat(version));
    }
    if bytes[44..48] != [0; 4] {
        return Err(CacheError::InvalidReservedBits);
    }
    let kind = u16::from_le_bytes(bytes[6..8].try_into().expect("fixed-length slice"));
    let found = BlockKey {
        volume: u128::from_le_bytes(bytes[8..24].try_into().expect("fixed-length slice")),
        physical_block: u64::from_le_bytes(bytes[24..32].try_into().expect("fixed-length slice")),
        generation: u64::from_le_bytes(bytes[32..40].try_into().expect("fixed-length slice")),
    };
    if found != expected {
        return Err(CacheError::WrongIdentity { expected, found });
    }
    let stored = u32::from_le_bytes(bytes[40..44].try_into().expect("fixed-length slice"));
    let computed = crc32c_metadata(&bytes);
    if stored != computed {
        return Err(CacheError::ChecksumMismatch { stored, computed });
    }
    Ok(MetadataBlock {
        key: expected,
        kind,
        bytes: bytes.into(),
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub validated_reads: u64,
    pub evictions: u64,
    pub resident_blocks: usize,
    pub resident_bytes: usize,
}

struct State {
    blocks: HashMap<BlockKey, Arc<MetadataBlock>>,
    recency: VecDeque<BlockKey>,
    stats: CacheStats,
}

impl State {
    fn touch(&mut self, key: BlockKey) {
        if let Some(index) = self.recency.iter().position(|&k| k == key) {
            self.recency.remove(index);
        }
        self.recency.push_back(key);
    }
}

/// Reference LRU with a byte budget. O(n) recency updates are intentional.
/// Concurrent cache misses can duplicate I/O; validation precedes insertion.
pub struct MetadataCache<D: BlockReader> {
    device: D,
    block_size: usize,
    capacity_bytes: usize,
    state: Mutex<State>,
}

impl<D: BlockReader> MetadataCache<D> {
    pub fn new(device: D, block_size: usize, capacity_bytes: usize) -> Result<Self, CacheError> {
        if block_size < HEADER_LEN {
            return Err(CacheError::InvalidBlockSize);
        }
        Ok(Self {
            device,
            block_size,
            capacity_bytes,
            state: Mutex::new(State {
                blocks: HashMap::new(),
                recency: VecDeque::new(),
                stats: CacheStats::default(),
            }),
        })
    }

    pub fn get(
        &self,
        snapshot: Snapshot,
        block: BlockRef,
    ) -> Result<Arc<MetadataBlock>, CacheError> {
        if block.allocation_generation > snapshot.generation {
            return Err(CacheError::FutureReference {
                snapshot_generation: snapshot.generation,
                block_generation: block.allocation_generation,
            });
        }
        let key = snapshot.key(block);
        {
            let mut state = self.state.lock().map_err(|_| CacheError::LockPoisoned)?;
            if let Some(hit) = state.blocks.get(&key).cloned() {
                state.stats.hits += 1;
                state.touch(key);
                return Ok(hit);
            }
            state.stats.misses += 1;
        }

        // No mutex held while reading potentially slow storage.
        let mut bytes = vec![0; self.block_size];
        self.device.read_block(block.physical_block, &mut bytes)?;
        let candidate = Arc::new(validate_block(bytes, key)?);
        let mut state = self.state.lock().map_err(|_| CacheError::LockPoisoned)?;
        state.stats.validated_reads += 1;
        if let Some(other) = state.blocks.get(&key).cloned() {
            state.touch(key);
            return Ok(other);
        }
        if self.capacity_bytes < self.block_size {
            return Ok(candidate);
        }
        while state.blocks.len() >= self.capacity_bytes / self.block_size {
            if let Some(oldest) = state.recency.pop_front() {
                if state.blocks.remove(&oldest).is_some() {
                    state.stats.evictions += 1;
                }
            } else {
                break;
            }
        }
        state.blocks.insert(key, Arc::clone(&candidate));
        state.recency.push_back(key);
        Ok(candidate)
    }

    pub fn stats(&self) -> Result<CacheStats, CacheError> {
        let state = self.state.lock().map_err(|_| CacheError::LockPoisoned)?;
        let mut stats = state.stats;
        stats.resident_blocks = state.blocks.len();
        stats.resident_bytes = state.blocks.len().saturating_mul(self.block_size);
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_known_vector() {
        assert_eq!(crc32c_metadata(b"123456789"), 0xe306_9283);
    }

    #[test]
    fn short_blocks_rejected() {
        let key = Snapshot {
            volume: 1,
            generation: 1,
        }
        .key(BlockRef {
            physical_block: 1,
            allocation_generation: 1,
        });
        assert!(matches!(
            validate_block(vec![0; 32], key),
            Err(CacheError::InvalidBlockSize)
        ));
    }
}
