use bfs_ng_storage::{
    AttributeDef, AttributeType, CrashDevice, Error, FileSystem, Predicate, Schema, TextCase, Value,
};

const VID: u128 = 0x3de5_907b_a13e_5524;

fn format(blocks: usize) -> FileSystem<CrashDevice> {
    FileSystem::format(CrashDevice::new(blocks), VID).unwrap()
}
fn reopen(fs: FileSystem<CrashDevice>) -> FileSystem<CrashDevice> {
    FileSystem::open(fs.into_device().power_loss()).unwrap()
}
fn schema() -> Schema {
    Schema::new(vec![AttributeDef {
        id: 1,
        name: "asset.kind".into(),
        ty: AttributeType::Text {
            case: TextCase::AsciiFold,
            max_bytes: 64,
        },
        indexed: true,
    }])
    .unwrap()
}
#[test]
fn rename_directory_preserves_ids_contents_attributes_and_historical_names() {
    let mut fs = format(1000);
    let mut tx = fs.begin();
    tx.install_schema(schema()).unwrap();
    tx.mkdir("/assets").unwrap();
    tx.mkdir("/assets/shaders").unwrap();
    tx.mkdir("/other").unwrap();
    let shader = tx
        .write_file("/assets/shaders/pbr.hlsl", b"pbr-shader")
        .unwrap();
    let texture = tx.write_file("/assets/wood.png", b"image-bytes").unwrap();
    tx.set_attributes(
        "/assets/shaders/pbr.hlsl",
        [("asset.kind".into(), Value::Text("shader".into()))],
    )
    .unwrap();
    tx.set_attributes(
        "/assets/wood.png",
        [("asset.kind".into(), Value::Text("texture".into()))],
    )
    .unwrap();
    assert_eq!(fs.commit(tx).unwrap(), 1);

    let mut tx = fs.begin();
    assert_eq!(tx.pin_snapshot().unwrap(), 1);
    tx.rename("/assets", "/other/resources").unwrap();
    fs.commit(tx).unwrap();

    assert_eq!(
        fs.stat("/other/resources/shaders/pbr.hlsl").unwrap().inode,
        shader
    );
    assert_eq!(fs.stat("/other/resources/wood.png").unwrap().inode, texture);
    assert_eq!(
        fs.read_file("/other/resources/shaders/pbr.hlsl").unwrap(),
        b"pbr-shader"
    );
    assert_eq!(
        fs.read_file("/other/resources/wood.png").unwrap(),
        b"image-bytes"
    );
    assert!(matches!(fs.stat("/assets"), Err(Error::NotFound)));
    assert_eq!(
        fs.file_attributes("/other/resources/shaders/pbr.hlsl")
            .unwrap()
            .get("asset.kind"),
        Some(&Value::Text("shader".into()))
    );
    assert_eq!(
        fs.search_attributes(
            &[Predicate::eq("asset.kind", Value::Text("shader".into())),],
            10
        )
        .unwrap()
        .files
        .len(),
        1
    );
    assert_eq!(
        fs.read_snapshot_file(1, "/assets/shaders/pbr.hlsl")
            .unwrap(),
        b"pbr-shader"
    );
    assert!(matches!(
        fs.read_snapshot_file(1, "/other/resources/shaders/pbr.hlsl"),
        Err(Error::NotFound)
    ));

    fs = reopen(fs);
    assert_eq!(
        fs.stat("/other/resources/shaders/pbr.hlsl").unwrap().inode,
        shader
    );
    assert_eq!(
        fs.read_snapshot_file(1, "/assets/wood.png").unwrap(),
        b"image-bytes"
    );
    assert_eq!(
        fs.file_attributes("/other/resources/wood.png")
            .unwrap()
            .get("asset.kind"),
        Some(&Value::Text("texture".into()))
    );
}

#[test]
fn rename_before_publication_updates_staged_file_and_fails_without_partial_move() {
    let mut fs = format(256);
    let mut tx = fs.begin();
    tx.mkdir("/a").unwrap();
    tx.mkdir("/b").unwrap();
    tx.write_file("/a/file", b"new-payload").unwrap();
    tx.rename("/a/file", "/b/file").unwrap();
    fs.commit(tx).unwrap();
    assert_eq!(fs.read_file("/b/file").unwrap(), b"new-payload");
    assert!(matches!(fs.read_file("/a/file"), Err(Error::NotFound)));

    let mut tx = fs.begin();
    tx.mkdir("/b/child").unwrap();
    tx.write_file("/b/child/one", b"one").unwrap();
    tx.rename("/b/child", "/a/renamed").unwrap();
    fs.commit(tx).unwrap();
    assert_eq!(fs.read_file("/a/renamed/one").unwrap(), b"one");

    let initial = fs.generation();
    let mut tx = fs.begin();
    for (source, destination, err) in [
        ("/a/renamed", "/a/renamed/sub", "cycle"),
        ("/a/renamed", "/missing/child", "missing parent"),
        ("/b/file", "/a/renamed/one", "destination exists"),
        ("/b/file", "/.bfs_f4_attrs", "private namespace"),
    ] {
        let r = tx.rename(source, destination);
        assert!(r.is_err(), "{err}");
    }
    // An invalid move cannot partially rewrite a descendant's name.
    assert_eq!(fs.commit(tx).unwrap(), initial);
    assert_eq!(fs.read_file("/a/renamed/one").unwrap(), b"one");
}

#[test]
fn rename_can_not_migrate_children_to_overlong_paths_or_alias_other_records() {
    let mut fs = format(500);
    let mut tx = fs.begin();
    tx.mkdir("/src").unwrap();
    tx.write_file("/src/child", b"child").unwrap();
    tx.mkdir("/dest").unwrap();
    fs.commit(tx).unwrap();
    let previous = fs.generation();
    let mut tx = fs.begin();
    let long = format!("/{}", "q".repeat(235));
    assert!(matches!(tx.rename("/src", &long), Err(Error::InvalidPath)));
    assert_eq!(fs.commit(tx).unwrap(), previous);
    assert_eq!(fs.read_file("/src/child").unwrap(), b"child");
}

#[test]
fn v9_partial_write_and_truncate_preserve_snapshots_with_bounded_file_growth() {
    let mut fs = format(1900);
    let original = vec![0x43; 512 * 1024 + 17];
    let mut tx = fs.begin();
    tx.write_file("/large", &original).unwrap();
    fs.commit(tx).unwrap();
    assert_eq!(
        fs.read_at("/large", original.len() as u64 - 8, 30).unwrap(),
        vec![0x43; 8]
    );
    let generation = fs.generation();
    let mut tx = fs.begin();
    tx.pin_snapshot().unwrap();
    fs.commit(tx).unwrap();

    let offset = 4092_u64;
    let patch = vec![0x7a; 17];
    fs.write_at("/large", offset, &patch).unwrap();
    let mut expected = original;
    expected[offset as usize..offset as usize + patch.len()].copy_from_slice(&patch);
    assert_eq!(fs.read_file("/large").unwrap(), expected);
    assert_eq!(
        fs.read_snapshot_file(generation, "/large").unwrap().len(),
        expected.len()
    );

    let expanded = expected.len() as u64 + 23;
    fs.truncate("/large", expanded).unwrap();
    let bytes = fs.read_file("/large").unwrap();
    assert_eq!(bytes.len(), expanded as usize);
    assert_eq!(&bytes[..expected.len()], expected);
    assert!(bytes[expected.len()..].iter().all(|&b| b == 0));
    assert_eq!(
        fs.read_at("/large", expanded + 1, 100).unwrap(),
        Vec::<u8>::new()
    );

    fs = reopen(fs);
    assert_eq!(fs.read_file("/large").unwrap(), bytes);
    assert_eq!(
        fs.read_snapshot_file(generation, "/large").unwrap().len(),
        expected.len()
    );

    assert!(matches!(
        fs.write_at("/large", 4 * 1024 * 1024, b"x"),
        Err(Error::TooLarge)
    ));
    assert!(matches!(
        fs.truncate("/large", 4 * 1024 * 1024 + 1),
        Err(Error::TooLarge)
    ));
}

#[test]
fn failed_partial_write_does_not_publish_and_new_file_holes_are_zero() {
    let mut fs = format(300);
    let mut tx = fs.begin();
    tx.mkdir("/data").unwrap();
    fs.commit(tx).unwrap();
    let before = fs.generation();
    assert!(matches!(
        fs.write_at("/absent/file", 0, b"bad"),
        Err(Error::NotFound)
    ));
    assert_eq!(fs.generation(), before);
    fs.write_at("/data/new", 8, b"ok").unwrap();
    assert_eq!(fs.read_file("/data/new").unwrap(), b"\0\0\0\0\0\0\0\0ok");
    let current = fs.generation();
    assert_eq!(fs.write_at("/data/new", 0, b"").unwrap(), current);
    assert_eq!(fs.truncate("/data/new", 10).unwrap(), current);
}
