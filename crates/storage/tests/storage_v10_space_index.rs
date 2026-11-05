use bfs_ng_storage::experimental_v10::{
    DeadRun, FreeRun, SpaceDomain, TreeId, dead_item, free_item,
    read_space_tree, retire_step, stage_space_delta,
};
use bfs_ng_storage::{BlockDevice, CrashDevice, Error};
use std::collections::{BTreeMap, BTreeSet};

const VID: u128 = 0xfc42_3189_1aaa_229d;

#[test]
fn persistent_free_tree_path_copies_only_changed_leaves() {
    let mut disk = CrashDevice::new(9000);
    let mut rows = BTreeMap::new();
    for i in 0..500_u64 {
        let r = FreeRun { start: 100 + i * 3, blocks: 1, eligible_after: 7 };
        let (key, value) = free_item(r, 9000).unwrap();
        rows.insert(key, Some(value));
    }
    let first_slots: Vec<_> = (0..40).map(|i| 8999 - i).collect();
    let first = stage_space_delta(
        &mut disk, TreeId::Free, None, &rows,
        &first_slots, VID, 8, 9000,
    ).unwrap();
    assert!(first.root.unwrap().level > 0);
    let old_root = first.root;
    let old_low = *first_slots.get(first.consumed_reservations - 1).unwrap();
    for (block, bytes) in first.staged_pages {
        disk.write_block(block, &bytes).unwrap();
    }
    disk.sync().unwrap();
    let before = read_space_tree(&mut disk, TreeId::Free, old_root, VID, 8).unwrap();
    assert_eq!(before.free.len(), 500);
    let mut delta = BTreeMap::new();
    for start in [100_u64, 160_u64, 1500_u64] {
        let r = FreeRun { start, blocks: 1, eligible_after: 9 };
        let (key, value) = free_item(r, 9000).unwrap();
        delta.insert(key, Some(value));
    }
    // Delete one interior item and insert an above-the-last-key run.
    delta.insert(103_u64.to_be_bytes().to_vec(), None);
    let (key, value) = free_item(
        FreeRun { start: 2000, blocks: 1, eligible_after: 10 }, 9000,
    ).unwrap();
    delta.insert(key, Some(value));
    let slots: Vec<_> = (1..=30).map(|i| old_low - i).collect();
    let edited = stage_space_delta(
        &mut disk, TreeId::Free, old_root, &delta,
        &slots, VID, 9, old_low,
    ).unwrap();
    assert!(edited.consumed_reservations < 12, "must be CoW paths, not full rebuild");
    for (block, bytes) in edited.staged_pages {
        disk.write_block(block, &bytes).unwrap();
    }
    disk.sync().unwrap();
    let new = read_space_tree(&mut disk, TreeId::Free, edited.root, VID, 9).unwrap();
    assert_eq!(new.free.len(), 500);
    assert_eq!(new.free.get(&100).unwrap().eligible_after, 9);
    assert_eq!(new.free.get(&1500).unwrap().eligible_after, 9);
    assert!(new.free.contains_key(&2000));
    assert!(!new.free.contains_key(&103));
    assert!(!new.pages.is_disjoint(&before.pages), "untouched subtrees remain shared");
    assert_eq!(read_space_tree(&mut disk, TreeId::Free, old_root, VID, 9).unwrap().free, before.free);
}

#[test]
fn dead_tree_keeps_snapshot_and_global_domains_distinct() {
    let mut disk = CrashDevice::new(150);
    let mut records = BTreeMap::new();
    let snap = DeadRun {
        dead_generation: 4, start: 20, blocks: 2, eligible_after: 6,
        domain: SpaceDomain::Snapshot,
    };
    let global = DeadRun {
        dead_generation: 4, start: 27, blocks: 3, eligible_after: 6,
        domain: SpaceDomain::Global,
    };
    for run in [snap, global] {
        let (key, value) = dead_item(run, 150).unwrap();
        records.insert(key, Some(value));
    }
    let stage = stage_space_delta(
        &mut disk, TreeId::DeadList, None, &records,
        &[145, 144, 143], VID, 5, 150,
    ).unwrap();
    let root = stage.root;
    for (block, bytes) in stage.staged_pages {
        disk.write_block(block, &bytes).unwrap();
    }
    disk.sync().unwrap();
    let observed = read_space_tree(&mut disk, TreeId::DeadList, root, VID, 5).unwrap();
    assert_eq!(observed.dead.len(), 2);
    let entries = observed.dead;
    assert_eq!(entries.get(&(4, 20)).unwrap().domain, SpaceDomain::Snapshot);
    assert_eq!(entries.get(&(4, 27)).unwrap().domain, SpaceDomain::Global);
    let active = BTreeSet::from([20, 21]);
    let before = retire_step(&BTreeMap::new(), &entries, &active, 5, 32).unwrap();
    assert_eq!(before.released, 0);
    let after = retire_step(&before.free, &before.dead, &active, 6, 1).unwrap();
    assert_eq!(after.released, 0, "protected snapshot run must remain pending");
    assert_eq!(after.dead.len(), 2);
    let unblocked = retire_step(&after.free, &after.dead, &active, 6, 32).unwrap();
    assert_eq!(unblocked.released, 3);
    assert_eq!(unblocked.dead.len(), 1);
    assert_eq!(unblocked.free.get(&27).unwrap().eligible_after, 6);
    let final_step = retire_step(&unblocked.free, &unblocked.dead, &BTreeSet::new(), 7, 32).unwrap();
    assert_eq!(final_step.released, 2);
    assert!(final_step.dead.is_empty());
}

#[test]
fn run_epochs_remain_absolute_and_invalid_geometry_fails_closed() {
    assert!(free_item(FreeRun { start: 5, blocks: 1, eligible_after: 1 }, 40).is_err());
    assert!(dead_item(DeadRun {
        dead_generation: 8, start: 9, blocks: 1, eligible_after: 9,
        domain: SpaceDomain::Global,
    }, 40).is_err());
    let mut disk = CrashDevice::new(100);
    let r = FreeRun { start: 55, blocks: 5, eligible_after: 13 };
    let (key, value) = free_item(r, 100).unwrap();
    let stage = stage_space_delta(
        &mut disk, TreeId::Free, None, &[(key, Some(value))].into(),
        &[95], VID, 15, 100,
    ).unwrap();
    for (block, bytes) in stage.staged_pages {
        disk.write_block(block, &bytes).unwrap();
    }
    disk.sync().unwrap();
    assert_eq!(
        read_space_tree(&mut disk, TreeId::Free, stage.root, VID, 15)
            .unwrap().free.get(&55).unwrap().eligible_after, 13
    );
    let bad = stage_space_delta(
        &mut disk, TreeId::Free, stage.root,
        &[(80_u64.to_be_bytes().to_vec(), None)].into(),
        &[94], VID, 16, 95,
    );
    assert!(matches!(bad, Err(Error::CorruptTree)));
}

#[test]
fn free_tree_rejects_overlapping_or_misencoded_ranges_with_valid_page_crc() {
    let mut disk = CrashDevice::new(150);
    let a = free_item(FreeRun { start: 20, blocks: 8, eligible_after: 4 }, 150).unwrap();
    let b = free_item(FreeRun { start: 26, blocks: 7, eligible_after: 5 }, 150).unwrap();
    let rows: BTreeMap<Vec<u8>, Option<Vec<u8>>> =
        [(a.0, Some(a.1)), (b.0, Some(b.1))].into();
    let stage = stage_space_delta(
        &mut disk, TreeId::Free, None, &rows, &[140], VID, 6, 150,
    ).unwrap();
    for (block, bytes) in stage.staged_pages {
        disk.write_block(block, &bytes).unwrap();
    }
    disk.sync().unwrap();
    assert!(read_space_tree(&mut disk, TreeId::Free, stage.root, VID, 6).is_err());
}

#[test]
fn bounded_retirement_rejects_larger_steps_and_never_releases_owners() {
    let d = DeadRun {
        dead_generation: 2, start: 50, blocks: 4,
        eligible_after: 4, domain: SpaceDomain::Snapshot,
    };
    let pending = BTreeMap::from([((2, 50), d)]);
    assert!(retire_step(
        &BTreeMap::new(), &pending, &BTreeSet::new(), 4, 33,
    ).is_err());
    let owners = BTreeSet::from([51]);
    let blocked = retire_step(&BTreeMap::new(), &pending, &owners, 50, 32).unwrap();
    assert_eq!(blocked.released, 0);
    assert_eq!(blocked.dead, pending);
}
