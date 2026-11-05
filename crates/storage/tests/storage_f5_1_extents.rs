use bfs_ng_storage::experimental_v10::{
    BlockRef, ExtentDataWriter, FileExtents, Item, MAX_FILE_BYTES, Page, ReadOnlyV10,
    SnapshotState, TreeId, read_page,
};
use bfs_ng_storage::{BLOCK_SIZE, BlockDevice, CrashDevice, Error, Fault};

const VID: u128 = 0xad28_c47b_1832_9942;

#[test]
fn unaligned_cross_block_overwrite_retains_unmodified_blocks_and_snapshot() {
    let mut writer = ExtentDataWriter::format(CrashDevice::new(1200), VID).unwrap();
    let original = vec![0x31; 5 * BLOCK_SIZE + 37];
    writer.write_at(0, &original).unwrap();
    let snap_gen = writer.manifest().generation;
    writer.pin_current().unwrap();
    let patched = vec![0xab; 712];
    writer
        .write_at((2 * BLOCK_SIZE - 123) as u64, &patched)
        .unwrap();
    let mut expected = original.clone();
    expected[2 * BLOCK_SIZE - 123..2 * BLOCK_SIZE - 123 + patched.len()].copy_from_slice(&patched);
    assert_eq!(writer.read_current().unwrap(), expected);
    assert_eq!(
        writer
            .read_snapshot_at(snap_gen, 0, original.len())
            .unwrap(),
        original
    );
    writer = ExtentDataWriter::open(writer.into_device().power_loss()).unwrap();
    assert_eq!(writer.read_current().unwrap(), expected);
    assert_eq!(
        writer
            .read_snapshot_at(snap_gen, 0, original.len())
            .unwrap(),
        original
    );
    // Snapshot drop mark retains both its historical FS root and checksums.
    writer.mark_dropping(snap_gen).unwrap();
    assert_eq!(
        writer.snapshots().unwrap()[0].state,
        SnapshotState::Dropping
    );
    assert_eq!(
        writer
            .read_snapshot_at(snap_gen, 0, original.len())
            .unwrap(),
        original
    );
}

#[test]
fn sparse_holes_truncate_and_zero_tail_are_canonical() {
    let mut writer = ExtentDataWriter::format(CrashDevice::new(500), VID).unwrap();
    writer
        .write_at(4 * BLOCK_SIZE as u64 + 27, b"payload")
        .unwrap();
    let mut expected = vec![0; 4 * BLOCK_SIZE + 34];
    expected[4 * BLOCK_SIZE + 27..].copy_from_slice(b"payload");
    assert_eq!(writer.read_current().unwrap(), expected);
    assert_eq!(
        writer
            .read_at(3 * BLOCK_SIZE as u64, BLOCK_SIZE + 40)
            .unwrap(),
        expected[3 * BLOCK_SIZE..].to_vec()
    );
    writer.write_at(2, b"head").unwrap();
    expected[2..6].copy_from_slice(b"head");
    assert_eq!(writer.read_current().unwrap(), expected);
    let snap = writer.manifest().generation;
    writer.pin_current().unwrap();
    writer.truncate(3).unwrap();
    assert_eq!(writer.read_current().unwrap(), [0, 0, b'h']);
    assert_eq!(
        writer.read_snapshot_at(snap, 0, expected.len()).unwrap(),
        expected
    );
    writer.truncate(2 * BLOCK_SIZE as u64 + 11).unwrap();
    let extended = writer.read_current().unwrap();
    assert_eq!(&extended[..3], &[0, 0, b'h']);
    assert!(extended[3..].iter().all(|&b| b == 0));
    writer = ExtentDataWriter::open(writer.into_device().power_loss()).unwrap();
    assert_eq!(writer.read_current().unwrap(), extended);
    writer.truncate(0).unwrap();
    assert!(writer.read_current().unwrap().is_empty());
    assert_eq!(writer.file_size().unwrap(), 0);
}

#[test]
fn physical_extents_coalesce_and_detect_aliases_and_bad_births() {
    let mut writer = ExtentDataWriter::format(CrashDevice::new(600), VID).unwrap();
    writer.write_at(0, &vec![0x63; 4 * BLOCK_SIZE]).unwrap();
    let root = writer.manifest().roots.fs;
    let generation = writer.manifest().generation;
    let mut disk = writer.into_device().power_loss();
    let page = read_page(&mut disk, root, TreeId::Fs, VID, generation).unwrap();
    let file = FileExtents::decode(&page, disk.block_count()).unwrap();
    assert_eq!(file.size, (4 * BLOCK_SIZE) as u64);
    assert_eq!(file.blocks.len(), 4);
    // Descending allocation is assigned in reversed logical order, allowing
    // a physically adjacent run to compress to one extent item.
    assert_eq!(page.items.len(), 2);
    assert_eq!(
        file.encode_items(generation, disk.block_count()).unwrap(),
        page.items
    );

    let mut bad = file.clone();
    bad.blocks.insert(1, *bad.blocks.get(&0).unwrap());
    assert!(bad.encode_items(generation, disk.block_count()).is_err());
    bad = file.clone();
    bad.blocks.insert(
        0,
        BlockRef {
            physical: 5,
            birth: generation + 1,
        },
    );
    assert!(bad.encode_items(generation, disk.block_count()).is_err());
    bad = file;
    bad.size = MAX_FILE_BYTES + 1;
    assert!(matches!(
        bad.encode_items(generation, disk.block_count()),
        Err(Error::TooLarge)
    ));
    assert!(
        FileExtents::decode(
            &Page {
                tree: TreeId::Fs,
                level: 1,
                ..page
            },
            disk.block_count()
        )
        .is_err()
    );
}

#[test]
fn exact_file_limit_rejects_out_of_bounds_without_publishing() {
    let mut writer = ExtentDataWriter::format(CrashDevice::new(1000), VID).unwrap();
    let size = MAX_FILE_BYTES as usize;
    writer.write_at(0, &vec![0x67; size]).unwrap();
    assert_eq!(writer.file_size().unwrap(), MAX_FILE_BYTES);
    assert_eq!(
        writer.read_at(MAX_FILE_BYTES - 16, 64).unwrap(),
        vec![0x67; 16]
    );
    let generation = writer.manifest().generation;
    assert!(matches!(
        writer.write_at(MAX_FILE_BYTES, b"x"),
        Err(Error::TooLarge)
    ));
    assert!(matches!(
        writer.truncate(MAX_FILE_BYTES + 1),
        Err(Error::TooLarge)
    ));
    assert_eq!(writer.manifest().generation, generation);
    assert_eq!(writer.read_at(MAX_FILE_BYTES, 5).unwrap(), Vec::<u8>::new());
}

#[test]
fn deterministic_random_writes_and_truncates_match_byte_oracle_after_reopen() {
    let mut writer = ExtentDataWriter::format(CrashDevice::new(2500), VID).unwrap();
    let mut model = Vec::<u8>::new();
    let mut seed = 0x8e42_9271_u64;
    for i in 0..64 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        if i % 7 == 0 {
            let size = (seed as usize % (12 * BLOCK_SIZE)) as u64;
            writer.truncate(size).unwrap();
            model.resize(size as usize, 0);
        } else {
            let start = seed as usize % (11 * BLOCK_SIZE);
            let len = ((seed >> 11) as usize % 2500) + 1;
            let data: Vec<u8> = (0..len).map(|n| (n as u8) ^ (i as u8)).collect();
            writer.write_at(start as u64, &data).unwrap();
            if model.len() < start + len {
                model.resize(start + len, 0);
            }
            model[start..start + len].copy_from_slice(&data);
        }
        assert_eq!(writer.read_current().unwrap(), model, "step {i}");
        if i % 8 == 0 {
            writer = ExtentDataWriter::open(writer.into_device().power_loss()).unwrap();
            assert_eq!(writer.read_current().unwrap(), model);
        }
    }
}

#[test]
fn corrupted_data_is_detected_and_writer_refuses_unknown_fallback_ownership() {
    let mut writer = ExtentDataWriter::format(CrashDevice::new(400), VID).unwrap();
    writer.write_at(0, &vec![0x47; 3 * BLOCK_SIZE]).unwrap();
    let pointer = writer.manifest().roots.fs;
    let generation = writer.manifest().generation;
    let mut disk = writer.into_device().power_loss();
    let root = read_page(&mut disk, pointer, TreeId::Fs, VID, generation).unwrap();
    let file = FileExtents::decode(&root, disk.block_count()).unwrap();
    let physical = file.blocks.get(&1).unwrap().physical;
    disk.corrupt_stable_for_testing(physical as usize, 21, 0);
    let reader = ReadOnlyV10::open(disk.power_loss()).unwrap();
    assert_eq!(reader.root_info().generation, generation);
    assert!(ExtentDataWriter::open(reader.into_device()).is_err());
}

#[test]
fn partial_write_torn_commits_have_only_old_or_new_full_file() {
    for operation in 1..=9 {
        for torn_bytes in [0, 32, 124, BLOCK_SIZE] {
            let mut writer = ExtentDataWriter::format(CrashDevice::new(400), VID).unwrap();
            let mut old = vec![0x22; 3 * BLOCK_SIZE];
            writer.write_at(0, &old).unwrap();
            let mut disk = writer.into_device().power_loss();
            disk.inject(Fault {
                operation,
                torn_bytes: Some(torn_bytes),
            })
            .unwrap();
            writer = ExtentDataWriter::open(disk).unwrap();
            let written = vec![0xa9; 701];
            let failed = writer.write_at(BLOCK_SIZE as u64 - 150, &written);
            assert!(matches!(failed, Err(Error::InjectedCrash)));
            let disk = writer.into_device().power_loss();
            let root = ReadOnlyV10::open(disk).unwrap();
            let selected = root.root_info().generation;
            assert!(matches!(selected, 2 | 3));
            let mut candidate = ExtentDataWriter::open(root.into_device());
            if let Ok(ref mut reopened) = candidate {
                let got = reopened.read_current().unwrap();
                if selected == 3 {
                    old[BLOCK_SIZE - 150..BLOCK_SIZE - 150 + written.len()]
                        .copy_from_slice(&written);
                }
                assert_eq!(got, old);
            }
        }
    }
}

#[test]
fn near_full_failure_does_not_publish_mutated_file() {
    let mut writer = ExtentDataWriter::format(CrashDevice::new(9), VID).unwrap();
    writer.write_at(0, b"first").unwrap();
    let generation = writer.manifest().generation;
    assert!(matches!(writer.write_at(0, b"second"), Err(Error::NoSpace)));
    assert_eq!(writer.manifest().generation, generation);
    let mut reopened = ExtentDataWriter::open(writer.into_device().power_loss()).unwrap();
    assert_eq!(reopened.read_current().unwrap(), b"first");
}

#[test]
fn malformed_extent_overlaps_are_rejected_even_with_valid_page_crc() {
    let base = FileExtents {
        size: (3 * BLOCK_SIZE) as u64,
        blocks: [
            (
                0,
                BlockRef {
                    physical: 100,
                    birth: 2,
                },
            ),
            (
                1,
                BlockRef {
                    physical: 101,
                    birth: 2,
                },
            ),
        ]
        .into(),
    };
    let mut items = base.encode_items(2, 500).unwrap();
    let mut fake = vec![0_u8; 24];
    fake[..8].copy_from_slice(&101_u64.to_le_bytes());
    fake[8..12].copy_from_slice(&2_u32.to_le_bytes());
    fake[16..24].copy_from_slice(&2_u64.to_le_bytes());
    items.insert(
        1,
        Item {
            key: 1_u64.to_be_bytes().to_vec(),
            value: fake,
        },
    );
    let page = Page {
        tree: TreeId::Fs,
        level: 0,
        birth: 2,
        address: 490,
        volume_id: VID,
        items,
    };
    assert!(FileExtents::decode(&page, 500).is_err());
}
