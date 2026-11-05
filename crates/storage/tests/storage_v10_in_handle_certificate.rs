//! F5.3 opt-in, in-handle certified allocator on an exclusive CrashDevice.
//! This only skips repeated *old* F0 graph reads; every newly staged
//! generation still passes full independent graph validation.
use bfs_ng_storage::experimental_v10::{ReadOnlyV10, UnifiedV10};
use bfs_ng_storage::{
    AttributeDef, AttributeType, CrashDevice, Error, Fault, Schema, TextCase, Value,
};

const VID: u128 = 0xc3f1_3a11_0ca7_2026;

fn mutate(fs: &mut UnifiedV10<CrashDevice>, i: u64, name: &mut &'static str) {
    if i % 13 == 7 {
        let target = if *name == "/asset/data" {
            "/asset/renamed"
        } else {
            "/asset/data"
        };
        fs.rename(name, target).unwrap();
        *name = target;
    } else if i % 11 == 3 {
        fs.truncate(name, 13).unwrap();
    } else if i % 5 == 2 {
        fs.write_at(name, 4100, &i.to_le_bytes()).unwrap();
    } else {
        fs.write_at(name, 0, &i.to_le_bytes()).unwrap();
    }
}

#[test]
fn exclusive_in_handle_allocator_matches_strict_mode_and_reduces_disk_walks() {
    let mut slow = UnifiedV10::format_indexed(CrashDevice::new(768), VID).unwrap();
    let mut fast = UnifiedV10::format_indexed(CrashDevice::new(768), VID).unwrap();
    fast.enable_in_handle_allocation_certificate().unwrap();

    let mut slow_name = "/asset/data";
    let mut fast_name = "/asset/data";
    for fs in [&mut slow, &mut fast] {
        fs.mkdir("/asset").unwrap();
        fs.write_at("/asset/data", 0, b"initial").unwrap();
        let schema = Schema::new(vec![AttributeDef {
            id: 1,
            name: "kind".into(),
            ty: AttributeType::Text {
                case: TextCase::AsciiFold,
                max_bytes: 24,
            },
            indexed: true,
        }])
        .unwrap();
        fs.install_schema(&schema).unwrap();
        fs.set_attributes(
            "/asset/data",
            &[("kind".into(), Value::Text("mesh".into()))],
        )
        .unwrap();
    }
    let inode = slow.stat(slow_name).unwrap().0;
    let mut history = Vec::new();
    for i in 0..96_u64 {
        mutate(&mut slow, i, &mut slow_name);
        mutate(&mut fast, i, &mut fast_name);
        assert_eq!(slow_name, fast_name);
        if [14, 38, 61].contains(&i) {
            let generation = slow.manifest().generation;
            assert_eq!(generation, fast.manifest().generation);
            slow.pin_current().unwrap();
            fast.pin_current().unwrap();
            history.push((generation, slow_name.to_owned()));
        }
        if i == 52 {
            for fs in [&mut slow, &mut fast] {
                fs.set_attributes(
                    slow_name,
                    &[("kind".into(), Value::Text("shader".into()))],
                )
                .unwrap();
            }
        }
        if i == 70 {
            for fs in [&mut slow, &mut fast] {
                fs.mark_dropping(history[1].0).unwrap();
            }
        }
        if i % 9 == 4 {
            slow.reclaim_step().unwrap();
            fast.reclaim_step().unwrap();
        }
        assert_eq!(slow.manifest(), fast.manifest(), "step {i}");
    }
    let old_device = slow.into_device();
    let fast_device = fast.into_device();
    let old_reads = old_device.read_blocks();
    let fast_reads = fast_device.read_blocks();
    assert!(
        fast_reads < old_reads,
        "old-root cache must reduce physical reads: cached={fast_reads}, full={old_reads}"
    );
    // Power-loss reopening always independently audits both F0 manifests.
    // Compare real persisted contents and all historical snapshots, including
    // the Dropping descriptor, rather than trusting identical counters alone.
    let mut slow = UnifiedV10::open(old_device.power_loss()).unwrap();
    let mut fast = UnifiedV10::open(fast_device.power_loss()).unwrap();
    assert_eq!(slow.manifest(), fast.manifest());
    assert_eq!(slow.read_file(slow_name).unwrap(), fast.read_file(fast_name).unwrap());
    assert_eq!(slow.stat(slow_name).unwrap().0, fast.stat(fast_name).unwrap().0);
    assert_eq!(slow.stat(slow_name).unwrap().0, inode);
    assert_eq!(
        slow.file_attributes(slow_name).unwrap(),
        fast.file_attributes(fast_name).unwrap()
    );
    for (generation, name) in &history {
        assert_eq!(
            slow.read_snapshot_file(*generation, name).unwrap(),
            fast.read_snapshot_file(*generation, name).unwrap(),
            "historical snapshot {generation}"
        );
    }
}

#[test]
fn rejected_edits_cannot_advance_an_in_handle_allocation_certificate() {
    let mut cached = UnifiedV10::format_indexed(CrashDevice::new(384), VID).unwrap();
    let mut strict = UnifiedV10::format_indexed(CrashDevice::new(384), VID).unwrap();
    cached.enable_in_handle_allocation_certificate().unwrap();
    for fs in [&mut cached, &mut strict] {
        fs.mkdir("/a").unwrap();
        fs.write_at("/a/file", 0, b"v1").unwrap();
    }
    let generation = cached.manifest().generation;
    assert!(matches!(
        cached.rename("/a/file", "/missing/file"),
        Err(Error::NotFound) | Err(Error::NotDirectory)
    ));
    assert_eq!(cached.manifest().generation, generation);
    // Reusing the cached certificate after a rejected (unpublished) edit
    // must give the same generation and bytes as the full-audit path.
    cached.write_at("/a/file", 0, b"v2").unwrap();
    strict.write_at("/a/file", 0, b"v2").unwrap();
    assert_eq!(cached.manifest(), strict.manifest());
    let mut checked = UnifiedV10::open(cached.into_device().power_loss()).unwrap();
    assert_eq!(checked.read_file("/a/file").unwrap(), b"v2");
}

#[test]
fn the_fast_lease_is_not_available_on_an_unindexed_image() {
    let mut fs = UnifiedV10::format(CrashDevice::new(128), VID).unwrap();
    assert!(fs.enable_in_handle_allocation_certificate().is_err());
}

#[test]
fn in_handle_certificate_crash_cuts_reopen_with_old_or_new_f0_graph() {
    for operation in 1..=8 {
        for torn_bytes in [0, 64, 4096] {
            let mut fs = UnifiedV10::format_indexed(CrashDevice::new(448), VID).unwrap();
            fs.mkdir("/demo").unwrap();
            fs.write_at("/demo/file", 0, b"previous").unwrap();
            let snap = fs.manifest().generation;
            fs.pin_current().unwrap();
            for i in 0..12_u64 {
                fs.write_at("/demo/file", 0, &i.to_le_bytes()).unwrap();
            }
            let old_generation = fs.manifest().generation;
            let previous = fs.read_file("/demo/file").unwrap();
            let mut disk = fs.into_device().power_loss();
            disk.inject(Fault {
                operation,
                torn_bytes: Some(torn_bytes),
            })
            .unwrap();
            let mut writer = UnifiedV10::open(disk).unwrap();
            writer.enable_in_handle_allocation_certificate().unwrap();
            assert!(matches!(
                writer.write_at("/demo/file", 0, b"new-data"),
                Err(Error::InjectedCrash)
            ));
            let recovered = writer.into_device().power_loss();
            let reader = ReadOnlyV10::open(recovered).unwrap();
            let root_generation = reader.root_info().generation;
            assert!(
                root_generation == old_generation || root_generation == old_generation + 1
            );
            if let Ok(mut checked) = UnifiedV10::open(reader.into_device()) {
                assert_eq!(
                    checked.read_file("/demo/file").unwrap(),
                    if root_generation == old_generation {
                        previous.as_slice()
                    } else {
                        b"new-data"
                    }
                );
                assert_eq!(
                    checked.read_snapshot_file(snap, "/demo/file").unwrap(),
                    b"previous"
                );
            }
        }
    }
}
