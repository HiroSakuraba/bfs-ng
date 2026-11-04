//! F2.R2: physical sharing, fallback-root retirement epochs and structural
//! corruption tests for the checksum-protected CoW reference B+tree.
use bfs_ng_storage::{BLOCK_SIZE, BlockDevice, CrashDevice, Error, FileSystem, Volume};
use std::collections::BTreeSet;

fn blank(blocks: usize) -> FileSystem<CrashDevice> {
    FileSystem::format(CrashDevice::new(blocks), 0xfeed_2026).unwrap()
}

fn ref_root(device: CrashDevice) -> (CrashDevice, u64, usize) {
    let mut volume = Volume::open(device).unwrap();
    let payload = volume.read_root().unwrap().unwrap();
    assert_eq!(&payload[..8], b"BFSNGM6\0");
    let snaps = u32::from_le_bytes(payload[56..60].try_into().unwrap()) as usize;
    let off = 64 + snaps * 40;
    let first = u64::from_le_bytes(payload[off..off + 8].try_into().unwrap());
    let pages = u32::from_le_bytes(payload[off + 8..off + 12].try_into().unwrap()) as usize;
    (volume.into_device(), first, pages)
}

fn children(device: &mut CrashDevice, root: u64) -> BTreeSet<u64> {
    let mut raw = [0; BLOCK_SIZE];
    device.read_block(root, &mut raw).unwrap();
    assert_eq!(&raw[..8], b"BFSRBR6\0", "test needs a branch index");
    let count = u16::from_le_bytes(raw[28..30].try_into().unwrap()) as usize;
    (0..count)
        .map(|i| {
            let at = 32 + i * 16 + 8;
            u64::from_le_bytes(raw[at..at + 8].try_into().unwrap())
        })
        .collect()
}

fn crc32c(data: &[u8]) -> u32 {
    let mut state = !0_u32;
    for &byte in data {
        state ^= byte as u32;
        for _ in 0..8 {
            state = (state >> 1) ^ (0x82f6_3b78_u32 & (0_u32.wrapping_sub(state & 1)));
        }
    }
    !state
}

#[test]
fn a_small_change_retains_unaffected_reference_leaves() {
    let mut fs = blank(4096);
    let mut tx = fs.begin();
    tx.mkdir("/bulk").unwrap();
    for n in 0..180 {
        tx.write_file(&format!("/bulk/f{n:03}"), &vec![n as u8; 19_000])
            .unwrap();
    }
    fs.commit(tx).unwrap();
    let (mut device, old_root, old_pages) = ref_root(fs.into_device().power_loss());
    assert!(old_pages >= 4, "at least three leaves plus root");
    let old_children = children(&mut device, old_root);
    assert!(old_children.len() >= 3);

    let mut fs = FileSystem::open(device).unwrap();
    let mut tx = fs.begin();
    tx.write_file("/bulk/f000", b"updated record").unwrap();
    fs.commit(tx).unwrap();
    let (mut device, next_root, next_pages) = ref_root(fs.into_device().power_loss());
    assert_ne!(old_root, next_root, "a changed index must path-copy its root");
    assert!(next_pages >= 4);
    let next_children = children(&mut device, next_root);
    assert!(
        old_children.intersection(&next_children).next().is_some(),
        "an unrelated reference subtree must remain physically shared"
    );
    let mut fs = FileSystem::open(device).unwrap();
    assert_eq!(fs.read_file("/bulk/f000").unwrap(), b"updated record");
    assert_eq!(fs.read_file("/bulk/f179").unwrap(), vec![179; 19_000]);
}

#[test]
fn fallback_only_blocks_are_delayed_until_slot_rotation() {
    let mut fs = blank(256);
    let mut tx = fs.begin();
    tx.write_file("/k", b"first").unwrap();
    fs.commit(tx).unwrap();
    let mut tx = fs.begin();
    tx.write_file("/k", b"second").unwrap();
    fs.commit(tx).unwrap();

    let report = fs.allocation_report().unwrap();
    assert_eq!(report.generation, 2);
    assert!(report.delayed_free_blocks > 0);
    assert_eq!(report.delayed_free_eligible_generation, 4);
    let mut fs = FileSystem::open(fs.into_device().power_loss()).unwrap();
    assert_eq!(fs.read_file("/k").unwrap(), b"second");
    let after_recovery = fs.allocation_report().unwrap();
    assert_eq!(after_recovery.delayed_free_blocks, report.delayed_free_blocks);

    let mut tx = fs.begin();
    tx.write_file("/k", b"third").unwrap();
    fs.commit(tx).unwrap();
    let rotated = fs.allocation_report().unwrap();
    assert_eq!(rotated.generation, 3);
    assert!(rotated.reclaimable_blocks > 0);
}

#[test]
fn checksum_correct_forged_separator_rejects_latest_root() {
    let mut fs = blank(4096);
    let mut tx = fs.begin();
    tx.mkdir("/bulk").unwrap();
    for n in 0..140 {
        tx.write_file(&format!("/bulk/f{n:03}"), &vec![n as u8; 19_000])
            .unwrap();
    }
    fs.commit(tx).unwrap();
    let mut tx = fs.begin();
    tx.write_file("/bulk/f000", b"new").unwrap();
    fs.commit(tx).unwrap();
    let (mut device, root, _) = ref_root(fs.into_device().power_loss());
    let mut block = [0; BLOCK_SIZE];
    device.read_block(root, &mut block).unwrap();
    assert_eq!(&block[..8], b"BFSRBR6\0");
    let key = u64::from_le_bytes(block[32..40].try_into().unwrap());
    block[32..40].copy_from_slice(&(key + 1).to_le_bytes());
    block[24..28].fill(0);
    let checksum = crc32c(&block);
    block[24..28].copy_from_slice(&checksum.to_le_bytes());
    device.write_block(root, &block).unwrap();
    device.sync().unwrap();

    let mut recovered = FileSystem::open(device.power_loss()).unwrap();
    assert_eq!(recovered.generation(), 1);
    assert_eq!(recovered.read_file("/bulk/f000").unwrap(), vec![0; 19_000]);
    assert!(recovered.allocation_report().unwrap().reuse_blocked);
    assert!(matches!(
        recovered.read_file("/missing"),
        Err(Error::NotFound) | Err(Error::InvalidPath)
    ));
}
