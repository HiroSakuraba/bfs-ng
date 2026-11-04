use bfs_ng_storage::{BLOCK_SIZE, CrashDevice, Error, Fault, FileDevice, FileSystem};
use std::sync::atomic::{AtomicU64, Ordering};

fn fresh() -> FileSystem<CrashDevice> {
    FileSystem::format(CrashDevice::new(192), 0x1234_5678).unwrap()
}
fn reopened(fs: FileSystem<CrashDevice>) -> FileSystem<CrashDevice> {
    FileSystem::open(fs.into_device().power_loss()).unwrap()
}
fn write(fs: &mut FileSystem<CrashDevice>, text: &[u8]) {
    let mut t = fs.begin();
    t.write_file("/state", text).unwrap();
    fs.commit(t).unwrap();
}

#[test]
fn snapshot_retains_old_file_after_replacements_and_deletion() {
    let mut fs = fresh();
    let original = b"axiom source version zero";
    write(&mut fs, original);
    let inode = fs.stat("/state").unwrap().inode;
    let mut t = fs.begin();
    assert_eq!(t.pin_snapshot().unwrap(), 1);
    t.write_file("/state", b"second").unwrap();
    fs.commit(t).unwrap();
    assert_eq!(fs.snapshots(), vec![1]);
    for i in 0..8 {
        write(&mut fs, format!("latest revision {i}").as_bytes());
    }
    let mut fs = reopened(fs);
    assert_eq!(fs.snapshots(), vec![1]);
    assert_eq!(fs.read_snapshot_file(1, "/state").unwrap(), original);
    assert_eq!(fs.stat("/state").unwrap().inode, inode);
    let mut t = fs.begin();
    t.remove_file("/state").unwrap();
    fs.commit(t).unwrap();
    assert!(matches!(fs.read_file("/state"), Err(Error::NotFound)));
    assert_eq!(fs.read_snapshot_file(1, "/state").unwrap(), original);
    let fs = reopened(fs);
    assert_eq!(fs.snapshots(), vec![1]);
}

#[test]
fn historical_snapshot_with_nested_paths_survives_reopen() {
    let mut fs = fresh();
    let mut t = fs.begin();
    t.mkdir("/dve").unwrap();
    t.write_file("/dve/material", &vec![0x41; 9000]).unwrap();
    fs.commit(t).unwrap();
    let mut t = fs.begin();
    t.pin_snapshot().unwrap();
    t.remove_file("/dve/material").unwrap();
    t.rmdir("/dve").unwrap();
    fs.commit(t).unwrap();
    let mut fs = reopened(fs);
    assert_eq!(fs.file_count(), 0);
    assert_eq!(
        fs.read_snapshot_file(1, "/dve/material").unwrap(),
        vec![0x41; 9000]
    );
    assert!(matches!(
        fs.read_snapshot_file(1, "/missing"),
        Err(Error::NotFound)
    ));
    assert!(matches!(
        fs.read_snapshot_file(99, "/dve/material"),
        Err(Error::InvalidSnapshot)
    ));
}

#[test]
fn append_only_old_blocks_become_reusable_only_after_fallback_retirement() {
    let mut fs = fresh();
    write(&mut fs, b"gen1"); // fresh data + node
    let cursor1 = fs.free_cursor();
    write(&mut fs, b"gen2"); // cannot overwrite gen1 (fallback)
    let cursor2 = fs.free_cursor();
    write(&mut fs, b"gen3"); // cannot overwrite gen2 (fallback)
    let cursor3 = fs.free_cursor();
    assert!(cursor3 < cursor2 && cursor2 < cursor1);
    // At the start of generation 4, only g2/g3 are recoverable; g1 has
    // left both slots and is no longer pinned, so its two blocks are free.
    write(&mut fs, b"gen4");
    assert_eq!(
        fs.free_cursor(),
        cursor3,
        "expected safe physical block reuse"
    );
    let mut fs = reopened(fs);
    assert_eq!(fs.read_file("/state").unwrap(), b"gen4");
}

#[test]
fn snapshot_pins_blocks_across_fallback_root_rotation() {
    let mut fs = fresh();
    write(&mut fs, b"PINNED");
    let mut t = fs.begin();
    t.pin_snapshot().unwrap();
    t.write_file("/state", b"gen2").unwrap();
    fs.commit(t).unwrap();
    write(&mut fs, b"gen3");
    let before = fs.free_cursor();
    write(&mut fs, b"gen4");
    // The generation-1 snapshot still holds its original data and tree.
    assert!(
        fs.free_cursor() < before,
        "pinned g1 blocks may not be reused"
    );
    let mut fs = reopened(fs);
    assert_eq!(fs.read_snapshot_file(1, "/state").unwrap(), b"PINNED");
    let mut drop = fs.begin();
    drop.drop_snapshot(1).unwrap();
    fs.commit(drop).unwrap();
    assert!(fs.snapshots().is_empty());
    // The previous F0 slot still contains the pin: no immediate reuse of g1.
    let mut fs = reopened(fs);
    assert!(matches!(
        fs.read_snapshot_file(1, "/state"),
        Err(Error::InvalidSnapshot)
    ));
    // Continue through root rotation; a subsequent commit can recycle
    // unreferenced blocks without threatening the current namespace.
    for i in 0..4 {
        write(&mut fs, format!("after drop {i}").as_bytes());
    }
    let mut fs = reopened(fs);
    assert_eq!(fs.read_file("/state").unwrap(), b"after drop 3");
}

#[test]
fn deterministic_crashes_during_reuse_preserve_whole_recoverable_versions() {
    // Three generations create free blocks reachable by neither live slot.
    for fault_op in 1..=7 {
        for torn in [None, Some(24), Some(124), Some(BLOCK_SIZE)] {
            let mut fs = fresh();
            write(&mut fs, b"first");
            write(&mut fs, b"second");
            write(&mut fs, b"third");
            let cursor = fs.free_cursor();
            let mut device = fs.into_device();
            device
                .inject(Fault {
                    operation: fault_op,
                    torn_bytes: torn,
                })
                .unwrap();
            let mut fs = FileSystem::open(device).unwrap();
            let mut t = fs.begin();
            t.write_file("/state", b"fourth").unwrap();
            assert!(matches!(fs.commit(t), Err(Error::InjectedCrash)));
            let mut fs = reopened(fs);
            let result = fs.read_file("/state").unwrap();
            assert!(
                result == b"third" || result == b"fourth",
                "op={fault_op} torn={torn:?}, got {result:?}"
            );
            // A failed write of the alternate slot can still persist the
            // *entire* 124-byte checksummed F0 header plus zero padding.
            // It is safe to recover that complete new generation after op 6,
            // even though the final sync (op 7) has not returned.
            if fault_op < 6 {
                assert_eq!(result, b"third");
            }
            if fault_op == 6 && torn.is_none_or(|n| n < 124) {
                assert_eq!(result, b"third");
            }
            if result == b"fourth" {
                assert_eq!(fs.free_cursor(), cursor);
            }
        }
    }
}

#[test]
fn snapshot_catalog_has_bounds_and_rejects_absent_pins() {
    let mut fs = fresh();
    let mut t = fs.begin();
    assert!(matches!(t.pin_snapshot(), Err(Error::InvalidSnapshot)));
    t.write_file("/state", b"first").unwrap();
    fs.commit(t).unwrap();
    for _ in 0..16 {
        let mut t = fs.begin();
        t.pin_snapshot().unwrap();
        fs.commit(t).unwrap();
    }
    assert_eq!(fs.snapshots().len(), 16);
    let mut t = fs.begin();
    assert!(matches!(t.pin_snapshot(), Err(Error::NoSpace)));
    assert!(matches!(t.drop_snapshot(500), Err(Error::InvalidSnapshot)));
    assert_eq!(reopened(fs).snapshots().len(), 16);
}

#[test]
fn repeated_pins_are_idempotent_and_do_not_duplicate_roots() {
    let mut fs = fresh();
    write(&mut fs, b"alpha");
    let mut t = fs.begin();
    assert_eq!(t.pin_snapshot().unwrap(), 1);
    assert_eq!(t.pin_snapshot().unwrap(), 1);
    fs.commit(t).unwrap();
    assert_eq!(fs.snapshots(), vec![1]);
    let mut t = fs.begin();
    assert_eq!(t.pin_snapshot().unwrap(), 2);
    t.drop_snapshot(1).unwrap();
    fs.commit(t).unwrap();
    assert_eq!(fs.snapshots(), vec![2]);
    let mut fs = reopened(fs);
    assert_eq!(fs.read_snapshot_file(2, "/state").unwrap(), b"alpha");
}

#[test]
fn blank_empty_snapshot_and_no_data_roundtrip() {
    let mut fs = fresh();
    let mut t = fs.begin();
    t.mkdir("/empty").unwrap();
    t.write_file("/empty/zero", b"").unwrap();
    fs.commit(t).unwrap();
    let mut t = fs.begin();
    t.pin_snapshot().unwrap();
    fs.commit(t).unwrap();
    let mut fs = reopened(fs);
    assert_eq!(fs.read_snapshot_file(1, "/empty/zero").unwrap(), b"");
    assert!(matches!(
        fs.read_snapshot_file(1, "/empty"),
        Err(Error::IsDirectory)
    ));
}

static NEXT: AtomicU64 = AtomicU64::new(0);
#[test]
fn snapshot_history_survives_real_file_image_close_reopen() {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "bfs-ng-f2-{}-{}-{id}.img",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut fs = FileSystem::format(FileDevice::create_new(&path, 128).unwrap(), 0x4444).unwrap();
    let mut tx = fs.begin();
    tx.write_file("/axiom", b"train").unwrap();
    fs.commit(tx).unwrap();
    let mut tx = fs.begin();
    tx.pin_snapshot().unwrap();
    tx.write_file("/axiom", b"test").unwrap();
    fs.commit(tx).unwrap();
    drop(fs);
    let mut fs = FileSystem::open(FileDevice::open(&path).unwrap()).unwrap();
    assert_eq!(fs.read_file("/axiom").unwrap(), b"test");
    assert_eq!(fs.read_snapshot_file(1, "/axiom").unwrap(), b"train");
    drop(fs);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn recoverability_of_current_and_old_roots_remains_after_repeated_reuse() {
    let mut fs = fresh();
    for i in 0..14 {
        write(&mut fs, format!("seed {i}").as_bytes());
        let mut fs2 = reopened(fs);
        assert_eq!(
            fs2.read_file("/state").unwrap(),
            format!("seed {i}").as_bytes()
        );
        fs = fs2;
    }
    assert!(
        fs.free_cursor() > 192 - 30,
        "allocator should reuse older blocks"
    );
}
