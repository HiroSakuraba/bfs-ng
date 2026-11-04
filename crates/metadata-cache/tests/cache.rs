use bfs_ng_metadata_cache::{
    BlockReader, BlockRef, CacheError, HEADER_LEN, MetadataCache, Snapshot, crc32c_metadata,
};
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, RwLock};

const SIZE: usize = 128;

fn r(physical_block: u64, allocation_generation: u64) -> BlockRef {
    BlockRef { physical_block, allocation_generation }
}

fn snap(generation: u64) -> Snapshot {
    Snapshot {
        volume: 0x1234,
        generation,
    }
}

fn encoded(snapshot: Snapshot, address: u64, payload: u8) -> Vec<u8> {
    let mut b = vec![0u8; SIZE];
    b[..4].copy_from_slice(b"BFNM");
    b[4..6].copy_from_slice(&1u16.to_le_bytes());
    b[6..8].copy_from_slice(&7u16.to_le_bytes());
    b[8..24].copy_from_slice(&snapshot.volume.to_le_bytes());
    b[24..32].copy_from_slice(&address.to_le_bytes());
    b[32..40].copy_from_slice(&snapshot.generation.to_le_bytes());
    b[HEADER_LEN..].fill(payload);
    let crc = crc32c_metadata(&b);
    b[40..44].copy_from_slice(&crc.to_le_bytes());
    b
}

#[derive(Default)]
struct MemoryReader(RwLock<HashMap<u64, Vec<u8>>>);

impl MemoryReader {
    fn put(&self, address: u64, value: Vec<u8>) {
        self.0.write().unwrap().insert(address, value);
    }
}

impl BlockReader for MemoryReader {
    fn read_block(&self, address: u64, destination: &mut [u8]) -> io::Result<()> {
        let guard = self.0.read().unwrap();
        let source = guard
            .get(&address)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        if source.len() != destination.len() {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        destination.copy_from_slice(source);
        Ok(())
    }
}

#[test]
fn same_generation_hits_and_is_immutable() {
    let reader = Arc::new(MemoryReader::default());
    reader.put(9, encoded(snap(1), 9, 0xaa));
    let cache = MetadataCache::new(reader, SIZE, SIZE).unwrap();
    let first = cache.get(snap(1), r(9, 1)).unwrap();
    assert_eq!(first.kind(), 7);
    assert_eq!(first.payload()[0], 0xaa);
    let second = cache.get(snap(1), r(9, 1)).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    let s = cache.stats().unwrap();
    assert_eq!((s.hits, s.misses, s.validated_reads), (1, 1, 1));
}

#[test]
fn reused_address_never_aliases_new_generation() {
    let device = Arc::new(MemoryReader::default());
    device.put(9, encoded(snap(1), 9, 0xaa));
    let cache = MetadataCache::new(device.clone(), SIZE, SIZE * 2).unwrap();
    let old = cache.get(snap(1), r(9, 1)).unwrap();
    device.put(9, encoded(snap(2), 9, 0xbb));
    let new = cache.get(snap(2), r(9, 2)).unwrap();
    assert_eq!(old.payload()[0], 0xaa);
    assert_eq!(new.payload()[0], 0xbb);
    assert_ne!(old.key(), new.key());
    // This is a post-retirement reuse test. A real allocator must retain
    // old physical blocks while an old snapshot remains active.
}

#[test]
fn wrong_generation_never_admitted() {
    let device = Arc::new(MemoryReader::default());
    device.put(9, encoded(snap(2), 9, 5));
    let cache = MetadataCache::new(device, SIZE, SIZE).unwrap();
    assert!(matches!(
        cache.get(snap(1), r(9, 1)),
        Err(CacheError::WrongIdentity { .. })
    ));
    assert_eq!(cache.stats().unwrap().resident_blocks, 0);
}

#[test]
fn corrupted_payload_never_cached() {
    let device = Arc::new(MemoryReader::default());
    let mut data = encoded(snap(3), 1, 4);
    data[HEADER_LEN] ^= 1;
    device.put(1, data);
    let cache = MetadataCache::new(device, SIZE, SIZE * 2).unwrap();
    assert!(matches!(
        cache.get(snap(3), r(1, 3)),
        Err(CacheError::ChecksumMismatch { .. })
    ));
    assert_eq!(cache.stats().unwrap().resident_bytes, 0);
}

#[test]
fn error_does_not_poison_cache() {
    let device = Arc::new(MemoryReader::default());
    let cache = MetadataCache::new(device.clone(), SIZE, SIZE).unwrap();
    assert!(matches!(cache.get(snap(1), r(8, 1)), Err(CacheError::Device(_))));
    device.put(8, encoded(snap(1), 8, 2));
    assert_eq!(cache.get(snap(1), r(8, 1)).unwrap().payload()[0], 2);
}

#[test]
fn lru_eviction_keeps_existing_handles_alive() {
    let device = Arc::new(MemoryReader::default());
    for id in 1..=3 {
        device.put(id, encoded(snap(1), id, id as u8));
    }
    let cache = MetadataCache::new(device, SIZE, SIZE * 2).unwrap();
    let retained = cache.get(snap(1), r(1, 1)).unwrap();
    cache.get(snap(1), r(2, 1)).unwrap();
    cache.get(snap(1), r(1, 1)).unwrap();
    cache.get(snap(1), r(3, 1)).unwrap();
    assert_eq!(cache.stats().unwrap().evictions, 1);
    assert_eq!(retained.payload()[0], 1);
    assert_eq!(cache.stats().unwrap().resident_bytes, SIZE * 2);
}

#[test]
fn zero_budget_disables_residency() {
    let device = Arc::new(MemoryReader::default());
    device.put(2, encoded(snap(1), 2, 9));
    let cache = MetadataCache::new(device, SIZE, 0).unwrap();
    cache.get(snap(1), r(2, 1)).unwrap();
    cache.get(snap(1), r(2, 1)).unwrap();
    assert_eq!(cache.stats().unwrap().resident_blocks, 0);
    assert_eq!(cache.stats().unwrap().validated_reads, 2);
}

#[test]
fn concurrent_access_returns_consistent_immutable_entries() {
    let device = Arc::new(MemoryReader::default());
    device.put(2, encoded(snap(1), 2, 9));
    let cache = Arc::new(MetadataCache::new(device, SIZE, SIZE * 4).unwrap());
    let threads: Vec<_> = (0..12)
        .map(|_| {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || {
                for _ in 0..100 {
                    let b = cache.get(snap(1), r(2, 1)).unwrap();
                    assert_eq!(b.key(), snap(1).key(r(2, 1)));
                    assert_eq!(b.payload()[0], 9);
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(cache.stats().unwrap().resident_blocks, 1);
}

#[test]
fn unchanged_block_shared_by_later_snapshot_reuses_cached_entry() {
    let device = Arc::new(MemoryReader::default());
    device.put(7, encoded(snap(1), 7, 6));
    let cache = MetadataCache::new(device, SIZE, SIZE).unwrap();
    let old = cache.get(snap(1), r(7, 1)).unwrap();
    let later = cache.get(snap(9), r(7, 1)).unwrap();
    assert!(Arc::ptr_eq(&old, &later));
}

#[test]
fn snapshot_cannot_reference_future_allocation() {
    let device = Arc::new(MemoryReader::default());
    let cache = MetadataCache::new(device, SIZE, SIZE).unwrap();
    assert!(matches!(
        cache.get(snap(1), r(7, 2)),
        Err(CacheError::FutureReference { .. })
    ));
}
