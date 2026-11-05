use bfs_ng_storage::experimental_v10::{ReadOnlyV10, UnifiedV10, SnapshotState};
use bfs_ng_storage::{BLOCK_SIZE, BlockDevice, CrashDevice, Error, Fault};

const VID: u128 = 0xc5af_1912_5503_8372;

fn fresh(blocks: usize) -> UnifiedV10<CrashDevice> {
    UnifiedV10::format(CrashDevice::new(blocks), VID).unwrap()
}
fn reopen(writer: UnifiedV10<CrashDevice>) -> UnifiedV10<CrashDevice> {
    UnifiedV10::open(writer.into_device().power_loss()).unwrap()
}

#[test]
fn hundreds_of_overwrites_recycle_unowned_metadata_without_running_out() {
    let mut fs = fresh(56);
    fs.mkdir("/data").unwrap();
    fs.write_at("/data/file", 0, b"initial").unwrap();
    let snapshot = fs.manifest().generation;
    fs.pin_current().unwrap();
    let mut reused = 0_usize;
    for i in 0..300_u64 {
        let payload = i.to_le_bytes();
        fs.write_at("/data/file", 0, &payload).unwrap();
        assert_eq!(fs.read_file("/data/file").unwrap(), payload);
        assert_eq!(fs.read_snapshot_file(snapshot, "/data/file").unwrap(), b"initial");
        reused += fs.last_reused_blocks();
        if i % 30 == 0 {
            fs = reopen(fs);
            assert_eq!(fs.read_file("/data/file").unwrap(), payload);
        }
    }
    assert!(reused > 100, "expected substantial certified block reuse");
    fs = reopen(fs);
    assert_eq!(fs.manifest().generation, 304);
    assert_eq!(fs.read_snapshot_file(snapshot, "/data/file").unwrap(), b"initial");
}

#[test]
fn nonzero_orphan_can_be_reused_only_after_independent_full_graph_audit() {
    let fs = fresh(60);
    let old_watermark = fs.manifest().high_water;
    let orphan_addr = old_watermark - 1;
    let mut disk = fs.into_device().power_loss();
    let mut stale = [0_u8; BLOCK_SIZE];
    stale[..8].copy_from_slice(b"ORPHAN!!");
    disk.write_block(orphan_addr, &stale).unwrap();
    disk.sync().unwrap();
    let mut fs = UnifiedV10::open(disk.power_loss()).unwrap();
    fs.mkdir("/proof").unwrap();
    assert!(fs.last_reused_blocks() >= 1);
    let mut disk = fs.into_device().power_loss();
    let mut now = [0; BLOCK_SIZE];
    disk.read_block(orphan_addr, &mut now).unwrap();
    assert_ne!(now, stale, "orphan was certified unowned and overwritten");
    fs = UnifiedV10::open(disk.power_loss()).unwrap();
    assert!(fs.stat("/proof").unwrap().1);
}

#[test]
fn pinned_snapshots_survive_many_reuses_even_when_live_directory_moves() {
    let mut fs = fresh(90);
    fs.mkdir("/dve").unwrap();
    fs.write_at("/dve/scene", 0, b"old-scene").unwrap();
    let snap = fs.manifest().generation;
    fs.pin_current().unwrap();
    fs.rename("/dve", "/game").unwrap();
    for i in 0..140 {
        fs.write_at("/game/scene", 0, format!("scene-{i:04}").as_bytes()).unwrap();
        if i % 14 == 0 {
            fs = reopen(fs);
            assert_eq!(fs.read_snapshot_file(snap, "/dve/scene").unwrap(), b"old-scene");
        }
    }
    assert!(matches!(fs.read_file("/dve/scene"), Err(Error::NotFound)));
    assert_eq!(fs.read_snapshot_file(snap, "/dve/scene").unwrap(), b"old-scene");
    fs.mark_dropping(snap).unwrap();
    assert_eq!(fs.snapshots().unwrap()[0].state, SnapshotState::Dropping);
    assert_eq!(fs.read_snapshot_file(snap, "/dve/scene").unwrap(), b"old-scene");
}

#[test]
fn many_pins_exhaust_volume_without_damaging_previous_generation() {
    let mut fs = fresh(48);
    fs.mkdir("/data").unwrap();
    fs.write_at("/data/v", 0, b"v0").unwrap();
    let first = fs.manifest().generation;
    fs.pin_current().unwrap();
    let mut ran_out = false;
    for i in 0..100 {
        let before = fs.manifest().generation;
        let data = format!("v{i}");
        match fs.write_at("/data/v", 0, data.as_bytes()) {
            Ok(_) => {
                match fs.pin_current() {
                    Ok(_) => {}
                    Err(Error::NoSpace) => { ran_out = true; break; }
                    Err(other) => panic!("unexpected pin error: {other}"),
                }
            }
            Err(Error::NoSpace) => {
                assert_eq!(fs.manifest().generation, before);
                ran_out = true;
                break;
            }
            Err(other) => panic!("unexpected write error: {other}"),
        }
    }
    assert!(ran_out, "pinned histories must eventually exhaust tiny image");
    fs = reopen(fs);
    assert_eq!(fs.read_snapshot_file(first, "/data/v").unwrap(), b"v0");
}

#[test]
fn crash_cuts_during_recycled_writes_cannot_damage_f0_fallback_roots() {
    for operation in 1..=8 {
        for torn_bytes in [0, 16, 124, BLOCK_SIZE] {
            let mut fs = fresh(55);
            fs.mkdir("/assets").unwrap();
            fs.write_at("/assets/material", 0, b"old").unwrap();
            for i in 0..25 {
                fs.write_at("/assets/material", 0, format!("step-{i}").as_bytes()).unwrap();
            }
            let old_gen = fs.manifest().generation;
            let original = fs.read_file("/assets/material").unwrap();
            let mut disk = fs.into_device().power_loss();
            disk.inject(Fault { operation, torn_bytes: Some(torn_bytes) }).unwrap();
            let mut attempting = UnifiedV10::open(disk).unwrap();
            assert!(matches!(
                attempting.write_at("/assets/material", 0, b"latest"),
                Err(Error::InjectedCrash)
            ));
            let disk = attempting.into_device().power_loss();
            let reader = ReadOnlyV10::open(disk).unwrap();
            let observed = reader.root_info().generation;
            assert!(observed == old_gen || observed == old_gen + 1);
            if let Ok(mut checked) = UnifiedV10::open(reader.into_device()) {
                assert_eq!(
                    checked.read_file("/assets/material").unwrap(),
                    if observed == old_gen { original.as_slice() } else { b"latest" },
                );
            }
        }
    }
}

#[test]
fn root_poisoning_still_blocks_reuse_despite_available_unowned_blocks() {
    let mut fs = fresh(95);
    fs.mkdir("/a").unwrap();
    fs.write_at("/a/file", 0, b"one").unwrap();
    let older = fs.manifest().roots.fs.address;
    fs.write_at("/a/file", 0, b"two").unwrap();
    let mut disk = fs.into_device().power_loss();
    disk.corrupt_stable_for_testing(older as usize, 90, 0x7f);
    assert!(UnifiedV10::open(disk.power_loss()).is_err());
}
