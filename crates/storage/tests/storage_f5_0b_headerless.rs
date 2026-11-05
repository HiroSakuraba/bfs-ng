use bfs_ng_storage::experimental_v10::{
    HeaderlessDataWriter, ReadOnlyV10, SnapshotState, read_checksum_tree, stage_checksum_tree,
};
use bfs_ng_storage::{BLOCK_SIZE, BlockDevice, CrashDevice, Error, Fault};
use std::collections::BTreeMap;

const VID: u128 = 0x792a_cda3_6105_4b91;

#[test]
fn snapshot_retains_old_headerless_data_and_global_checksum_after_rotations() {
    let mut writer = HeaderlessDataWriter::format(CrashDevice::new(384), VID).unwrap();
    assert_eq!(writer.read_current().unwrap(), b"");
    let mut old = b"some original file contents".to_vec();
    old.extend(vec![0x51; 145]);
    writer.write_current(&old).unwrap(); // generation 2
    writer.pin_current().unwrap(); // pin generation 2 in generation 3
    let mut latest = b"replacement".to_vec();
    for _ in 0..12 {
        writer.write_current(&latest).unwrap();
        latest.push(b'X');
    }
    assert_eq!(writer.read_snapshot(2).unwrap(), old);
    assert_eq!(writer.read_current().unwrap(), latest[..latest.len() - 1]);
    let cs_before = writer.manifest().roots.checksum.unwrap();
    writer = HeaderlessDataWriter::open(writer.into_device().power_loss()).unwrap();
    assert_eq!(writer.read_snapshot(2).unwrap(), old);
    assert_eq!(writer.manifest().roots.checksum, Some(cs_before));
    writer.mark_dropping(2).unwrap();
    let pinned = writer.snapshots().unwrap();
    assert_eq!(pinned.len(), 1);
    assert_eq!(pinned[0].state, SnapshotState::Dropping);
    // Drop-mark is *not* a reclamation event; historical data/checksum remain.
    assert_eq!(writer.read_snapshot(2).unwrap(), old);
    writer = HeaderlessDataWriter::open(writer.into_device().power_loss()).unwrap();
    assert_eq!(writer.read_snapshot(2).unwrap(), old);
}

#[test]
fn unpinned_overwrite_drops_obsolete_checksum_only_from_current_manifest() {
    let mut writer = HeaderlessDataWriter::format(CrashDevice::new(128), VID).unwrap();
    writer.write_current(b"old").unwrap();
    let previous = writer.manifest().roots.checksum.unwrap();
    writer.write_current(b"new").unwrap();
    assert_ne!(
        writer.manifest().roots.checksum.unwrap().address,
        previous.address
    );
    // The *fallback* F0 manifest still has its own historical csum root,
    // even though the selected newest manifest no longer lists old data.
    assert_eq!(writer.read_current().unwrap(), b"new");
    assert!(matches!(
        writer.read_snapshot(2),
        Err(Error::InvalidSnapshot)
    ));
    assert_eq!(
        HeaderlessDataWriter::open(writer.into_device().power_loss())
            .unwrap()
            .read_current()
            .unwrap(),
        b"new"
    );
}

#[test]
fn headerless_data_covers_exactly_four_kib_and_rejects_too_large_writes() {
    let mut writer = HeaderlessDataWriter::format(CrashDevice::new(80), VID).unwrap();
    let full = vec![0x6d; BLOCK_SIZE];
    writer.write_current(&full).unwrap();
    assert_eq!(writer.read_current().unwrap(), full);
    let old_g = writer.manifest().generation;
    assert!(matches!(
        writer.write_current(&[1; BLOCK_SIZE + 1]),
        Err(Error::TooLarge)
    ));
    assert_eq!(writer.manifest().generation, old_g);
    writer.write_current(b"tiny").unwrap();
    assert_eq!(writer.read_current().unwrap(), b"tiny");
    writer.write_current(b"").unwrap();
    assert_eq!(writer.read_current().unwrap(), b"");
    assert!(writer.manifest().roots.checksum.is_none());
}

#[test]
fn corrupt_data_detected_on_strict_open_but_direct_root_open_is_lazy() {
    let mut writer = HeaderlessDataWriter::format(CrashDevice::new(96), VID).unwrap();
    writer.write_current(b"checksum protected").unwrap();
    let mut disk = writer.into_device().power_loss();
    let mut raw = [0; BLOCK_SIZE];
    let data_block = disk.block_count() - 2; // g1 fs at -1, g2 data at -2
    disk.read_block(data_block, &mut raw).unwrap();
    assert_eq!(raw[..8], *b"checksum");
    disk.corrupt_stable_for_testing(data_block as usize, 4, 0xaa);
    // Opening checks direct metadata-root pages, not arbitrary data blocks.
    let reader = ReadOnlyV10::open(disk.power_loss()).unwrap();
    assert_eq!(reader.root_info().generation, 2);
    assert!(HeaderlessDataWriter::open(reader.into_device()).is_err());
}

#[test]
fn strict_writer_refuses_corrupt_fallback_checksum_even_if_latest_valid() {
    let mut writer = HeaderlessDataWriter::format(CrashDevice::new(128), VID).unwrap();
    writer.write_current(b"one").unwrap();
    let fallback_checksum = writer.manifest().roots.checksum.unwrap().address;
    writer.write_current(b"two").unwrap();
    let mut disk = writer.into_device().power_loss();
    disk.corrupt_stable_for_testing(fallback_checksum as usize, 74, 0x39);
    // An unaffected latest generation remains lazily readable, but a
    // damaged checksummed fallback is not a safe source of allocation.
    let readonly = ReadOnlyV10::open(disk.power_loss()).unwrap();
    assert_eq!(readonly.root_info().generation, 3);
    assert!(HeaderlessDataWriter::open(readonly.into_device()).is_err());
}

#[test]
fn nonzero_orphan_is_skipped_never_used_for_file_data_or_metadata() {
    let writer = HeaderlessDataWriter::format(CrashDevice::new(96), VID).unwrap();
    let water = writer.manifest().high_water;
    let mut disk = writer.into_device().power_loss();
    let mut orphan = [0_u8; BLOCK_SIZE];
    orphan[..8].copy_from_slice(b"ORPHAN!!");
    disk.write_block(water - 1, &orphan).unwrap();
    disk.sync().unwrap();
    let mut writer = HeaderlessDataWriter::open(disk.power_loss()).unwrap();
    writer.write_current(b"live data").unwrap();
    assert!(writer.manifest().high_water < water - 1);
    let mut device = writer.into_device().power_loss();
    let mut check = [0; BLOCK_SIZE];
    device.read_block(water - 1, &mut check).unwrap();
    assert_eq!(check, orphan);
    assert_eq!(
        HeaderlessDataWriter::open(device.power_loss())
            .unwrap()
            .read_current()
            .unwrap(),
        b"live data"
    );
}

#[test]
fn crashes_never_publish_mixed_fs_data_checksum_versions() {
    for operation in 1..=8 {
        for torn_bytes in [0, 16, 124, BLOCK_SIZE] {
            let mut writer = HeaderlessDataWriter::format(CrashDevice::new(128), VID).unwrap();
            writer.write_current(b"old value").unwrap();
            let mut disk = writer.into_device().power_loss();
            disk.inject(Fault {
                operation,
                torn_bytes: Some(torn_bytes),
            })
            .unwrap();
            let mut writer = HeaderlessDataWriter::open(disk).unwrap();
            assert!(matches!(
                writer.write_current(b"new value"),
                Err(Error::InjectedCrash)
            ));
            let disk = writer.into_device().power_loss();
            // Fail-closed strict recovery may reject a damaged *fallback*;
            // the read-only selector always chooses one complete generation.
            let lazy = ReadOnlyV10::open(disk).unwrap();
            assert!(matches!(lazy.root_info().generation, 2 | 3));
            let generation = lazy.root_info().generation;
            // Current strict writer can only reopen when every fallback is
            // intact, but whenever it does, read data must match generation.
            if let Ok(mut recovered) = HeaderlessDataWriter::open(lazy.into_device()) {
                let contents = recovered.read_current().unwrap();
                assert_eq!(
                    contents,
                    if generation == 2 {
                        b"old value"
                    } else {
                        b"new value"
                    }
                );
            }
        }
    }
}

#[test]
fn physical_checksum_tree_splits_into_authenticated_branches() {
    let mut disk = CrashDevice::new(3000);
    let checksums: BTreeMap<u64, u32> = (100..530).map(|n| (n, (n as u32) * 17)).collect();
    let reservations: Vec<u64> = (0..15).map(|n| 2999 - n).collect();
    let staged = stage_checksum_tree(&checksums, &reservations, VID, 5, 3000, 3000).unwrap();
    assert!(staged.root.unwrap().level > 0);
    for (addr, bytes) in staged.staged_pages {
        disk.write_block(addr, &bytes).unwrap();
    }
    disk.sync().unwrap();
    let scan = read_checksum_tree(&mut disk, staged.root, VID, 5).unwrap();
    assert_eq!(scan.checksums, checksums);
    assert!(scan.metadata_pages.len() > 2);
    let victim = *scan.metadata_pages.iter().next().unwrap();
    disk.corrupt_stable_for_testing(victim as usize, 70, 0xd2);
    assert!(read_checksum_tree(&mut disk.power_loss(), staged.root, VID, 5).is_err());
}

#[test]
fn no_space_never_creates_a_partial_file_publication() {
    let mut writer = HeaderlessDataWriter::format(CrashDevice::new(11), VID).unwrap();
    writer.write_current(b"a").unwrap();
    assert_eq!(writer.manifest().generation, 2);
    assert!(matches!(writer.write_current(b"b"), Err(Error::NoSpace)));
    assert_eq!(writer.manifest().generation, 2);
    let mut reopened = HeaderlessDataWriter::open(writer.into_device().power_loss()).unwrap();
    assert_eq!(reopened.read_current().unwrap(), b"a");
}
