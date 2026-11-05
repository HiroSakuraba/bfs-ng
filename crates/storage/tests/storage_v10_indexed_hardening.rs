use bfs_ng_storage::experimental_v10::{SnapshotState, UnifiedV10};
use bfs_ng_storage::{
    AttributeDef, AttributeType, CrashDevice, Predicate, Schema, TextCase, Value,
};
const VID: u128 = 0x3fee_5201_a4c0_9271;
fn fresh(blocks: usize) -> UnifiedV10<CrashDevice> {
    UnifiedV10::format_indexed(CrashDevice::new(blocks), VID).unwrap()
}
fn reopen(fs: UnifiedV10<CrashDevice>) -> UnifiedV10<CrashDevice> {
    UnifiedV10::open(fs.into_device().power_loss()).unwrap()
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
            assert_eq!(
                fs.read_snapshot_file(snap, "/dve/material").unwrap(),
                b"original"
            );
        }
        recycled += fs.last_reused_blocks();
    }
    assert!(
        recycled > 0,
        "expected indexed reuse of old data or namespace pages"
    );
    assert_eq!(
        fs.read_snapshot_file(snap, "/dve/material").unwrap(),
        b"original"
    );
    fs.mark_dropping(snap).unwrap();
    assert_eq!(fs.snapshots().unwrap()[0].state, SnapshotState::Dropping);
    fs.reclaim_step().unwrap();
    fs = reopen(fs);
    assert_eq!(
        fs.read_snapshot_file(snap, "/dve/material").unwrap(),
        b"original"
    );
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
    assert!(
        observed_free_growth,
        "expected retirement return to free space"
    );
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
fn indexed_mode_keeps_inode_keyed_attributes_and_historical_postings() {
    let mut fs = fresh(512);
    fs.mkdir("/assets").unwrap();
    fs.write_at("/assets/shader", 0, b"shader-v1").unwrap();
    let schema = Schema::new(vec![AttributeDef {
        id: 1,
        name: "kind".into(),
        ty: AttributeType::Text {
            case: TextCase::AsciiFold,
            max_bytes: 40,
        },
        indexed: true,
    }])
    .unwrap();
    fs.install_schema(&schema).unwrap();
    fs.set_attributes(
        "/assets/shader",
        &[("kind".into(), Value::Text("Shader".into()))],
    )
    .unwrap();
    let before = fs.manifest().generation;
    fs.pin_current().unwrap();
    fs.rename("/assets", "/resources").unwrap();
    fs.set_attributes(
        "/resources/shader",
        &[("kind".into(), Value::Text("Material".into()))],
    )
    .unwrap();
    for n in 0..20 {
        fs.write_at("/resources/shader", 0, format!("shader-{n:04}").as_bytes())
            .unwrap();
    }
    let historical = fs
        .search_snapshot_attributes(
            before,
            &[Predicate::eq("kind", Value::Text("shader".into()))],
            10,
        )
        .unwrap();
    assert_eq!(historical.files.len(), 1);
    assert_eq!(
        fs.read_snapshot_file(before, "/assets/shader").unwrap(),
        b"shader-v1"
    );
    let now = fs
        .search_attributes(&[Predicate::eq("kind", Value::Text("material".into()))], 10)
        .unwrap();
    assert_eq!(now.files.len(), 1);
    fs = reopen(fs);
    assert_eq!(
        fs.search_snapshot_attributes(
            before,
            &[Predicate::eq("kind", Value::Text("shader".into()))],
            10,
        )
        .unwrap()
        .files
        .len(),
        1
    );
    assert_eq!(
        fs.file_attributes("/resources/shader").unwrap().get("kind"),
        Some(&Value::Text("material".into()))
    );
}

#[test]
fn indexed_retirement_cursor_and_counters_survive_power_loss_reopen() {
    let mut fs = fresh(320);
    fs.mkdir("/corpus").unwrap();
    fs.write_at("/corpus/one", 0, b"initial").unwrap();
    for i in 0..16_u64 {
        fs.write_at("/corpus/one", 0, &i.to_le_bytes()).unwrap();
    }
    let before = *fs.manifest();
    assert!(before.pending_blocks > 0);
    assert!(before.sweep_snapshot > 0);
    fs = reopen(fs);
    assert_eq!(fs.manifest().sweep_snapshot, before.sweep_snapshot);
    assert_eq!(fs.manifest().sweep_key, before.sweep_key);
    assert_eq!(fs.manifest().free_blocks, before.free_blocks);
    assert_eq!(fs.manifest().pending_blocks, before.pending_blocks);
    let generation = fs.reclaim_step().unwrap();
    let after = *fs.manifest();
    assert_eq!(after.generation, generation);
    fs = reopen(fs);
    assert_eq!(fs.manifest().sweep_snapshot, after.sweep_snapshot);
    assert_eq!(fs.manifest().sweep_key, after.sweep_key);
    assert_eq!(fs.manifest().free_blocks, after.free_blocks);
    assert_eq!(fs.manifest().pending_blocks, after.pending_blocks);
    assert_eq!(fs.read_file("/corpus/one").unwrap(), 15_u64.to_le_bytes());
}
