use bfs_ng_storage::{BLOCK_SIZE, BlockDevice, CrashDevice, Error, Fault, FileSystem, Volume};
use std::collections::{BTreeMap, BTreeSet};

fn fresh(blocks: usize) -> FileSystem<CrashDevice> {
    FileSystem::format(CrashDevice::new(blocks), 0xf333).unwrap()
}
fn reopen(fs: FileSystem<CrashDevice>) -> FileSystem<CrashDevice> {
    FileSystem::open(fs.into_device().power_loss()).unwrap()
}
fn bulk(fs: &mut FileSystem<CrashDevice>, n: usize) {
    let mut tx = fs.begin();
    tx.mkdir("/dataset").unwrap();
    for i in 0..n {
        tx.write_file(
            &format!("/dataset/doc_{i:04}"),
            format!("original:{i}").as_bytes(),
        )
        .unwrap();
    }
    fs.commit(tx).unwrap();
}

fn u16_at(b: &[u8], at: usize) -> usize {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap()) as usize
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// Independent test-only walk of physical node addresses, not F3's patcher.
fn reachable_tree(device: &mut impl BlockDevice, root: u64) -> BTreeSet<u64> {
    let mut found = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(addr) = stack.pop() {
        if addr == 0 {
            continue;
        }
        assert!(found.insert(addr), "duplicate physical node {addr}");
        let mut block = [0; BLOCK_SIZE];
        device.read_block(addr, &mut block).unwrap();
        assert_eq!(&block[..8], b"BFSNGN1\0");
        if block[8] > 0 {
            let mut offset = 32;
            for _ in 0..u16_at(&block, 10) {
                let len = u16_at(&block, offset);
                offset += 2 + len;
                stack.push(u64_at(&block, offset));
                offset += 8;
            }
        }
    }
    found
}

#[test]
fn single_update_copies_ancestor_path_and_shares_untouched_nodes() {
    let mut fs = fresh(2048);
    bulk(&mut fs, 150);
    assert!(fs.last_metadata_blocks_written() <= 20);
    let old_inode = fs.stat("/dataset/doc_0075").unwrap().inode;
    let mut tx = fs.begin();
    tx.pin_snapshot().unwrap();
    tx.write_file("/dataset/doc_0075", b"revised").unwrap();
    fs.commit(tx).unwrap();
    assert!(
        fs.last_metadata_blocks_written() <= 3,
        "single-key update must not rebuild the 150-record tree"
    );
    let mut volume = Volume::open(fs.into_device()).unwrap();
    let manifest = volume.read_root().unwrap().unwrap();
    let live_root = u64_at(&manifest, 24);
    let old_root = u64_at(&manifest, 64 + 8);
    let mut dev = volume.into_device();
    let old_nodes = reachable_tree(&mut dev, old_root);
    let new_nodes = reachable_tree(&mut dev, live_root);
    let shared = old_nodes.intersection(&new_nodes).count();
    assert!(shared >= old_nodes.len() - 3, "lost unchanged node sharing");
    let mut fs = FileSystem::open(dev.power_loss()).unwrap();
    assert_eq!(fs.read_file("/dataset/doc_0075").unwrap(), b"revised");
    assert_eq!(
        fs.read_snapshot_file(1, "/dataset/doc_0075").unwrap(),
        b"original:75"
    );
    assert_eq!(fs.stat("/dataset/doc_0075").unwrap().inode, old_inode);
    assert_eq!(fs.read_file("/dataset/doc_0149").unwrap(), b"original:149");
}

#[test]
fn snapshot_only_commit_copies_no_tree_nodes() {
    let mut fs = fresh(256);
    bulk(&mut fs, 48);
    let root = fs.stat("/dataset/doc_0002").unwrap().inode;
    let mut tx = fs.begin();
    tx.pin_snapshot().unwrap();
    fs.commit(tx).unwrap();
    assert_eq!(fs.last_metadata_blocks_written(), 0);
    let mut fs = reopen(fs);
    assert_eq!(fs.stat("/dataset/doc_0002").unwrap().inode, root);
    assert_eq!(
        fs.read_snapshot_file(1, "/dataset/doc_0002").unwrap(),
        b"original:2"
    );
    let mut tx = fs.begin();
    tx.drop_snapshot(1).unwrap();
    fs.commit(tx).unwrap();
    assert_eq!(fs.last_metadata_blocks_written(), 0);
}

#[test]
fn leaf_splits_and_subtree_deletions_keep_search_order() {
    let mut fs = fresh(1024);
    bulk(&mut fs, 25);
    // Insert into middle of a full leaf range, triggering a split.
    let mut tx = fs.begin();
    tx.write_file("/dataset/doc_0008a", b"split-member")
        .unwrap();
    tx.write_file("/dataset/doc_0013a", b"middle-member")
        .unwrap();
    fs.commit(tx).unwrap();
    assert_eq!(fs.read_file("/dataset/doc_0008a").unwrap(), b"split-member");
    fs = reopen(fs);
    assert_eq!(fs.list_dir("/dataset").unwrap().len(), 27);
    for i in 0..25 {
        assert_eq!(
            fs.read_file(&format!("/dataset/doc_{i:04}")).unwrap(),
            format!("original:{i}").as_bytes()
        );
    }
    let mut tx = fs.begin();
    for i in 0..24 {
        tx.remove_file(&format!("/dataset/doc_{i:04}")).unwrap();
    }
    tx.remove_file("/dataset/doc_0008a").unwrap();
    tx.remove_file("/dataset/doc_0013a").unwrap();
    fs.commit(tx).unwrap();
    fs = reopen(fs);
    assert_eq!(fs.list_dir("/dataset").unwrap(), vec!["/dataset/doc_0024"]);
    let mut tx = fs.begin();
    tx.remove_file("/dataset/doc_0024").unwrap();
    tx.rmdir("/dataset").unwrap();
    fs.commit(tx).unwrap();
    fs = reopen(fs);
    assert_eq!(fs.file_count(), 0);
    assert!(fs.list_dir("/").unwrap().is_empty());
}

#[test]
fn randomized_index_mutations_match_independent_map_after_reopen() {
    let mut fs = fresh(4096);
    bulk(&mut fs, 120);
    let mut oracle = BTreeMap::<String, Vec<u8>>::new();
    for i in 0..120 {
        oracle.insert(
            format!("/dataset/doc_{i:04}"),
            format!("original:{i}").into_bytes(),
        );
    }
    let mut rng = 0x7834_ba21_ec09_4613u64;
    for step in 0..200 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let id = rng % 155;
        let key = format!("/dataset/doc_{id:04}");
        let mut tx = fs.begin();
        if step % 7 == 0 && oracle.contains_key(&key) {
            tx.remove_file(&key).unwrap();
            oracle.remove(&key);
        } else {
            let value = format!("changed:{step}:{id}").into_bytes();
            tx.write_file(&key, &value).unwrap();
            oracle.insert(key.clone(), value);
        }
        fs.commit(tx).unwrap();
        if step % 13 == 0 {
            fs = reopen(fs);
        }
        assert_eq!(fs.list_dir("/dataset").unwrap().len(), oracle.len());
        if step % 17 == 0 {
            for (key, expected) in &oracle {
                assert_eq!(
                    &fs.read_file(key).unwrap(),
                    expected,
                    "step {step}, key {key}"
                );
            }
        }
    }
    fs = reopen(fs);
    assert_eq!(fs.list_dir("/dataset").unwrap().len(), oracle.len());
    for (key, expected) in &oracle {
        assert_eq!(&fs.read_file(key).unwrap(), expected);
    }
}

#[test]
fn multi_edit_commit_discards_intermediate_staged_tree_paths() {
    let mut fs = fresh(2048);
    bulk(&mut fs, 150);
    let mut tx = fs.begin();
    for i in [1, 9, 17, 40, 72, 109, 149] {
        tx.write_file(
            &format!("/dataset/doc_{i:04}"),
            format!("new:{i}").as_bytes(),
        )
        .unwrap();
    }
    fs.commit(tx).unwrap();
    assert!(
        fs.last_metadata_blocks_written() <= 17,
        "reused ancestors should not be written more than once"
    );
    fs = reopen(fs);
    for i in [1, 9, 17, 40, 72, 109, 149] {
        assert_eq!(
            fs.read_file(&format!("/dataset/doc_{i:04}")).unwrap(),
            format!("new:{i}").as_bytes()
        );
    }
}

#[test]
fn path_copy_faults_accept_only_complete_old_or_new_file() {
    // A 150-file tree has 3 levels. One data chunk, 3 new metadata nodes,
    // stage sync, then F0's four root operations => nine fault points.
    for fail_at in 1..=9 {
        for torn_bytes in [None, Some(24), Some(124), Some(BLOCK_SIZE)] {
            let mut fs = fresh(1024);
            bulk(&mut fs, 150);
            let mut d = fs.into_device();
            d.inject(Fault {
                operation: fail_at,
                torn_bytes,
            })
            .unwrap();
            let mut fs = FileSystem::open(d).unwrap();
            let mut tx = fs.begin();
            tx.write_file("/dataset/doc_0075", b"new-atomic").unwrap();
            assert!(
                matches!(fs.commit(tx), Err(Error::InjectedCrash)),
                "unexpected commit at operation {fail_at}"
            );
            let mut fs = reopen(fs);
            let value = fs.read_file("/dataset/doc_0075").unwrap();
            assert!(
                value == b"original:75" || value == b"new-atomic",
                "partial commit at operation {fail_at} torn {torn_bytes:?}"
            );
            assert_eq!(fs.read_file("/dataset/doc_0076").unwrap(), b"original:76");
        }
    }
}

#[test]
fn shared_nodes_are_protected_by_snapshot_and_reclamation() {
    let mut fs = fresh(2048);
    bulk(&mut fs, 130);
    let mut tx = fs.begin();
    tx.pin_snapshot().unwrap();
    tx.write_file("/dataset/doc_0007", b"revised-2").unwrap();
    fs.commit(tx).unwrap();
    for n in 3..12 {
        let mut tx = fs.begin();
        tx.write_file("/dataset/doc_0007", format!("revised-{n}").as_bytes())
            .unwrap();
        fs.commit(tx).unwrap();
    }
    let mut fs = reopen(fs);
    assert_eq!(
        fs.read_snapshot_file(1, "/dataset/doc_0007").unwrap(),
        b"original:7"
    );
    assert_eq!(
        fs.read_snapshot_file(1, "/dataset/doc_0129").unwrap(),
        b"original:129"
    );
    assert_eq!(fs.read_file("/dataset/doc_0007").unwrap(), b"revised-11");
}
