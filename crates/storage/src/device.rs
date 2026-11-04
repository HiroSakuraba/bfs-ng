//! Fixed-size block devices. Never use an arbitrary user disk for F0 experiments.
use crate::{BLOCK_SIZE, Error, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

pub trait BlockDevice {
    fn block_count(&self) -> u64;
    fn read_block(&mut self, block: u64, out: &mut [u8; BLOCK_SIZE]) -> Result<()>;
    fn write_block(&mut self, block: u64, data: &[u8; BLOCK_SIZE]) -> Result<()>;
    /// Must order and durably persist all earlier writes on success.
    fn sync(&mut self) -> Result<()>;
}

/// A single-owner, preallocated file-backed image. The caller must serialize
/// all access; this implementation does not provide a cross-process lock.
pub struct FileDevice {
    file: File,
    blocks: u64,
}

impl FileDevice {
    pub fn create_new(path: impl AsRef<Path>, blocks: u64) -> Result<Self> {
        if blocks < 4 {
            return Err(Error::Geometry);
        }
        let length = blocks
            .checked_mul(BLOCK_SIZE as u64)
            .ok_or(Error::Geometry)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        file.set_len(length)?;
        file.sync_all()?;
        Ok(Self { file, blocks })
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        // Best effort: reject symlinks, not a defense against path-race attacks.
        let metadata = std::fs::symlink_metadata(path.as_ref())?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(Error::Geometry);
        }
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let length = file.metadata()?.len();
        if length < 4 * BLOCK_SIZE as u64 || length % BLOCK_SIZE as u64 != 0 {
            return Err(Error::Geometry);
        }
        Ok(Self {
            file,
            blocks: length / BLOCK_SIZE as u64,
        })
    }

    fn seek_block(&mut self, block: u64) -> Result<()> {
        if block >= self.blocks {
            return Err(Error::OutOfBounds(block));
        }
        self.file.seek(SeekFrom::Start(block * BLOCK_SIZE as u64))?;
        Ok(())
    }
}

impl BlockDevice for FileDevice {
    fn block_count(&self) -> u64 {
        self.blocks
    }

    fn read_block(&mut self, block: u64, out: &mut [u8; BLOCK_SIZE]) -> Result<()> {
        self.seek_block(block)?;
        self.file.read_exact(out)?;
        Ok(())
    }

    fn write_block(&mut self, block: u64, data: &[u8; BLOCK_SIZE]) -> Result<()> {
        self.seek_block(block)?;
        self.file.write_all(data)?;
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }
}

/// Inject a simulated power loss after the nth write/sync. A write may
/// partially reach stable media even without a successful sync.
#[derive(Clone, Copy, Debug)]
pub struct Fault {
    pub operation: usize,
    /// On a failing write, force 0..=BLOCK_SIZE leading bytes to stable media.
    /// None means no extra bytes from the interrupted write reach stable media.
    pub torn_bytes: Option<usize>,
}

/// Deterministic memory-backed durability *model*, not an actual device test.
pub struct CrashDevice {
    stable: Vec<[u8; BLOCK_SIZE]>,
    pending: Vec<[u8; BLOCK_SIZE]>,
    fault: Option<Fault>,
    operations: usize,
}

impl CrashDevice {
    pub fn new(blocks: usize) -> Self {
        Self {
            stable: vec![[0; BLOCK_SIZE]; blocks],
            pending: vec![[0; BLOCK_SIZE]; blocks],
            fault: None,
            operations: 0,
        }
    }

    pub fn inject(&mut self, fault: Fault) -> Result<()> {
        if fault.operation == 0 || fault.torn_bytes.is_some_and(|n| n > BLOCK_SIZE) {
            return Err(Error::Geometry);
        }
        self.operations = 0;
        self.fault = Some(fault);
        Ok(())
    }

    /// Discard volatile writes; restart from the durable bytes only.
    pub fn power_loss(mut self) -> Self {
        self.pending.clone_from(&self.stable);
        self.fault = None;
        self.operations = 0;
        self
    }

    /// Explicit test-only media corruption; never use on a production image.
    pub fn corrupt_stable_for_testing(&mut self, block: usize, offset: usize, value: u8) {
        self.stable[block][offset] = value;
        self.pending[block][offset] = value;
    }

    fn next_operation(&mut self) -> Option<Fault> {
        self.operations += 1;
        self.fault
            .filter(|fault| fault.operation == self.operations)
    }
}

impl BlockDevice for CrashDevice {
    fn block_count(&self) -> u64 {
        self.stable.len() as u64
    }

    fn read_block(&mut self, block: u64, out: &mut [u8; BLOCK_SIZE]) -> Result<()> {
        let src = self
            .pending
            .get(block as usize)
            .ok_or(Error::OutOfBounds(block))?;
        out.copy_from_slice(src);
        Ok(())
    }

    fn write_block(&mut self, block: u64, data: &[u8; BLOCK_SIZE]) -> Result<()> {
        let id = usize::try_from(block).map_err(|_| Error::OutOfBounds(block))?;
        if id >= self.pending.len() {
            return Err(Error::OutOfBounds(block));
        }
        if let Some(fault) = self.next_operation() {
            if let Some(prefix) = fault.torn_bytes {
                self.stable[id][..prefix].copy_from_slice(&data[..prefix]);
            }
            return Err(Error::InjectedCrash);
        }
        self.pending[id] = *data;
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        self.stable.clone_from(&self.pending);
        if self.next_operation().is_some() {
            return Err(Error::InjectedCrash);
        }
        Ok(())
    }
}
