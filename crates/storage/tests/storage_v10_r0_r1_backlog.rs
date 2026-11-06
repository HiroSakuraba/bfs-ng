//! R0 verified read-view and R1 atomic per-inode range-write milestones.
//!
//! The read view performs one full open audit; it never grants allocation
//! authority. Hole punching and range batching retain F0/snapshot fencing.
use bfs_ng_storage::experimental_v10::{UnifiedV10, VerifiedReadView};
use bfs_ng_storage::{
    AttributeDef, AttributeType, CrashDevice, Error, Fault, Predicate, Schema, TextCase, Value,
};

const ID: u128 = 0x2026_1105_5245;

fn indexed(blocks: usize) -> UnifiedV10<CrashDevice> {
    UnifiedV10::format_indexed(CrashDevice::new(blocks), ID).unwrap()
}

fn model_write(target: &mut Vec<u8>, offset: usize, bytes: &[u8]) {
    if target.len() < offset + bytes.len() {
        target.resize(offset + bytes.len(), 0);
    }
    target[offset..offset + bytes.len()].copy_from_slice(bytes);
}

#[test]
fn ordered_multi_range_write_is_one_atomic_f0_generation() {
    let mut fs = indexed(960);
    fs.mkdir("/assets").unwrap();
    let mut model = vec![0x53; 3 * 4096];
    fs.write_at("/assets/mesh", 0, &model).unwrap();
    let snap = fs.manifest().generation;
    fs.pin_current().unwrap();
    let before = fs.manifest().generation;
    let ranges: &[(u64, &[u8])] = &[
        (0, b"first"),
        (4092, b"abcdefgh"),
        (4096, b"last"),
        (8193, b"!"),
        (0, b"FINAL"),
        (bfs_ng_storage::experimental_v10::MAX_FILE_BYTES, b""),
    ];
    let published = fs.write_ranges("/assets/mesh", ranges).unwrap();
    assert_eq!(published, before + 1);
    assert_eq!(fs.manifest().generation, published);
    for (offset, bytes) in ranges {
        model_write(&mut model, *offset as usize, bytes);
    }
    assert_eq!(fs.read_file("/assets/mesh").unwrap(), model);
    assert_eq!(
        fs.read_snapshot_file(snap, "/assets/mesh").unwrap(),
        vec![0x53; 3 * 4096]
    );
    // Empty segments are true no-ops, as with single-range write_at.
    let generation = fs.manifest().generation;
    assert_eq!(fs.write_ranges("/assets/mesh", &[]).unwrap(), generation);
    assert!(matches!(
        fs.write_ranges("/assets/mesh", &[(u64::MAX, b"x")]),
        Err(Error::TooLarge)
    ));
    assert_eq!(fs.manifest().generation, generation);
    let mut recovered = UnifiedV10::open(fs.into_device().power_loss()).unwrap();
    assert_eq!(recovered.read_file("/assets/mesh").unwrap(), model);
}

#[test]
fn sparse_punch_hole_keeps_length_and_historical_snapshot_bytes() {
    let mut fs = indexed(960);
    fs.mkdir("/d").unwrap();
    let mut original = vec![0x41; 4 * 4096];
    original[2 * 4096..3 * 4096].fill(0x7b);
    fs.write_at("/d/f", 0, &original).unwrap();
    let pin = fs.manifest().generation;
    fs.pin_current().unwrap();

    let before = fs.manifest().generation;
    let offset = 3000_u64;
    let length = 9500_u64;
    assert_eq!(fs.punch_hole("/d/f", offset, length).unwrap(), before + 1);
    let mut expected = original.clone();
    expected[offset as usize..(offset + length) as usize].fill(0);
    assert_eq!(fs.read_file("/d/f").unwrap(), expected);
    assert_eq!(fs.stat("/d/f").unwrap().2, original.len() as u64);
    assert_eq!(fs.read_snapshot_file(pin, "/d/f").unwrap(), original);
    // Repeating the punch must not stage new blocks or advance F0.
    let generation = fs.manifest().generation;
    assert_eq!(fs.punch_hole("/d/f", offset, length).unwrap(), generation);
    assert_eq!(fs.punch_hole("/d/f", 20_000, 100).unwrap(), generation);
    assert_eq!(fs.punch_hole("/d/f", 0, 0).unwrap(), generation);
    let mut recovered = UnifiedV10::open(fs.into_device().power_loss()).unwrap();
    assert_eq!(recovered.read_file("/d/f").unwrap(), expected);
    assert_eq!(recovered.read_snapshot_file(pin, "/d/f").unwrap(), original);
}

#[test]
fn sparse_and_range_operations_match_a_reference_buffer_through_reopen() {
    let mut fs = indexed(1536);
    fs.mkdir("/d").unwrap();
    fs.write_at("/d/file", 0, &[0; 16 * 1024]).unwrap();
    let mut model = vec![0_u8; 16 * 1024];
    for n in 0..50_u64 {
        if n % 4 == 0 {
            let start = ((n * 173) % 15_000) as usize;
            let len = 1111_usize.min(model.len() - start);
            fs.punch_hole("/d/file", start as u64, len as u64).unwrap();
            model[start..start + len].fill(0);
        } else {
            let p = ((n * 277) % 15_000) as usize;
            let a = n.to_le_bytes();
            let b = (n ^ 0x1234).to_be_bytes();
            fs.write_ranges("/d/file", &[(p as u64, &a), (p as u64 + 3, &b)])
                .unwrap();
            model_write(&mut model, p, &a);
            model_write(&mut model, p + 3, &b);
        }
        assert_eq!(fs.read_file("/d/file").unwrap(), model, "iteration {n}");
        if n % 10 == 9 {
            fs = UnifiedV10::open(fs.into_device().power_loss()).unwrap();
            assert_eq!(fs.read_file("/d/file").unwrap(), model);
        }
    }
}

#[test]
fn cached_read_view_stops_per_lookup_full_audits_and_matches_strict_writer() {
    let mut fs = indexed(1024);
    fs.mkdir("/d").unwrap();
    for i in 0..22_u64 {
        fs.write_at(&format!("/d/f{i:02}"), 0, &i.to_le_bytes())
            .unwrap();
    }
    fs.install_schema(
        &Schema::new(vec![AttributeDef {
            id: 1,
            name: "asset.kind".into(),
            ty: AttributeType::Text {
                case: TextCase::AsciiFold,
                max_bytes: 24,
            },
            indexed: true,
        }])
        .unwrap(),
    )
    .unwrap();
    fs.set_attributes(
        "/d/f00",
        &[("asset.kind".into(), Value::Text("mesh".into()))],
    )
    .unwrap();
    let snapshot = fs.manifest().generation;
    fs.pin_current().unwrap();
    fs.write_at("/d/f00", 0, b"updated!").unwrap();

    let mut view = VerifiedReadView::open(fs.into_device().power_loss()).unwrap();
    assert_eq!(view.list_dir("/d").unwrap().len(), 22);
    assert_eq!(view.snapshots().len(), 1);
    assert_eq!(
        view.file_attributes("/d/f00").unwrap().get("asset.kind"),
        Some(&Value::Text("mesh".into()))
    );
    assert_eq!(
        view.search_attributes(
            &[Predicate::eq("asset.kind", Value::Text("mesh".into()))],
            10
        )
        .unwrap()
        .files
        .len(),
        1
    );
    assert_eq!(
        view.search_snapshot_attributes(
            snapshot,
            &[Predicate::eq("asset.kind", Value::Text("mesh".into()))],
            10
        )
        .unwrap()
        .files
        .len(),
        1
    );
    assert_eq!(
        view.read_snapshot_file(snapshot, "/d/f00").unwrap(),
        0_u64.to_le_bytes()
    );
    assert_eq!(view.read_file("/d/f00").unwrap(), b"updated!");
    let start = view.physical_read_count().unwrap();
    for _ in 0..48 {
        assert_eq!(view.stat("/d/f01").unwrap().2, 8);
        assert_eq!(view.read_at("/d/f01", 0, 8).unwrap(), 1_u64.to_le_bytes());
        assert_eq!(view.list_dir("/d").unwrap().len(), 22);
        assert_eq!(view.file_attributes("/d/f00").unwrap().len(), 1);
    }
    let delta = view.physical_read_count().unwrap() - start;
    assert!(
        delta <= 48 * 3,
        "verified read view must use only extent and data reads, got {delta}"
    );

    let mut strict = UnifiedV10::open(view.into_device().power_loss()).unwrap();
    assert_eq!(strict.stat("/d/f01").unwrap().2, 8);
    assert_eq!(strict.read_at("/d/f01", 0, 8).unwrap(), 1_u64.to_le_bytes());
    assert_eq!(strict.read_file("/d/f00").unwrap(), b"updated!");
    assert_eq!(strict.list_dir("/d").unwrap().len(), 22);
    assert_eq!(
        strict.read_snapshot_file(snapshot, "/d/f00").unwrap(),
        0_u64.to_le_bytes()
    );
}

#[test]
fn torn_range_batch_and_hole_punch_recover_complete_old_or_new_state() {
    for operation in 1..=7 {
        for hole in [false, true] {
            let mut fs = indexed(640);
            fs.mkdir("/d").unwrap();
            let original = vec![0x3c_u8; 4 * 4096];
            fs.write_at("/d/f", 0, &original).unwrap();
            let stable_generation = fs.manifest().generation;
            let mut disk = fs.into_device().power_loss();
            disk.inject(Fault {
                operation,
                torn_bytes: Some(100),
            })
            .unwrap();
            let mut tx = UnifiedV10::open(disk).unwrap();
            let outcome = if hole {
                tx.punch_hole("/d/f", 2000, 10000)
            } else {
                tx.write_ranges("/d/f", &[(0, b"A"), (4094, b"cross"), (8192, b"another")])
            };
            assert!(matches!(outcome, Err(Error::InjectedCrash)));
            let mut recovered = UnifiedV10::open(tx.into_device().power_loss()).unwrap();
            assert!(
                recovered.manifest().generation == stable_generation
                    || recovered.manifest().generation == stable_generation + 1
            );
            let actual = recovered.read_file("/d/f").unwrap();
            let mut new = original.clone();
            if hole {
                new[2000..12_000].fill(0);
            } else {
                model_write(&mut new, 0, b"A");
                model_write(&mut new, 4094, b"cross");
                model_write(&mut new, 8192, b"another");
            }
            assert!(
                actual == original || actual == new,
                "torn F0 mixed file data at operation {operation}, hole={hole}"
            );
        }
    }
}
