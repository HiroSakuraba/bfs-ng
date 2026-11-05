use bfs_ng_storage::experimental_v10::{ReadOnlyV10, SnapshotState, VirginSnapshotWriter};
use bfs_ng_storage::{BLOCK_SIZE, BlockDevice, CrashDevice, Error, Fault, Volume};

const VID: u128 = 0x541b_785a_63d1_04af;
const BLOCKS: usize = 8192;

#[test]
fn virgin_writer_pins_100_generations_across_multilevel_splits_and_f0_rotations() {
    let mut writer = VirginSnapshotWriter::format(CrashDevice::new(BLOCKS), VID).unwrap();
    assert_eq!(writer.manifest().generation, 1);
    assert_eq!(writer.manifest().high_water, BLOCKS as u64 - 1);
    let immutable_fs = writer.manifest().roots.fs;
    let mut old_watermark = writer.manifest().high_water;
    for expected_generation in 2..=111_u64 {
        assert_eq!(writer.pin_current().unwrap(), expected_generation);
        assert!(writer.manifest().high_water < old_watermark);
        old_watermark = writer.manifest().high_water;
        if expected_generation % 16 == 0 {
            writer = VirginSnapshotWriter::open(writer.into_device().power_loss()).unwrap();
            let snapshots = writer.snapshots().unwrap();
            assert_eq!(snapshots.len(), expected_generation as usize - 1);
            assert!(snapshots.iter().all(|desc| desc.fs_root == immutable_fs));
        }
    }
    let mut reader = ReadOnlyV10::open(writer.into_device().power_loss()).unwrap();
    assert_eq!(reader.root_info().generation, 111);
    let pinned = reader.snapshot_descriptors().unwrap();
    assert_eq!(pinned.len(), 110);
    assert_eq!(pinned[0].generation, 1);
    assert_eq!(pinned.last().unwrap().generation, 110);
}

#[test]
fn dropping_state_is_durable_but_never_physically_reclaims_a_snapshot() {
    let mut writer = VirginSnapshotWriter::format(CrashDevice::new(96), VID).unwrap();
    assert_eq!(writer.pin_current().unwrap(), 2);
    assert_eq!(writer.mark_dropping(1).unwrap(), 3);
    writer = VirginSnapshotWriter::open(writer.into_device().power_loss()).unwrap();
    let pinned = writer.snapshots().unwrap();
    assert_eq!(pinned.len(), 1);
    assert_eq!(pinned[0].generation, 1);
    assert_eq!(pinned[0].state, SnapshotState::Dropping);
    assert!(matches!(
        writer.mark_dropping(1),
        Err(Error::InvalidSnapshot)
    ));
    assert_eq!(writer.manifest().generation, 3);
}

#[test]
fn orphaned_nonzero_block_below_watermark_is_quarantined_forever() {
    let writer = VirginSnapshotWriter::format(CrashDevice::new(80), VID).unwrap();
    let original_watermark = writer.manifest().high_water;
    let mut device = writer.into_device().power_loss();
    let mut orphan = [0_u8; BLOCK_SIZE];
    orphan[..8].copy_from_slice(b"ORPHAN!!");
    let orphan_address = original_watermark - 1;
    device.write_block(orphan_address, &orphan).unwrap();
    device.sync().unwrap();
    let mut writer = VirginSnapshotWriter::open(device.power_loss()).unwrap();
    assert_eq!(writer.pin_current().unwrap(), 2);
    // The contaminated block must not be overwritten by the new leaf.
    assert!(writer.manifest().high_water < orphan_address);
    let mut disk = writer.into_device().power_loss();
    let mut raw = [0; BLOCK_SIZE];
    disk.read_block(orphan_address, &mut raw).unwrap();
    assert_eq!(raw, orphan);
    let writer = VirginSnapshotWriter::open(disk.power_loss()).unwrap();
    assert_eq!(writer.manifest().generation, 2);
}

#[test]
fn corrupt_fallback_only_catalog_page_blocks_writer_but_not_lazy_reader() {
    let mut writer = VirginSnapshotWriter::format(CrashDevice::new(128), VID).unwrap();
    writer.pin_current().unwrap(); // g2 snapshot catalog leaf
    let fallback_leaf = writer.manifest().roots.snapshots.unwrap().address;
    writer.pin_current().unwrap(); // g3 new catalog leaf
    let latest_leaf = writer.manifest().roots.snapshots.unwrap().address;
    assert_ne!(fallback_leaf, latest_leaf);
    let mut disk = writer.into_device().power_loss();
    disk.corrupt_stable_for_testing(fallback_leaf as usize, 75, 0xed);
    // Latest g3 is well-formed and remains readable. The older g2
    // candidate may own unknown blocks so the writer must refuse reuse.
    let reader = ReadOnlyV10::open(disk.power_loss()).unwrap();
    assert_eq!(reader.root_info().generation, 3);
    assert!(reader.into_device().block_count() >= 128);
    // Restore a second corrupted copy of the image through fresh fixture.
    let mut writer = VirginSnapshotWriter::format(CrashDevice::new(128), VID).unwrap();
    writer.pin_current().unwrap();
    let old = writer.manifest().roots.snapshots.unwrap().address;
    writer.pin_current().unwrap();
    let mut disk = writer.into_device().power_loss();
    disk.corrupt_stable_for_testing(old as usize, 75, 0xed);
    assert!(matches!(
        VirginSnapshotWriter::open(disk.power_loss()),
        Err(Error::CorruptTree)
    ));
}

#[test]
fn genuine_out_of_space_does_not_publish_partial_snapshot_catalog() {
    let mut writer = VirginSnapshotWriter::format(CrashDevice::new(9), VID).unwrap();
    writer.pin_current().unwrap(); // initial new snapshot leaf at block 7
    writer.pin_current().unwrap(); // new leaf at block 6
    writer.pin_current().unwrap(); // new leaf at block 5
    assert_eq!(writer.manifest().generation, 4);
    assert!(matches!(writer.pin_current(), Err(Error::NoSpace)));
    assert_eq!(writer.manifest().generation, 4);
    let mut reopened = VirginSnapshotWriter::open(writer.into_device().power_loss()).unwrap();
    assert_eq!(reopened.snapshots().unwrap().len(), 3);
    assert_eq!(reopened.manifest().generation, 4);
}

#[test]
fn error_during_staged_page_write_or_root_publish_is_not_a_partial_commit() {
    // Start with a valid g2 snapshot. The new edit requires one staged
    // leaf write and F0's payload/superblock writes, with sync barriers.
    for operation in 1..=6 {
        for torn_bytes in [0, 16, 120, BLOCK_SIZE] {
            let mut writer = VirginSnapshotWriter::format(CrashDevice::new(128), VID).unwrap();
            writer.pin_current().unwrap();
            let mut disk = writer.into_device().power_loss();
            disk.inject(Fault {
                operation,
                torn_bytes: Some(torn_bytes),
            })
            .unwrap();
            let mut writer = VirginSnapshotWriter::open(disk).unwrap();
            let failed = writer.mark_dropping(1);
            assert!(matches!(failed, Err(Error::InjectedCrash)));
            let disk = writer.into_device().power_loss();
            // An invalid checksummed newest candidate may force strict
            // writer quarantine; the read-only root verifier still recovers
            // either the fully old or fully new generation.
            let mut reader = ReadOnlyV10::open(disk).unwrap();
            assert!(matches!(reader.root_info().generation, 2 | 3));
            let snapshots = reader.snapshot_descriptors().unwrap();
            assert_eq!(snapshots.len(), 1);
            let correct = if reader.root_info().generation == 3 {
                SnapshotState::Dropping
            } else {
                SnapshotState::Pinned
            };
            assert_eq!(snapshots[0].state, correct);
        }
    }
}

#[test]
fn rejects_v9_images_and_nonempty_v10_namespaces_without_mutation() {
    use bfs_ng_storage::FileSystem;
    let v9 = FileSystem::format(CrashDevice::new(128), VID).unwrap();
    assert!(VirginSnapshotWriter::open(v9.into_device().power_loss()).is_err());
    // Nonempty fs namespace layouts await F5.1, and can never be treated
    // as a snapshot-only volume simply because their root CRC verifies.
    let writer = VirginSnapshotWriter::format(CrashDevice::new(128), VID).unwrap();
    let mut disk = writer.into_device().power_loss();
    let mut root = [0; BLOCK_SIZE];
    let fs_addr = 127;
    disk.read_block(fs_addr, &mut root).unwrap();
    root[100] = 1;
    disk.write_block(fs_addr, &root).unwrap();
    disk.sync().unwrap();
    assert!(VirginSnapshotWriter::open(disk.power_loss()).is_err());
}

#[test]
fn zeroed_historical_root_is_never_mistaken_for_unallocated_virgin_space() {
    let mut writer = VirginSnapshotWriter::format(CrashDevice::new(128), VID).unwrap();
    writer.pin_current().unwrap();
    let old_fs = writer.manifest().roots.fs.address;
    let mut disk = writer.into_device().power_loss();
    for offset in 0..BLOCK_SIZE {
        disk.corrupt_stable_for_testing(old_fs as usize, offset, 0);
    }
    assert!(VirginSnapshotWriter::open(disk.power_loss()).is_err());
}

#[test]
fn read_only_or_plain_f0_open_does_not_activate_v10_writer_or_allocator() {
    let mut writer = VirginSnapshotWriter::format(CrashDevice::new(128), VID).unwrap();
    writer.pin_current().unwrap();
    let disk = writer.into_device().power_loss();
    let mut f0 = Volume::open(disk).unwrap();
    assert!(f0.read_root().unwrap().is_some());
    let mut reader = ReadOnlyV10::open(f0.into_device()).unwrap();
    assert_eq!(reader.snapshot_descriptors().unwrap().len(), 1);
}
