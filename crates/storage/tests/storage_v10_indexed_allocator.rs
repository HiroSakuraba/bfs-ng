//! F5.3 opt-in indexed-space integration: verify F0 publication, persisted
//! Free/Dead roots, historical data and repeated-write endurance.
use bfs_ng_storage::experimental_v10::{ReadOnlyV10, UnifiedV10};
use bfs_ng_storage::{BLOCK_SIZE, CrashDevice, Error, Fault};

const VID: u128 = 0xabcc_3521_0457_9988;

fn fresh(blocks: usize) -> UnifiedV10<CrashDevice> {
    UnifiedV10::format_indexed(CrashDevice::new(blocks), VID).unwrap()
}
fn reopen(writer: UnifiedV10<CrashDevice>) -> UnifiedV10<CrashDevice> {
    UnifiedV10::open(writer.into_device().power_loss()).unwrap()
}

#[test]
fn indexed_format_publishes_a_persistent_free_root_under_f0() {
    let mut fs = fresh(512);
    let m = *fs.manifest();
    assert_eq!(m.metadata_reserve, 64);
    assert_eq!(m.generation, 2);
    assert!(m.roots.free.is_some());
    assert!(m.roots.dead_list.is_none());
    assert_eq!(m.pending_blocks, 0);
    assert!(m.free_blocks > 300);
    assert!(fs.list_dir("/").unwrap().is_empty());
    fs = reopen(fs);
    assert_eq!(fs.manifest(), &m);
    assert_eq!(fs.reclaim_step().unwrap(), m.generation);
}

#[test]
fn indexed_repeated_overwrites_preserve_inode_snapshot_and_search() {
    let mut fs = fresh(420);
    fs.mkdir("/assets").unwrap();
    fs.write_at("/assets/material", 0, b"original").unwrap();
    let historical = fs.manifest().generation;
    let inode = fs.stat("/assets/material").unwrap().0;
    fs.pin_current().unwrap();
    let mut reused = 0_usize;
    for generation in 0..100_u32 {
        let bytes = format!("update-{generation:05}");
        fs.write_at("/assets/material", 0, bytes.as_bytes())
            .unwrap();
        assert_eq!(fs.stat("/assets/material").unwrap().0, inode);
        assert_eq!(fs.read_file("/assets/material").unwrap(), bytes.as_bytes());
        assert_eq!(
            fs.read_snapshot_file(historical, "/assets/material")
                .unwrap(),
            b"original"
        );
        reused += fs.last_reused_blocks();
        if generation % 20 == 0 {
            fs = reopen(fs);
        }
    }
    assert!(reused > 20, "expected repeated recycled physical pages");
    assert_eq!(
        fs.read_snapshot_file(historical, "/assets/material")
            .unwrap(),
        b"original"
    );
    fs = reopen(fs);
    assert_eq!(fs.stat("/assets/material").unwrap().0, inode);
}

#[test]
fn indexed_retirement_cursor_roundtrips_after_reopen() {
    let mut fs = fresh(400);
    fs.mkdir("/world").unwrap();
    fs.write_at("/world/a", 0, b"abc").unwrap();
    let before = fs.manifest().generation;
    for n in 0..18 {
        fs.write_at("/world/a", 0, format!("{n:03}").as_bytes())
            .unwrap();
    }
    let first = fs.manifest().pending_blocks;
    fs.reclaim_step().unwrap();
    let after = *fs.manifest();
    assert_eq!(after.metadata_reserve, 64);
    assert!(after.roots.free.is_some());
    assert!(after.generation > before);
    assert!(after.pending_blocks <= first + 512);
    fs = reopen(fs);
    assert_eq!(fs.manifest(), &after);
    assert_eq!(fs.read_file("/world/a").unwrap(), b"017");
}

#[test]
fn indexed_crash_cut_does_not_corrupt_old_or_new_f0_namespace() {
    for operation in 1..=8 {
        for torn_bytes in [0, 64, BLOCK_SIZE] {
            let mut fs = fresh(380);
            fs.mkdir("/dve").unwrap();
            fs.write_at("/dve/asset", 0, b"old!").unwrap();
            for n in 0..8 {
                fs.write_at("/dve/asset", 0, format!("{n:04}").as_bytes())
                    .unwrap();
            }
            let generation = fs.manifest().generation;
            let previous = fs.read_file("/dve/asset").unwrap();
            let mut device = fs.into_device().power_loss();
            device
                .inject(Fault {
                    operation,
                    torn_bytes: Some(torn_bytes),
                })
                .unwrap();
            let mut attempted = UnifiedV10::open(device).unwrap();
            assert!(matches!(
                attempted.write_at("/dve/asset", 0, b"new!"),
                Err(Error::InjectedCrash)
            ));
            let recovered = attempted.into_device().power_loss();
            let reader = ReadOnlyV10::open(recovered).unwrap();
            let got_generation = reader.root_info().generation;
            assert!(got_generation == generation || got_generation == generation + 1);
            if let Ok(mut checked) = UnifiedV10::open(reader.into_device()) {
                assert_eq!(
                    checked.read_file("/dve/asset").unwrap(),
                    if got_generation == generation {
                        previous.as_slice()
                    } else {
                        b"new!"
                    },
                );
            }
        }
    }
}
