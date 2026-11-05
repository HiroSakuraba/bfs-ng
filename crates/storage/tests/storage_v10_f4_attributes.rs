use bfs_ng_storage::{
    AttributeDef, AttributeType, CrashDevice, Error, Predicate, Schema, TextCase, Value,
};
use bfs_ng_storage::experimental_v10::{ReadOnlyV10, UnifiedV10};
use std::collections::BTreeSet;

const VID: u128 = 0xd34a_4987_1017_3271;
fn fresh() -> UnifiedV10<CrashDevice> {
    UnifiedV10::format(CrashDevice::new(2500), VID).unwrap()
}
fn reopen(fs: UnifiedV10<CrashDevice>) -> UnifiedV10<CrashDevice> {
    UnifiedV10::open(fs.into_device().power_loss()).unwrap()
}
fn schema() -> Schema {
    Schema::new(vec![
        AttributeDef {
            id: 1,
            name: "asset.kind".into(),
            ty: AttributeType::Text {
                case: TextCase::AsciiFold,
                max_bytes: 64,
            },
            indexed: true,
        },
        AttributeDef {
            id: 2,
            name: "asset.tags".into(),
            ty: AttributeType::TextSet {
                case: TextCase::ExactUtf8,
                max_bytes: 64,
                max_items: 8,
            },
            indexed: true,
        },
        AttributeDef {
            id: 3,
            name: "asset.updated".into(),
            ty: AttributeType::Unsigned,
            indexed: true,
        },
    ]).unwrap()
}
fn kind(s: &str) -> Value { Value::Text(s.to_owned()) }
fn eq_kind(value: &str) -> Predicate {
    Predicate::eq("asset.kind", kind(value))
}
fn rows(v: &str, index: u64) -> Vec<(String, Value)> {
    vec![
        ("asset.kind".into(), kind(v)),
        ("asset.tags".into(), Value::TextSet(
            ["dve".to_string(), "game".to_string()].into_iter().collect::<BTreeSet<_>>()
        )),
        ("asset.updated".into(), Value::Unsigned(index)),
    ]
}

#[test]
fn v10_f4_attribute_queries_survive_reopen_rename_and_snapshot_rotation() {
    let mut fs = fresh();
    fs.mkdir("/assets").unwrap();
    fs.mkdir("/other").unwrap();
    fs.write_at("/assets/shader.hlsl", 0, b"old shader").unwrap();
    fs.write_at("/assets/wood.bin", 0, b"wood").unwrap();
    assert!(fs.search_attributes(&[eq_kind("shader")], 10).is_err());
    fs.install_schema(&schema()).unwrap();
    fs.set_attributes("/assets/shader.hlsl", &rows("SHADER", 2)).unwrap();
    fs.set_attributes("/assets/wood.bin", &rows("texture", 3)).unwrap();
    let old_attrs = fs.file_attributes("/assets/shader.hlsl").unwrap();
    assert_eq!(old_attrs.get("asset.kind"), Some(&kind("shader")));
    let before = fs.search_attributes(&[eq_kind("shader")], 10).unwrap();
    assert_eq!(before.files.len(), 1);
    let pinned = fs.manifest().generation;
    let old_bp = fs.manifest().roots.attribute_rows.unwrap();
    fs.pin_current().unwrap();

    fs.rename("/assets", "/other/resources").unwrap();
    fs.set_attributes("/other/resources/shader.hlsl", &rows("material", 4)).unwrap();
    fs.write_at("/other/resources/shader.hlsl", 0, b"new shader").unwrap();
    for n in 0..6_u64 {
        fs.write_at("/other/resources/wood.bin", 0, format!("wood-{n}").as_bytes()).unwrap();
    }
    assert!(fs.search_attributes(&[eq_kind("shader")], 10).unwrap().files.is_empty());
    assert_eq!(fs.search_attributes(&[eq_kind("material")], 10).unwrap().files.len(), 1);
    let old_query = fs.search_snapshot_attributes(pinned, &[eq_kind("shader")], 10).unwrap();
    assert_eq!(old_query.files.len(), 1);
    assert_eq!(fs.read_snapshot_file(pinned, "/assets/shader.hlsl").unwrap(), b"old shader");
    assert_eq!(fs.file_attributes("/other/resources/shader.hlsl").unwrap()
        .get("asset.kind"), Some(&kind("material")));
    assert_ne!(fs.manifest().roots.attribute_rows.unwrap(), old_bp);
    fs = reopen(fs);
    assert_eq!(fs.search_snapshot_attributes(pinned, &[eq_kind("shader")], 10)
        .unwrap().files, old_query.files);
    assert_eq!(fs.search_attributes(&[eq_kind("material")], 10).unwrap().files.len(), 1);
    assert_eq!(fs.file_attributes("/other/resources/shader.hlsl").unwrap()
        .get("asset.kind"), Some(&kind("material")));
}

#[test]
fn unlink_removes_live_postings_but_pins_keep_original_rows() {
    let mut fs = fresh();
    fs.mkdir("/a").unwrap();
    fs.write_at("/a/one", 0, b"data").unwrap();
    fs.install_schema(&schema()).unwrap();
    fs.set_attributes("/a/one", &rows("shader", 1)).unwrap();
    let pinned = fs.manifest().generation;
    fs.pin_current().unwrap();
    fs.remove_file("/a/one").unwrap();
    assert!(fs.search_attributes(&[eq_kind("shader")], 10).unwrap().files.is_empty());
    assert_eq!(fs.search_snapshot_attributes(pinned, &[eq_kind("shader")], 10)
        .unwrap().files.len(), 1);
    assert_eq!(fs.read_snapshot_file(pinned, "/a/one").unwrap(), b"data");
    fs = reopen(fs);
    assert_eq!(fs.search_snapshot_attributes(pinned, &[eq_kind("shader")], 10)
        .unwrap().files.len(), 1);
}

#[test]
fn failed_attribute_mutations_do_not_advance_generation() {
    let mut fs = fresh();
    fs.mkdir("/a").unwrap();
    fs.write_at("/a/file", 0, b"foo").unwrap();
    let before = fs.manifest().generation;
    assert!(fs.set_attributes("/a/file", &rows("shader", 1)).is_err());
    assert_eq!(fs.manifest().generation, before);
    fs.install_schema(&schema()).unwrap();
    let generation = fs.manifest().generation;
    assert!(fs.install_schema(&schema()).is_err());
    assert!(fs.set_attributes("/a", &rows("shader", 1)).is_err());
    assert!(fs.set_attributes("/not-here", &rows("shader", 1)).is_err());
    assert!(fs.set_attributes("/a/file", &[("missing".into(), kind("shader"))]).is_err());
    assert_eq!(fs.manifest().generation, generation);
}

#[test]
fn corruption_of_fallback_only_attribute_root_forbids_reuse() {
    let mut fs = fresh();
    fs.mkdir("/a").unwrap();
    fs.write_at("/a/file", 0, b"data").unwrap();
    fs.install_schema(&schema()).unwrap();
    fs.set_attributes("/a/file", &rows("shader", 1)).unwrap();
    let previous = fs.manifest().roots.attribute_rows.unwrap();
    fs.set_attributes("/a/file", &rows("material", 2)).unwrap();
    let newer = fs.manifest().roots.attribute_rows.unwrap();
    assert_ne!(previous, newer);
    let mut disk = fs.into_device().power_loss();
    disk.corrupt_stable_for_testing(previous.address as usize, 60, 0x6d);
    let read_only = ReadOnlyV10::open(disk.power_loss()).unwrap();
    assert_eq!(read_only.root_info().generation, 6);
    assert!(UnifiedV10::open(read_only.into_device()).is_err());
}

#[test]
fn installed_catalog_is_pinned_in_entire_historical_schema_triplet() {
    let mut fs = fresh();
    fs.mkdir("/doc").unwrap();
    fs.write_at("/doc/main", 0, b"v1").unwrap();
    fs.install_schema(&schema()).unwrap();
    fs.set_attributes("/doc/main", &rows("shader", 1)).unwrap();
    let g = fs.manifest().generation;
    let m = *fs.manifest();
    assert!(m.roots.schema.is_some());
    assert!(m.roots.attribute_rows.is_some());
    assert!(m.roots.attribute_postings.is_some());
    fs.pin_current().unwrap();
    let desc = fs.snapshots().unwrap().into_iter().find(|d| d.generation == g).unwrap();
    assert_eq!(desc.attribute_schema_root, m.roots.schema);
    assert_eq!(desc.attribute_rows_root, m.roots.attribute_rows);
    assert_eq!(desc.attribute_postings_root, m.roots.attribute_postings);
    fs.set_attributes("/doc/main", &rows("texture", 2)).unwrap();
    for n in 0..5 {
        fs.write_at("/doc/main", 0, format!("version-{n}").as_bytes()).unwrap();
    }
    assert_eq!(fs.search_snapshot_attributes(g, &[eq_kind("shader")], 10).unwrap().files.len(), 1);
}
