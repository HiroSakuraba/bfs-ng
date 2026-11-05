use bfs_ng_storage::experimental_v10::{SnapshotState, UnifiedV10};
use bfs_ng_storage::{BLOCK_SIZE, CrashDevice, Error, Fault};

const VID: u128 = 0x3fee_5201_a4c0_9271;
fn fresh(blocks: usize) -> UnifiedV10<CrashDevice> {
    UnifiedV10::format_indexed(CrashDevice::new(blocks), VID).unwrap()
}
fn reopen(fs: UnifiedV10<CrashDevice>) -> UnifiedV10<CrashDevice> {
    UnifiedV10::open(fs.into_device().power_loss()).unwrap()
}

#[test]
fn indexed_format_commits_durable_free_root_in_bounded_metadata_arena() {
    let fs = fresh(320);
    let m = *fs.manifest();
    assert_eq!(m.generation, 2);
    assert_eq!(m.metadata_reserve, 64);
    assert!(m.roots.free.is_some());
    assert!(m.roots.dead_list.is_none());
    assert_eq!(m.pending_blocks, 0);
    assert!(m.free_blocks > 200);
    assert!(m.roots.free.unwrap().address < 69);
    let mut fs = reopen(fs);
    assert_eq!(fs.manifest(), &m);
    assert!(fs.list_dir("/").unwrap().is_empty());
    fs.mkdir("/assets").unwrap();
    fs.write_at("/assets/main", 0, b"v1").unwrap();
    assert_eq!(fs.read_file("/assets/main").unwrap(), b"v1");
    assert_eq!(fs.manifest().metadata_reserve, 64);
    assert!(fs.manifest().free_blocks < m.free_blocks);
    fs = reopen(fs);
    assert_eq!(fs.read_file("/assets/main").unwrap(), b"v1");
}

#[test]
fn indexed_repeated_writes_preserve_snapshots_and_reopen_cursor() {
    let mut fs = fresh(320);
    fs.mkdir("/dve").unwrap();
    fs.write_at("/dve/material", 0, b"original").unwrap();
    let snap = fs.manifest().generation;
    fs.pin_current().unwrap();
    let mut prior = b"original".to_vec();
    let mut recycled = 0;
    for i in 0..120_u64 {
        let input = format!("version-{i:04}").into_bytes();
        fs.write_at("/dve/material", 0, &input).unwrap();
        prior.resize(prior.len().max(input.len()), 0);
        prior[..input.len()].copy_from_slice(&input);
        assert_eq!(fs.read_file("/dve/material").unwrap(), prior);
        if i % 12 == 0 {
            fs = reopen(fs);
            assert_eq!(fs.read_snapshot_file(snap, "/dve/material").unwrap(), b"original");
        }
        recycled += fs.last_reused_blocks();
    }
    assert!(recycled > 0, "expected indexed reuse of old data or namespace pages");
    assert_eq!(fs.read_snapshot_file(snap, "/dve/material").unwrap(), b"original");
    fs.mark_dropping(snap).unwrap();
    assert_eq!(fs.snapshots().unwrap()[0].state, SnapshotState::Dropping);
    fs.reclaim_step().unwrap();
    fs = reopen(fs);
    assert_eq!(fs.read_snapshot_file(snap, "/dve/material").unwrap(), b"original");
}

#[test]
fn indexed_reclaim_persists_pending_counts_across_f0_rotation() {
    let mut fs = fresh(320);
    fs.mkdir("/test").unwrap();
    fs.write_at("/test/f", 0, b"first").unwrap();
    let mut observed_pending = false;
    let mut observed_free_growth = false;
    let mut previous_free = fs.manifest().free_blocks;
    for i in 0..50_u64 {
        fs.write_at("/test/f", 0, &i.to_le_bytes()).unwrap();
        let m = *fs.manifest();
        assert!(m.roots.free.is_some());
        assert_eq!(m.metadata_reserve, 64);
        observed_pending |= m.pending_blocks > 0;
        observed_free_growth |= m.free_blocks > previous_free;
        previous_free = m.free_blocks;
        if i % 10 == 0 {
            fs = reopen(fs);
        }
        // Reclaim may move at most the bounded pending interval set; each
        // completion is a new atomic manifest with the same file contents.
        let data = fs.read_file("/test/f").unwrap();
        fs.reclaim_step().unwrap();
        assert_eq!(fs.read_file("/test/f").unwrap(), data);
    }
    assert!(observed_pending, "expected delayed retirement entries");
    assert!(observed_free_growth, "expected retirement return to free space");
}

#[test]
fn damaged_fallback_space_root_blocks_the_indexed_writer() {
    let mut fs = fresh(320);
    fs.mkdir("/a").unwrap();
    fs.write_at("/a/f", 0, b"old").unwrap();
    let previous = fs.manifest().roots.free.unwrap();
    fs.write_at("/a/f", 0, b"new").unwrap();
    // The prior Free root is referenced by the fallback F0 candidate.
    let mut disk = fs.into_device().power_loss();
    disk.corrupt_stable_for_testing(previous.address as usize, 36, 0xbc);
    assert!(UnifiedV10::open(disk.power_loss()).is_err());
}

#[test]
fn failure_before_f0_publication_never_exposes_mixed_free_and_data_roots() {
    for operation in 1..=8 {
        for torn_bytes in [0, 124, BLOCK_SIZE] {
            let mut fs = fresh(320);
            fs.mkdir("/assets").unwrap();
            fs.write_at("/assets/file", 0, b"old").unwrap();
            for i in 0..9 {
                fs.write_at("/assets/file", 0, format!("v-{i:02}").as_bytes()).unwrap();
            }
            let previous = fs.read_file("/assets/file").unwrap();
            let selected = fs.manifest().generation;
            let mut device = fs.into_device().power_loss();
            device.inject(Fault { operation, torn_bytes: Some(torn_bytes) }).unwrap();
            let mut interrupted = UnifiedV10::open(device).unwrap();
            assert!(matches!(
                interrupted.write_at("/assets/file", 0, b"next"),
                Err(Error::InjectedCrash)
            ));
            let disk = interrupted.into_device().power_loss();
            if let Ok(mut recovered) = UnifiedV10::open(disk) {
                assert!(
                    recovered.manifest().generation == selected
                        || recovered.manifest().generation == selected + 1
                );
                let mut expected_next = previous.clone();
                expected_next[..4].copy_from_slice(b"next");
                let recovered_bytes = recovered.read_file("/assets/file").unwrap();
                assert!(
                    recovered_bytes == previous || recovered_bytes == expected_next,
                    "mixed data/free-index publication"
                );
            }
        }
    }
}
