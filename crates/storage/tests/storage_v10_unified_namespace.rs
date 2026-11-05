use bfs_ng_storage::experimental_v10::{
    NamespaceRecord, ReadOnlyV10, SnapshotState, UnifiedV10,
    read_namespace_tree, stage_namespace_tree,
};
use bfs_ng_storage::{BLOCK_SIZE, BlockDevice, CrashDevice, Error, Fault, FileSystem};
use std::collections::BTreeMap;

const VID: u128 = 0xe618_7935_4400_1adf;

fn fresh(n: usize) -> UnifiedV10<CrashDevice> {
    UnifiedV10::format(CrashDevice::new(n), VID).unwrap()
}
fn reopen(fs: UnifiedV10<CrashDevice>) -> UnifiedV10<CrashDevice> {
    UnifiedV10::open(fs.into_device().power_loss()).unwrap()
}

#[test]
fn one_v10_manifest_commits_directories_files_extents_checksums_and_snapshots() {
    let mut fs = fresh(1800);
    assert!(fs.list_dir("/").unwrap().is_empty());
    fs.mkdir("/assets").unwrap();
    fs.mkdir("/assets/shaders").unwrap();
    fs.mkdir("/assets/materials").unwrap();
    let shader = b"void main() { color = vec4(1); }";
    fs.write_at("/assets/shaders/pbr.hlsl", 0, shader).unwrap();
    fs.write_at("/assets/materials/wood.json", 7, b"wood").unwrap();
    let shader_id = fs.stat("/assets/shaders/pbr.hlsl").unwrap().0;
    let material_id = fs.stat("/assets/materials/wood.json").unwrap().0;
    assert_ne!(shader_id, material_id);
    assert_eq!(fs.read_file("/assets/shaders/pbr.hlsl").unwrap(), shader);
    assert_eq!(fs.read_file("/assets/materials/wood.json").unwrap(), b"\0\0\0\0\0\0\0wood");
    let pinned = fs.manifest().generation;
    fs.pin_current().unwrap();

    fs.rename("/assets", "/project").unwrap();
    assert!(matches!(fs.stat("/assets"), Err(Error::NotFound)));
    assert_eq!(fs.stat("/project/shaders/pbr.hlsl").unwrap().0, shader_id);
    assert_eq!(fs.stat("/project/materials/wood.json").unwrap().0, material_id);
    fs.write_at("/project/shaders/pbr.hlsl", 5, b"stage").unwrap();
    assert_eq!(fs.read_snapshot_file(pinned, "/assets/shaders/pbr.hlsl").unwrap(), shader);
    assert!(matches!(
        fs.read_snapshot_file(pinned, "/project/shaders/pbr.hlsl"),
        Err(Error::NotFound)
    ));
    fs = reopen(fs);
    assert_eq!(fs.stat("/project/materials/wood.json").unwrap().0, material_id);
    assert_eq!(fs.read_snapshot_file(pinned, "/assets/shaders/pbr.hlsl").unwrap(), shader);
    assert_eq!(fs.list_dir("/project").unwrap(), vec![
        "/project/materials".to_owned(), "/project/shaders".to_owned(),
    ]);
    fs.mark_dropping(pinned).unwrap();
    assert_eq!(fs.snapshots().unwrap()[0].state, SnapshotState::Dropping);
    assert_eq!(fs.read_snapshot_file(pinned, "/assets/shaders/pbr.hlsl").unwrap(), shader);
}

#[test]
fn multiple_file_updates_do_not_collapse_independent_extent_roots() {
    let mut fs = fresh(2500);
    fs.mkdir("/data").unwrap();
    let first = vec![0x52; 3 * BLOCK_SIZE + 11];
    let second = vec![0x9f; 5 * BLOCK_SIZE + 19];
    fs.write_at("/data/a", 0, &first).unwrap();
    fs.write_at("/data/b", 0, &second).unwrap();
    let a_id = fs.stat("/data/a").unwrap().0;
    let b_id = fs.stat("/data/b").unwrap().0;
    let snap = fs.manifest().generation;
    fs.pin_current().unwrap();
    fs.write_at("/data/a", BLOCK_SIZE as u64 + 103, &[0x55; 83]).unwrap();
    fs.truncate("/data/b", 2 * BLOCK_SIZE as u64 + 17).unwrap();
    assert_eq!(fs.read_snapshot_file(snap, "/data/a").unwrap(), first);
    assert_eq!(fs.read_snapshot_file(snap, "/data/b").unwrap(), second);
    assert_eq!(fs.stat("/data/a").unwrap().0, a_id);
    assert_eq!(fs.stat("/data/b").unwrap().0, b_id);
    assert_eq!(fs.read_file("/data/b").unwrap().len(), 2 * BLOCK_SIZE + 17);
    fs = reopen(fs);
    assert_eq!(fs.read_snapshot_file(snap, "/data/b").unwrap(), second);
}

#[test]
fn sparse_and_randomized_byte_operations_match_reference_files() {
    let mut fs = fresh(10000);
    fs.mkdir("/corpus").unwrap();
    let mut source = BTreeMap::<String, Vec<u8>>::new();
    for i in 0..5 {
        let name = format!("/corpus/file{i}");
        fs.write_at(&name, 0, b"seed").unwrap();
        source.insert(name, b"seed".to_vec());
    }
    let mut seed = 0x451a_996c_u64;
    for step in 0..90 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let idx = (seed as usize) % 5;
        let path = format!("/corpus/file{idx}");
        if step % 7 == 0 {
            let size = ((seed >> 11) as usize % (6 * BLOCK_SIZE)) as u64;
            fs.truncate(&path, size).unwrap();
            source.get_mut(&path).unwrap().resize(size as usize, 0);
        } else {
            let pos = (seed >> 17) as usize % (8 * BLOCK_SIZE);
            let len = ((seed >> 7) as usize % 350) + 1;
            let bytes: Vec<u8> = (0..len)
                .map(|n| (n as u8).wrapping_add(step as u8)).collect();
            fs.write_at(&path, pos as u64, &bytes).unwrap();
            let model = source.get_mut(&path).unwrap();
            if model.len() < pos + len { model.resize(pos + len, 0); }
            model[pos..pos + len].copy_from_slice(&bytes);
        }
        if step % 13 == 0 {
            fs = reopen(fs);
            for (name, data) in &source {
                assert_eq!(fs.read_file(name).unwrap(), *data, "step {step} {name}");
            }
        }
    }
    for (path, data) in source {
        assert_eq!(fs.read_file(&path).unwrap(), data);
    }
}

#[test]
fn namespace_builder_creates_and_validates_multilevel_tree() {
    let mut disk = CrashDevice::new(4000);
    let mut entries = BTreeMap::new();
    entries.insert("/generated".into(), NamespaceRecord::directory(2));
    for i in 0..200_u64 {
        entries.insert(
            format!("/generated/item_{i:04}"),
            NamespaceRecord::file(i + 3, None),
        );
    }
    let reservations: Vec<u64> = (0..40).map(|n| 3999 - n).collect();
    let staged = stage_namespace_tree(
        &entries, &reservations, VID, 2, 204, 4000, 4000,
    ).unwrap();
    assert!(staged.root.level > 0);
    let root = staged.root;
    for (address, bytes) in staged.pages {
        disk.write_block(address, &bytes).unwrap();
    }
    disk.sync().unwrap();
    let checked = read_namespace_tree(&mut disk, root, VID, 2, 204).unwrap();
    assert_eq!(checked.records, entries);
    assert!(checked.metadata_pages.len() >= 4);
    // A direct namespace leaf read is not a substitute for full traversal.
    let victim = checked.metadata_pages.iter().min().copied().unwrap();
    disk.corrupt_stable_for_testing(victim as usize, 35, 0xa5);
    assert!(read_namespace_tree(&mut disk.power_loss(), root, VID, 2, 204).is_err());
}

#[test]
fn invalid_moves_do_not_publish_or_change_live_paths() {
    let mut fs = fresh(600);
    fs.mkdir("/a").unwrap();
    fs.mkdir("/b").unwrap();
    fs.mkdir("/a/child").unwrap();
    fs.write_at("/a/child/file", 0, b"data").unwrap();
    let generation = fs.manifest().generation;
    assert!(matches!(fs.rename("/a", "/a/child/nested"), Err(Error::InvalidPath)));
    assert!(matches!(fs.rename("/a", "/b"), Err(Error::AlreadyExists)));
    assert!(matches!(fs.rename("/a", "/missing/place"), Err(Error::NotFound)));
    assert!(matches!(fs.write_at("/a/child", 0, b"x"), Err(Error::IsDirectory)));
    assert!(matches!(fs.remove_file("/a"), Err(Error::IsDirectory)));
    assert_eq!(fs.manifest().generation, generation);
    assert_eq!(fs.read_file("/a/child/file").unwrap(), b"data");
}

#[test]
fn interrupted_publication_cannot_mix_directory_and_data_versions() {
    for operation in 1..=8 {
        for torn_bytes in [0, 16, 124, BLOCK_SIZE] {
            let mut fs = fresh(500);
            fs.mkdir("/test").unwrap();
            fs.write_at("/test/file", 0, b"old").unwrap();
            let before = fs.manifest().generation;
            let mut disk = fs.into_device().power_loss();
            disk.inject(Fault { operation, torn_bytes: Some(torn_bytes) }).unwrap();
            fs = UnifiedV10::open(disk).unwrap();
            assert!(matches!(
                fs.write_at("/test/file", 0, b"new"),
                Err(Error::InjectedCrash)
            ));
            let disk = fs.into_device().power_loss();
            let reader = ReadOnlyV10::open(disk).unwrap();
            let selected = reader.root_info().generation;
            assert!(matches!(selected, n if n == before || n == before + 1));
            if let Ok(mut recovered) = UnifiedV10::open(reader.into_device()) {
                assert_eq!(
                    recovered.read_file("/test/file").unwrap(),
                    if selected == before { b"old" } else { b"new" }
                );
            }
        }
    }
}

#[test]
fn limited_space_does_not_tear_staged_directory_or_file_changes() {
    let mut fs = fresh(13);
    fs.mkdir("/assets").unwrap();
    fs.write_at("/assets/a", 0, b"one").unwrap();
    let before = fs.manifest().generation;
    assert!(matches!(
        fs.write_at("/assets/a", 0, &[0x7a; BLOCK_SIZE]),
        Err(Error::NoSpace)
    ));
    assert_eq!(fs.manifest().generation, before);
    let mut fs = reopen(fs);
    assert_eq!(fs.read_file("/assets/a").unwrap(), b"one");
}

#[test]
fn v9_and_v10_agree_on_ordinary_namespace_and_inode_semantics() {
    let mut old = FileSystem::format(CrashDevice::new(800), VID).unwrap();
    let mut new = fresh(800);
    let mut tx = old.begin();
    tx.mkdir("/assets").unwrap();
    old.commit(tx).unwrap();
    new.mkdir("/assets").unwrap();
    let mut tx = old.begin();
    let a_id = tx.write_file("/assets/a", b"alpha").unwrap();
    let b_id = tx.write_file("/assets/b", b"beta").unwrap();
    old.commit(tx).unwrap();
    new.write_at("/assets/a", 0, b"alpha").unwrap();
    new.write_at("/assets/b", 0, b"beta").unwrap();
    assert_eq!(old.stat("/assets/a").unwrap().inode, a_id);
    assert_eq!(new.stat("/assets/a").unwrap().0, a_id);
    assert_eq!(new.stat("/assets/b").unwrap().0, b_id);

    let old_snap = old.generation();
    let new_snap = new.manifest().generation;
    let mut tx = old.begin();
    tx.pin_snapshot().unwrap();
    tx.rename("/assets", "/sources").unwrap();
    old.commit(tx).unwrap();
    new.pin_current().unwrap();
    new.rename("/assets", "/sources").unwrap();
    for path in ["/sources/a", "/sources/b"] {
        assert_eq!(old.read_file(path).unwrap(), new.read_file(path).unwrap());
        assert_eq!(old.stat(path).unwrap().inode, new.stat(path).unwrap().0);
    }
    assert_eq!(old.read_snapshot_file(old_snap, "/assets/a").unwrap(),
               new.read_snapshot_file(new_snap, "/assets/a").unwrap());
}
