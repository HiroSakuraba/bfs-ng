//! Two alternating CRC32C root slots and three rotating CoW payload blocks.
//! The durable ordering is: data -> sync -> payload -> sync -> root slot -> sync.
use crate::{BLOCK_SIZE, BlockDevice, Error, ROOT_CAPACITY, Result};

const SUPER_MAGIC: [u8; 8] = *b"BFSNGF0\0";
const PAYLOAD_MAGIC: [u8; 8] = *b"BFSNGR0\0";
const VERSION: u32 = 2;
/// Fixed, permanently reserved root-payload region [2, 4]. Every pair of
/// consecutive generations uses distinct blocks, and the next commit always
/// targets the third (unreferenced) block. These addresses NEVER belong to F1.
pub(crate) const ROOT_PAYLOAD_LAST: u64 = 4;
const ROOT_RING_SIZE: u64 = ROOT_PAYLOAD_LAST - 1;
const MIN_BLOCKS: u64 = ROOT_PAYLOAD_LAST + 1;

fn payload_block(generation: u64) -> Option<u64> {
    (generation > 0).then_some(2 + generation.saturating_sub(1) % ROOT_RING_SIZE)
}
const NO_ROOT: u64 = u64::MAX;
const CHECKSUM_OFFSET: usize = 120;

pub(crate) fn crc32c(data: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ ((0_u32.wrapping_sub(crc & 1)) & 0x82f6_3b78);
        }
    }
    !crc
}

fn u32_at(block: &[u8], start: usize) -> u32 {
    u32::from_le_bytes(block[start..start + 4].try_into().expect("fixed offsets"))
}

fn u64_at(block: &[u8], start: usize) -> u64 {
    u64::from_le_bytes(block[start..start + 8].try_into().expect("fixed offsets"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootInfo {
    pub volume_id: u128,
    pub generation: u64,
    pub root_block: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
struct Super {
    volume_id: u128,
    generation: u64,
    root_block: Option<u64>,
    root_crc: u32,
    blocks: u64,
}

impl Super {
    fn initial(volume_id: u128, blocks: u64) -> Self {
        Self {
            volume_id,
            generation: 0,
            root_block: None,
            root_crc: 0,
            blocks,
        }
    }

    fn encode(self) -> [u8; BLOCK_SIZE] {
        let mut block = [0_u8; BLOCK_SIZE];
        block[0..8].copy_from_slice(&SUPER_MAGIC);
        block[8..12].copy_from_slice(&VERSION.to_le_bytes());
        block[12..16].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
        block[16..32].copy_from_slice(&self.volume_id.to_le_bytes());
        block[32..40].copy_from_slice(&self.generation.to_le_bytes());
        block[40..48].copy_from_slice(&self.root_block.unwrap_or(NO_ROOT).to_le_bytes());
        block[48..52].copy_from_slice(&self.root_crc.to_le_bytes());
        block[56..64].copy_from_slice(&self.blocks.to_le_bytes());
        let checksum = crc32c(&block[..CHECKSUM_OFFSET]);
        block[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());
        block
    }

    fn decode(block: &[u8; BLOCK_SIZE], slot: u64, blocks: u64) -> Option<Self> {
        if block[..8] != SUPER_MAGIC
            || u32_at(block, 8) != VERSION
            || u32_at(block, 12) != BLOCK_SIZE as u32
            || u64_at(block, 56) != blocks
            || block[52..56].iter().any(|b| *b != 0)
            || block[64..CHECKSUM_OFFSET].iter().any(|b| *b != 0)
            || block[CHECKSUM_OFFSET + 4..].iter().any(|b| *b != 0)
            || u32_at(block, CHECKSUM_OFFSET) != crc32c(&block[..CHECKSUM_OFFSET])
        {
            return None;
        }
        let volume_id = u128::from_le_bytes(block[16..32].try_into().ok()?);
        let generation = u64_at(block, 32);
        let block_number = u64_at(block, 40);
        let root_crc = u32_at(block, 48);
        if volume_id == 0 || generation % 2 != slot {
            return None;
        }
        let root_block = if generation == 0 {
            if block_number != NO_ROOT || root_crc != 0 {
                return None;
            }
            None
        } else {
            let expected = payload_block(generation)?;
            if block_number != expected || block_number >= blocks {
                return None;
            }
            Some(block_number)
        };
        Some(Self {
            volume_id,
            generation,
            root_block,
            root_crc,
            blocks,
        })
    }

    fn info(self) -> RootInfo {
        RootInfo {
            volume_id: self.volume_id,
            generation: self.generation,
            root_block: self.root_block,
        }
    }
}

/// One writer per image. Never call this through a second concurrent device.
/// Snapshots and free-space allocation do not exist in F0.
pub struct Volume<D: BlockDevice> {
    device: D,
    head: Super,
    poisoned: bool,
}

fn read_payload<D: BlockDevice>(device: &mut D, head: Super) -> Result<Option<Vec<u8>>> {
    let Some(block_number) = head.root_block else {
        return Ok(Some(Vec::new()));
    };
    let mut block = [0_u8; BLOCK_SIZE];
    device.read_block(block_number, &mut block)?;
    if crc32c(&block) != head.root_crc
        || block[..8] != PAYLOAD_MAGIC
        || block[12..16].iter().any(|b| *b != 0)
    {
        return Ok(None);
    }
    let len = u32_at(&block, 8) as usize;
    if len > ROOT_CAPACITY || block[16 + len..].iter().any(|b| *b != 0) {
        return Ok(None);
    }
    Ok(Some(block[16..16 + len].to_vec()))
}

impl<D: BlockDevice> Volume<D> {
    /// Only formats a *blank, disposable* image. Reserved blocks 0/1 must be
    /// zero; all later blocks are treated as unused.
    pub fn format(mut device: D, volume_id: u128) -> Result<Self> {
        if volume_id == 0 || device.block_count() < MIN_BLOCKS {
            return Err(Error::Geometry);
        }
        // F0 has no reclaimable allocation map. Formatting is permitted
        // only on an entirely blank, disposable image, not merely empty
        // root slots that might hide orphaned user data.
        for block_number in 0..device.block_count() {
            let mut block = [0_u8; BLOCK_SIZE];
            device.read_block(block_number, &mut block)?;
            if block.iter().any(|b| *b != 0) {
                return Err(Error::NotBlank);
            }
        }
        let head = Super::initial(volume_id, device.block_count());
        device.write_block(0, &head.encode())?;
        device.sync()?;
        Ok(Self {
            device,
            head,
            poisoned: false,
        })
    }

    /// Validates user payloads for *each* recoverable root candidate.
    /// A verifier may reject a checksummed F0 root whose referenced F1
    /// metadata/data nodes are invalid; the previous slot is then tried.
    pub(crate) fn open_verified<T>(
        mut device: D,
        mut verify: impl FnMut(&mut D, RootInfo, Option<&[u8]>) -> Result<Option<T>>,
    ) -> Result<(Self, T)> {
        if device.block_count() < MIN_BLOCKS {
            return Err(Error::Geometry);
        }
        let mut candidates = Vec::new();
        for slot in 0..2 {
            let mut block = [0_u8; BLOCK_SIZE];
            device.read_block(slot, &mut block)?;
            if let Some(candidate) = Super::decode(&block, slot, device.block_count()) {
                candidates.push(candidate);
            }
        }
        if candidates.is_empty() {
            return Err(Error::NoValidRoot);
        }
        if candidates.len() == 2 {
            let a = candidates[0];
            let b = candidates[1];
            if a.volume_id != b.volume_id
                || a.blocks != b.blocks
                || a.generation == b.generation
                || a.generation.abs_diff(b.generation) != 1
            {
                return Err(Error::InconsistentVolume);
            }
        }
        candidates.sort_by(|a, b| b.generation.cmp(&a.generation));
        for head in candidates {
            if let Some(bytes) = read_payload(&mut device, head)? {
                let payload = head.root_block.map(|_| bytes.as_slice());
                if let Some(state) = verify(&mut device, head.info(), payload)? {
                    return Ok((
                        Self {
                            device,
                            head,
                            poisoned: false,
                        },
                        state,
                    ));
                }
            }
        }
        Err(Error::CorruptRoot)
    }

    /// Writes new, caller-allocated immutable blocks and publishes a root
    /// only after a successful data sync barrier. Caller must ensure these
    /// addresses are unreferenced by all live and fallback roots.
    pub(crate) fn commit_root_with_blocks(
        &mut self,
        blocks: &[(u64, [u8; BLOCK_SIZE])],
        payload: &[u8],
    ) -> Result<RootInfo> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        if payload.len() > ROOT_CAPACITY {
            return Err(Error::TooLarge);
        }
        let next_generation = self
            .head
            .generation
            .checked_add(1)
            .ok_or(Error::GenerationOverflow)?;
        let root_block = payload_block(next_generation).ok_or(Error::GenerationOverflow)?;
        let mut seen = std::collections::BTreeSet::new();
        for (number, _) in blocks {
            if *number <= ROOT_PAYLOAD_LAST || *number >= self.head.blocks || !seen.insert(*number) {
                return Err(Error::Geometry);
            }
        }
        if root_block >= self.head.blocks {
            return Err(Error::NoSpace);
        }
        // After the first write, an error makes the handle unusable until
        // recovery: a failed write may have reached stable media.
        self.poisoned = true;
        for (number, bytes) in blocks {
            self.device.write_block(*number, bytes)?;
        }
        self.device.sync()?;
        // commit_root performs payload write -> sync -> root-slot write -> sync.
        self.poisoned = false;
        self.commit_root(payload)
    }

    pub(crate) fn read_data_block(&mut self, number: u64) -> Result<[u8; BLOCK_SIZE]> {
        if number < 2 || number >= self.head.blocks {
            return Err(Error::OutOfBounds(number));
        }
        let mut block = [0; BLOCK_SIZE];
        self.device.read_block(number, &mut block)?;
        Ok(block)
    }

    /// All structurally valid F0 roots with checksummed payloads. Callers
    /// must independently verify F1 trees before considering blocks reusable.
    /// A valid F0 slot is not proof that its higher-level tree is intact.
    pub(crate) fn candidate_roots(&mut self) -> Result<Vec<(RootInfo, Option<Vec<u8>>)>> {
        let mut roots = Vec::new();
        for slot in 0..2 {
            let mut raw = [0; BLOCK_SIZE];
            self.device.read_block(slot, &mut raw)?;
            let Some(head) = Super::decode(&raw, slot, self.head.blocks) else {
                continue;
            };
            if head.volume_id != self.head.volume_id {
                return Err(Error::InconsistentVolume);
            }
            // A checksummed F0 slot with a damaged payload still owns
            // UNKNOWN high-region blocks. Expose it to F2's scrub as an
            // invalid candidate rather than silently treating its space as
            // reclaimable. Generation zero deliberately has no payload.
            let payload = read_payload(&mut self.device, head)?
                .map(|bytes| head.root_block.map(|_| bytes))
                .unwrap_or(None);
            roots.push((head.info(), payload));
        }
        roots.sort_by_key(|(info, _)| info.generation);
        Ok(roots)
    }

    /// Allow F1 to examine child blocks without exposing raw-device writes.
    pub(crate) fn inspect<R>(&mut self, f: impl FnOnce(&mut D) -> Result<R>) -> Result<R> {
        f(&mut self.device)
    }

    /// Reopens a volume, selecting the highest complete and checksummed root.
    /// A damaged latest root falls back to the previous valid slot. A malformed
    /// image with no valid root is refused, never silently formatted.
    pub fn open(mut device: D) -> Result<Self> {
        if device.block_count() < MIN_BLOCKS {
            return Err(Error::Geometry);
        }
        let mut candidates = Vec::new();
        for slot in 0..2 {
            let mut block = [0_u8; BLOCK_SIZE];
            device.read_block(slot, &mut block)?;
            if let Some(candidate) = Super::decode(&block, slot, device.block_count()) {
                candidates.push(candidate);
            }
        }
        if candidates.is_empty() {
            return Err(Error::NoValidRoot);
        }
        if candidates.len() == 2 {
            let a = candidates[0];
            let b = candidates[1];
            if a.volume_id != b.volume_id
                || a.blocks != b.blocks
                || a.generation == b.generation
                || a.generation.abs_diff(b.generation) != 1
            {
                return Err(Error::InconsistentVolume);
            }
        }
        candidates.sort_by(|a, b| b.generation.cmp(&a.generation));
        for head in candidates {
            if read_payload(&mut device, head)?.is_some() {
                return Ok(Self {
                    device,
                    head,
                    poisoned: false,
                });
            }
        }
        Err(Error::CorruptRoot)
    }

    pub fn info(&self) -> RootInfo {
        self.head.info()
    }

    pub fn block_count(&self) -> u64 {
        self.head.blocks
    }

    pub fn read_root(&mut self) -> Result<Option<Vec<u8>>> {
        if self.head.root_block.is_none() {
            return Ok(None);
        }
        read_payload(&mut self.device, self.head)?
            .ok_or(Error::CorruptRoot)
            .map(Some)
    }

    /// Publish into the third slot of a three-block rotating CoW payload ring.
    /// The new payload is never in either current/fallback recoverable root.
    /// A successful return means both sync barriers succeeded. A failed commit
    /// poisons this handle; reopen from disk before another attempt.
    pub fn commit_root(&mut self, payload: &[u8]) -> Result<RootInfo> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        if payload.len() > ROOT_CAPACITY {
            return Err(Error::TooLarge);
        }
        let generation = self
            .head
            .generation
            .checked_add(1)
            .ok_or(Error::GenerationOverflow)?;
        let next_block = payload_block(generation).ok_or(Error::GenerationOverflow)?;
        if next_block >= self.head.blocks {
            return Err(Error::NoSpace);
        }
        let mut block = [0_u8; BLOCK_SIZE];
        block[..8].copy_from_slice(&PAYLOAD_MAGIC);
        block[8..12].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        block[16..16 + payload.len()].copy_from_slice(payload);
        let head = Super {
            volume_id: self.head.volume_id,
            generation,
            root_block: Some(next_block),
            root_crc: crc32c(&block),
            blocks: self.head.blocks,
        };
        let action = (|| {
            self.device.write_block(next_block, &block)?;
            self.device.sync()?; // CoW payload precedes publishing its pointer.
            self.device.write_block(generation % 2, &head.encode())?;
            self.device.sync()?; // Root slot publication barrier.
            Ok(())
        })();
        match action {
            Ok(()) => {
                self.head = head;
                Ok(head.info())
            }
            Err(error) => {
                self.poisoned = true;
                Err(error)
            }
        }
    }

    pub fn into_device(self) -> D {
        self.device
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_crc32c_vector() {
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }

    #[test]
    fn rejects_corrupt_superblock_and_geometry() {
        let h = Super::initial(55, 16);
        let mut block = h.encode();
        assert!(Super::decode(&block, 0, 16).is_some());
        block[18] ^= 0x80;
        assert!(Super::decode(&block, 0, 16).is_none());
        assert!(Super::decode(&h.encode(), 1, 16).is_none());
        assert!(Super::decode(&h.encode(), 0, 12).is_none());
    }
}
