use bfs_ng_storage::experimental_v10::{
    read_checksum_tree, stage_checksum_delta, stage_checksum_tree,
};
use bfs_ng_storage::{BlockDevice, CrashDevice, Error};
use std::collections::BTreeMap;

const VID: u128 = 0xbac1_0809_1337_4221;

#[test]
fn checksum_delta_copies_only_modified_leaves_and_ancestors() {
    let mut disk = CrashDevice::new(7000);
    let original: BTreeMap<u64, u32> = (100..740).map(|n| (n, (n * 19) as u32)).collect();
    let old_pages: Vec<u64> = (0..20).map(|n| 6999 - n).collect();
    let initial = stage_checksum_tree(&original, &old_pages, VID, 5, 7000, 7000).unwrap();
    assert!(initial.root.unwrap().level >= 1);
    let first_root = initial.root;
    let old_low = old_pages[initial.consumed_reservations - 1];
    for (block, bytes) in initial.staged_pages {
        disk.write_block(block, &bytes).unwrap();
    }
    disk.sync().unwrap();
    let old_scan = read_checksum_tree(&mut disk, first_root, VID, 5).unwrap();
    assert_eq!(old_scan.checksums, original);
    assert!(old_scan.metadata_pages.len() >= 4);

    let changes: BTreeMap<u64, Option<u32>> = [(250, None), (810, Some(0xface_cafe))].into();
    let slots: Vec<u64> = (1..=12).map(|n| old_low - n).collect();
    let staged =
        stage_checksum_delta(&mut disk, first_root, &changes, &slots, VID, 6, old_low).unwrap();
    assert!(
        staged.staged_pages.len() <= 5,
        "must path-copy, not rebuild"
    );
    for (block, bytes) in &staged.staged_pages {
        disk.write_block(*block, bytes).unwrap();
    }
    disk.sync().unwrap();
    let scan = read_checksum_tree(&mut disk, staged.root, VID, 6).unwrap();
    let mut expected = original;
    expected.remove(&250);
    expected.insert(810, 0xface_cafe);
    assert_eq!(scan.checksums, expected);
    assert!(
        !scan.metadata_pages.is_disjoint(&old_scan.metadata_pages),
        "unmodified leaves must retain physical identity"
    );
}

#[test]
fn checksum_delta_full_delete_and_empty_noop_are_canonical() {
    let mut disk = CrashDevice::new(100);
    let old: BTreeMap<u64, u32> = [(70, 0xaabb_ccdd)].into();
    let initial = stage_checksum_tree(&old, &[95], VID, 2, 99, 100).unwrap();
    for (block, bytes) in initial.staged_pages {
        disk.write_block(block, &bytes).unwrap();
    }
    disk.sync().unwrap();
    let changes: BTreeMap<u64, Option<u32>> = [(70, None)].into();
    let result =
        stage_checksum_delta(&mut disk, initial.root, &changes, &[94, 93], VID, 3, 95).unwrap();
    assert!(result.root.is_none());
    assert!(result.staged_pages.is_empty());
    let scan = read_checksum_tree(&mut disk, result.root, VID, 3).unwrap();
    assert!(scan.checksums.is_empty());

    let none =
        stage_checksum_delta(&mut disk, result.root, &BTreeMap::new(), &[], VID, 4, 95).unwrap();
    assert!(none.root.is_none());
    assert_eq!(none.consumed_reservations, 0);
}

#[test]
fn checksum_delta_rejects_missing_delete_and_inplace_rechecksum() {
    let mut disk = CrashDevice::new(100);
    let old: BTreeMap<u64, u32> = [(70, 1234)].into();
    let initial = stage_checksum_tree(&old, &[95], VID, 2, 99, 100).unwrap();
    for (block, bytes) in initial.staged_pages {
        disk.write_block(block, &bytes).unwrap();
    }
    disk.sync().unwrap();

    let missing: BTreeMap<u64, Option<u32>> = [(71, None)].into();
    assert!(matches!(
        stage_checksum_delta(&mut disk, initial.root, &missing, &[94], VID, 3, 95,),
        Err(Error::CorruptTree)
    ));

    let overwrite: BTreeMap<u64, Option<u32>> = [(70, Some(4321))].into();
    assert!(matches!(
        stage_checksum_delta(&mut disk, initial.root, &overwrite, &[94], VID, 3, 95,),
        Err(Error::CorruptTree)
    ));

    let insert: BTreeMap<u64, Option<u32>> = [(71, Some(123))].into();
    assert!(matches!(
        stage_checksum_delta(&mut disk, initial.root, &insert, &[], VID, 3, 95,),
        Err(Error::NoSpace)
    ));
}
