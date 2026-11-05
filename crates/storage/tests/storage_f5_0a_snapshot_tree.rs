use bfs_ng_storage::experimental_v10::{
    BlockPointer, CatalogEdit, Item, Manifest, Page, ReadOnlyV10, SnapshotDescriptor,
    SnapshotState, TreeId, TreeRoots, plan_snapshot_edit, read_page, read_snapshot_tree,
};
use bfs_ng_storage::{BLOCK_SIZE, BlockDevice, CrashDevice, Error, Fault, Result, Volume};

const VID: u128 = 0x5432_76aa_8943_1234;
const BLOCKS: u64 = 2_048;

fn fs_page(address: u64) -> Page {
    Page {
        tree: TreeId::Fs,
        level: 0,
        birth: 1,
        address,
        volume_id: VID,
        items: vec![Item {
            key: b"inode".to_vec(),
            value: b"fixed".to_vec(),
        }],
    }
}
fn descriptor(generation: u64, fs: BlockPointer) -> SnapshotDescriptor {
    SnapshotDescriptor {
        generation,
        fs_root: fs,
        attribute_rows_root: None,
        attribute_postings_root: None,
        attribute_schema_root: None,
        state: SnapshotState::Pinned,
    }
}
fn roots(fs: BlockPointer, snaps: Option<BlockPointer>) -> TreeRoots {
    TreeRoots {
        fs,
        snapshots: snaps,
        checksum: None,
        free: None,
        dead_list: None,
        attribute_rows: None,
        attribute_postings: None,
        schema: None,
    }
}
fn manifest(
    generation: u64,
    high_water: u64,
    fs: BlockPointer,
    snaps: Option<BlockPointer>,
) -> Manifest {
    Manifest {
        generation,
        high_water,
        next_inode: 2,
        roots: roots(fs, snaps),
        sweep_snapshot: 0,
        sweep_key: 0,
        metadata_reserve: 0,
        inode_count: 1,
        used_blocks: 1,
        free_blocks: 0,
        pending_blocks: 0,
    }
}
fn initial() -> (Volume<CrashDevice>, Manifest, BlockPointer) {
    let formatted = Volume::format(CrashDevice::new(BLOCKS as usize), VID).unwrap();
    let mut disk = formatted.into_device();
    let fs = fs_page(BLOCKS - 1);
    disk.write_block(fs.address, &fs.encode().unwrap()).unwrap();
    disk.sync().unwrap();
    let mut volume = Volume::open(disk.power_loss()).unwrap();
    let m = manifest(1, BLOCKS - 1, fs.pointer().unwrap(), None);
    volume.commit_root(&m.encode(BLOCKS).unwrap()).unwrap();
    (volume, m, fs.pointer().unwrap())
}

/// The supplied addresses are known virgin only because the fixture begins
/// with a fresh zeroed CrashDevice and monotonically consumes its high-water
/// prefix. This helper is NOT a public allocator or a v10 filesystem writer.
fn stage_commit(
    volume: Volume<CrashDevice>,
    old: &Manifest,
    edit: CatalogEdit,
    reservations: usize,
) -> Result<(Volume<CrashDevice>, Manifest, usize)> {
    let mut disk = volume.into_device().power_loss();
    let addrs: Vec<u64> = (1..=reservations as u64)
        .map(|n| old.high_water - n)
        .collect();
    let staged = plan_snapshot_edit(
        &mut disk,
        old.roots.snapshots,
        edit,
        VID,
        old.generation + 1,
        old.high_water,
        &addrs,
    )?;
    let used = staged.consumed_reservations;
    for (address, bytes) in staged.staged_pages {
        disk.write_block(address, &bytes)?;
    }
    disk.sync()?;
    let mut next = Volume::open(disk.power_loss())?;
    let manifest = manifest(
        old.generation + 1,
        staged.high_water,
        old.roots.fs,
        Some(staged.root),
    );
    next.commit_root(&manifest.encode(BLOCKS)?)?;
    Ok((next, manifest, used))
}

#[test]
fn many_pins_cross_leaf_splits_and_survive_repeated_f0_rotations() {
    let (mut volume, mut current, fs) = initial();
    let mut saw_branch = false;
    let mut max_copied = 0;
    // The 112-byte value plus 8-byte key fills a leaf after ~31 pins.
    // Every commit stages only its changed leaf and ancestor paths.
    for g in 2..=145_u64 {
        let mut disk = volume.into_device().power_loss();
        let addrs: Vec<u64> = (1..=5).map(|n| current.high_water - n).collect();
        let plan = plan_snapshot_edit(
            &mut disk,
            current.roots.snapshots,
            CatalogEdit::Pin(descriptor(g - 1, fs)),
            VID,
            g,
            current.high_water,
            &addrs,
        )
        .unwrap();
        assert!(plan.staged_pages.len() <= 3, "unexpected full-tree rebuild");
        max_copied = max_copied.max(plan.staged_pages.len());
        for (addr, bytes) in plan.staged_pages {
            disk.write_block(addr, &bytes).unwrap();
        }
        disk.sync().unwrap();
        volume = Volume::open(disk.power_loss()).unwrap();
        current = manifest(g, plan.high_water, fs, Some(plan.root));
        volume
            .commit_root(&current.encode(BLOCKS).unwrap())
            .unwrap();
        if plan.root.level == 1 {
            saw_branch = true;
        }
        if g % 29 == 0 {
            let mut view = ReadOnlyV10::open(volume.into_device().power_loss()).unwrap();
            let ds = view.snapshot_descriptors().unwrap();
            assert_eq!(ds.len(), (g - 1) as usize);
            for (i, d) in ds.iter().enumerate() {
                assert_eq!(d.generation, (i + 1) as u64);
                assert_eq!(d.fs_root, fs);
            }
            volume = Volume::open(view.into_device()).unwrap();
        }
    }
    assert!(saw_branch);
    assert!(max_copied > 1);
    let mut disk = volume.into_device().power_loss();
    let scan = read_snapshot_tree(&mut disk, current.roots.snapshots, VID, 145).unwrap();
    assert_eq!(scan.descriptors.len(), 144);
    assert!(scan.pages_read >= 3);
    assert!(scan.pages_read < 20, "catalog must use packed B+ pages");

    // An already pinned snapshot cannot silently change historical roots.
    let duplicate = plan_snapshot_edit(
        &mut disk,
        current.roots.snapshots,
        CatalogEdit::Pin(descriptor(1, fs)),
        VID,
        146,
        current.high_water,
        &[current.high_water - 1, current.high_water - 2],
    );
    assert!(matches!(duplicate, Err(Error::AlreadyExists)));
}

#[test]
fn mark_dropping_keeps_historical_root_and_does_not_reclaim_anything() {
    let (volume, original, fs) = initial();
    let (volume, m2, count) =
        stage_commit(volume, &original, CatalogEdit::Pin(descriptor(1, fs)), 6).unwrap();
    assert_eq!(count, 1);
    let (volume, m3, copied) = stage_commit(volume, &m2, CatalogEdit::MarkDropping(1), 6).unwrap();
    assert_eq!(copied, 1);
    let mut view = ReadOnlyV10::open(volume.into_device().power_loss()).unwrap();
    let pin = view.snapshot_descriptors().unwrap();
    assert_eq!(pin.len(), 1);
    assert_eq!(pin[0].state, SnapshotState::Dropping);
    assert_eq!(pin[0].fs_root, fs);
    assert!(matches!(
        plan_snapshot_edit(
            &mut view.into_device(),
            m3.roots.snapshots,
            CatalogEdit::MarkDropping(1),
            VID,
            4,
            m3.high_water,
            &[m3.high_water - 1],
        ),
        Err(Error::InvalidSnapshot)
    ));
}

#[test]
fn insufficient_private_arena_does_not_write_or_mutate_old_tree() {
    let (mut volume, mut m, fs) = initial();
    for g in 2..=33_u64 {
        let (next, mm, _) =
            stage_commit(volume, &m, CatalogEdit::Pin(descriptor(g - 1, fs)), 5).unwrap();
        volume = next;
        m = mm;
    }
    // A split requires 2 new leaves and a branch root. One reservation fails.
    let mut disk = volume.into_device().power_loss();
    let err = plan_snapshot_edit(
        &mut disk,
        m.roots.snapshots,
        CatalogEdit::Pin(descriptor(33, fs)),
        VID,
        34,
        m.high_water,
        &[m.high_water - 1],
    );
    assert!(matches!(err, Err(Error::NoSpace)));
    let view = ReadOnlyV10::open(disk.power_loss()).unwrap();
    assert_eq!(view.root_info().generation, 33);
    assert_eq!(view.manifest().roots.snapshots, m.roots.snapshots);
}

#[test]
fn forged_deep_leaf_or_parent_separator_fails_full_scan_but_not_lazy_open() {
    let (mut volume, mut m, fs) = initial();
    for g in 2..=65_u64 {
        let (next, mm, _) =
            stage_commit(volume, &m, CatalogEdit::Pin(descriptor(g - 1, fs)), 5).unwrap();
        volume = next;
        m = mm;
    }
    let mut disk = volume.into_device().power_loss();
    let root = m.roots.snapshots.unwrap();
    assert_eq!(root.level, 1);
    let node = read_page(&mut disk, root, TreeId::Snapshots, VID, m.generation).unwrap();
    let child = BlockPointer::decode_required(&node.items[0].value).unwrap();

    // Deep corruption: F0 and the direct root still verify, but complete
    // snapshot traversal must find the damaged leaf before returning pins.
    disk.corrupt_stable_for_testing(child.address as usize, BLOCK_SIZE - 1, 0xfa);
    let mut lazy = ReadOnlyV10::open(disk.power_loss()).unwrap();
    assert_eq!(lazy.root_info().generation, 65);
    assert!(lazy.snapshot_descriptors().is_err());
}

#[test]
fn adversarial_internal_tree_with_incorrect_high_key_is_rejected() {
    let (volume, m, fs) = initial();
    let mut disk = volume.into_device().power_loss();
    let first = Page {
        tree: TreeId::Snapshots,
        level: 0,
        birth: 2,
        address: 2035,
        volume_id: VID,
        items: vec![Item {
            key: 1_u64.to_be_bytes().to_vec(),
            value: descriptor(1, fs).encode(2, BLOCKS).unwrap().to_vec(),
        }],
    };
    let second = Page {
        tree: TreeId::Snapshots,
        level: 0,
        birth: 2,
        address: 2034,
        volume_id: VID,
        items: vec![Item {
            key: 2_u64.to_be_bytes().to_vec(),
            value: descriptor(2, fs).encode(3, BLOCKS).unwrap().to_vec(),
        }],
    };
    // Note: descriptor g2 is valid in a synthetic later g3; here all
    // pointers were born at g1. Build two leaves at g3 instead.
    let mut first = first;
    let mut second = second;
    first.birth = 3;
    second.birth = 3;
    let branch = Page {
        tree: TreeId::Snapshots,
        level: 1,
        birth: 3,
        address: 2033,
        volume_id: VID,
        items: vec![
            Item {
                key: 1_u64.to_be_bytes().to_vec(),
                value: first.pointer().unwrap().encode().to_vec(),
            },
            Item {
                // Key 3 is not the maximum of its leaf (2). Both page CRC
                // and parent pointer CRC are otherwise internally valid.
                key: 3_u64.to_be_bytes().to_vec(),
                value: second.pointer().unwrap().encode().to_vec(),
            },
        ],
    };
    for page in [&first, &second, &branch] {
        disk.write_block(page.address, &page.encode().unwrap())
            .unwrap();
    }
    disk.sync().unwrap();
    assert!(read_snapshot_tree(&mut disk, Some(branch.pointer().unwrap()), VID, 3).is_err());
    // Real F0 publication still cannot be inferred as safe from this scan.
    assert_eq!(m.generation, 1);
}

#[test]
fn caller_reservations_must_be_below_watermark_and_strictly_decreasing() {
    let (volume, m, fs) = initial();
    let mut disk = volume.into_device().power_loss();
    for invalid in [
        vec![],
        vec![m.high_water],
        vec![4],
        vec![m.high_water - 1, m.high_water - 1],
        vec![m.high_water - 2, m.high_water - 1],
        vec![BLOCKS + 1],
    ] {
        assert!(matches!(
            plan_snapshot_edit(
                &mut disk,
                None,
                CatalogEdit::Pin(descriptor(1, fs)),
                VID,
                2,
                m.high_water,
                &invalid,
            ),
            Err(Error::Geometry)
        ));
    }
}

#[test]
fn nested_two_level_branch_snapshot_tree_is_checked() {
    let (volume, m, fs) = initial();
    let mut disk = volume.into_device().power_loss();
    // Synthetic three-level tree: root level 2 -> branch level 1 ->
    // leaves. Tests multi-level traversal without 3,000 commits.
    let mut leaves = Vec::new();
    for g in [1_u64, 2, 3, 4] {
        let p = Page {
            tree: TreeId::Snapshots,
            level: 0,
            birth: 5,
            address: 2020 - g,
            volume_id: VID,
            items: vec![Item {
                key: g.to_be_bytes().to_vec(),
                value: descriptor(g, fs).encode(5, BLOCKS).unwrap().to_vec(),
            }],
        };
        leaves.push(p);
    }
    let mut branches = Vec::new();
    for (i, chunk) in leaves.chunks(2).enumerate() {
        let p = Page {
            tree: TreeId::Snapshots,
            level: 1,
            birth: 5,
            address: 2010 - i as u64,
            volume_id: VID,
            items: chunk
                .iter()
                .map(|child| Item {
                    key: child.items.last().unwrap().key.clone(),
                    value: child.pointer().unwrap().encode().to_vec(),
                })
                .collect(),
        };
        branches.push(p);
    }
    let root = Page {
        tree: TreeId::Snapshots,
        level: 2,
        birth: 5,
        address: 2000,
        volume_id: VID,
        items: branches
            .iter()
            .map(|child| Item {
                key: child.items.last().unwrap().key.clone(),
                value: child.pointer().unwrap().encode().to_vec(),
            })
            .collect(),
    };
    for page in leaves.iter().chain(&branches).chain(std::iter::once(&root)) {
        disk.write_block(page.address, &page.encode().unwrap())
            .unwrap();
    }
    disk.sync().unwrap();
    let scan = read_snapshot_tree(&mut disk, Some(root.pointer().unwrap()), VID, 5).unwrap();
    assert_eq!(scan.pages_read, 7);
    assert_eq!(
        scan.descriptors
            .iter()
            .map(|d| d.generation)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    assert_eq!(m.generation, 1);
}

#[test]
fn staged_snapshot_mark_survives_f0_crash_at_each_barrier() {
    for operation in 1..=4 {
        for torn_bytes in [0, 8, 124, BLOCK_SIZE] {
            let (volume, first, fs) = initial();
            let (volume, committed, _) =
                stage_commit(volume, &first, CatalogEdit::Pin(descriptor(1, fs)), 5).unwrap();
            let mut disk = volume.into_device().power_loss();
            let pages = [
                committed.high_water - 1,
                committed.high_water - 2,
                committed.high_water - 3,
            ];
            let staged = plan_snapshot_edit(
                &mut disk,
                committed.roots.snapshots,
                CatalogEdit::MarkDropping(1),
                VID,
                3,
                committed.high_water,
                &pages,
            )
            .unwrap();
            for (address, bytes) in staged.staged_pages {
                disk.write_block(address, &bytes).unwrap();
            }
            disk.sync().unwrap();
            let mut publishing = Volume::open(disk.power_loss()).unwrap();
            let next = manifest(3, staged.high_water, fs, Some(staged.root));
            let mut device = publishing.into_device();
            device
                .inject(Fault {
                    operation,
                    torn_bytes: Some(torn_bytes),
                })
                .unwrap();
            publishing = Volume::open(device).unwrap();
            assert!(matches!(
                publishing.commit_root(&next.encode(BLOCKS).unwrap()),
                Err(Error::InjectedCrash)
            ));
            let mut recovered = ReadOnlyV10::open(publishing.into_device().power_loss()).unwrap();
            assert!(matches!(recovered.root_info().generation, 2 | 3));
            let pins = recovered.snapshot_descriptors().unwrap();
            assert_eq!(pins.len(), 1);
            assert_eq!(pins[0].fs_root, fs);
            let expected_state = if recovered.root_info().generation == 2 {
                SnapshotState::Pinned
            } else {
                SnapshotState::Dropping
            };
            assert_eq!(pins[0].state, expected_state);
        }
    }
}
