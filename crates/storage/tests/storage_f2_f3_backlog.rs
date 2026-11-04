//! F2/F3 backlog closeout: allocation reachability and on-disk B+tree ranges.
use bfs_ng_storage::{
    BLOCK_SIZE, BlockDevice, CrashDevice, Error, Fault, FileSystem, Volume,
    AttributeDef, AttributeType, Predicate, Schema, TextCase, Value,
};
use std::collections::BTreeSet;

fn fs(blocks: usize) -> FileSystem<CrashDevice> {
    FileSystem::format(CrashDevice::new(blocks), 0xf233).unwrap()
}
fn reopen(fs: FileSystem<CrashDevice>) -> FileSystem<CrashDevice> {
    FileSystem::open(fs.into_device().power_loss()).unwrap()
}
fn populate(fs: &mut FileSystem<CrashDevice>, count: usize) {
    let mut tx = fs.begin();
    tx.mkdir("/dataset").unwrap();
    for n in 0..count {
        tx.write_file(
            &format!("/dataset/doc_{n:04}"),
            format!("source:{n}").as_bytes(),
        ).unwrap();
    }
    fs.commit(tx).unwrap();
}
fn read_u16(block: &[u8], off: usize) -> usize {
    u16::from_le_bytes(block[off..off+2].try_into().unwrap()) as usize
}
fn read_u64(block: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(block[off..off+8].try_into().unwrap())
}
fn current_tree(device: CrashDevice) -> (CrashDevice, u64) {
    let mut volume = Volume::open(device).unwrap();
    let manifest = volume.read_root().unwrap().unwrap();
    let root = read_u64(&manifest, 24);
    (volume.into_device(), root)
}
fn tree_occupancy(
    device: &mut CrashDevice,
    address: u64,
    is_root: bool,
    visited: &mut BTreeSet<u64>,
) -> (usize, usize) {
    assert!(visited.insert(address), "cycle or duplicate node at {address}");
    let mut data = [0; BLOCK_SIZE];
    device.read_block(address, &mut data).unwrap();
    assert_eq!(&data[..8], b"BFSNGN1\0");
    let count = read_u16(&data, 10);
    assert!(count <= 12 && count > 0);
    if !is_root {
        assert!(count >= 6, "underfilled non-root node {address}: {count}");
    }
    if data[8] == 0 {
        return (1, 1);
    }
    let mut total = 1;
    let mut leaves = 0;
    let mut at = 32;
    for _ in 0..count {
        let len = read_u16(&data, at);
        at += 2 + len;
        let child = read_u64(&data, at);
        at += 8;
        let (nodes, leaf_count) = tree_occupancy(device, child, false, visited);
        total += nodes;
        leaves += leaf_count;
    }
    (total, leaves)
}

#[test]
fn bulk_load_is_balanced_at_all_tree_levels() {
    for count in [13, 25, 150, 257] {
        let mut image = fs(1024);
        populate(&mut image, count);
        let device = image.into_device().power_loss();
        let (mut device, root) = current_tree(device);
        let mut seen = BTreeSet::new();
        let (_, leaves) = tree_occupancy(&mut device, root, true, &mut seen);
        assert!(leaves >= 2);
        let live = FileSystem::open(device).unwrap();
        assert_eq!(live.file_count(), count+1);
    }
}

#[test]
fn repeated_deletions_rebalance_and_collapse_tree_without_losing_files() {
    let mut image = fs(2048);
    populate(&mut image, 180);
    let mut tx = image.begin();
    // Remove across first/middle/last subtrees, then test sibling borrowing.
    for n in 0..150 {
        tx.remove_file(&format!("/dataset/doc_{n:04}")).unwrap();
    }
    image.commit(tx).unwrap();
    assert_eq!(image.list_dir("/dataset").unwrap().len(),30);
    let dev = image.into_device().power_loss();
    let (mut dev, root) = current_tree(dev);
    let mut seen = BTreeSet::new();
    tree_occupancy(&mut dev, root, true, &mut seen);
    let mut image = FileSystem::open(dev).unwrap();
    for n in 150..180 {
        assert_eq!(image.read_file(&format!("/dataset/doc_{n:04}")).unwrap(),
            format!("source:{n}").as_bytes());
    }
    let mut tx = image.begin();
    for n in 150..180 {
        tx.remove_file(&format!("/dataset/doc_{n:04}")).unwrap();
    }
    tx.rmdir("/dataset").unwrap();
    image.commit(tx).unwrap();
    image = reopen(image);
    assert_eq!(image.file_count(), 0);
    assert!(image.list_dir("/").unwrap().is_empty());
}

#[test]
fn disk_pages_match_logical_namespace_and_skip_unrelated_subtrees() {
    let mut image = fs(2048);
    populate(&mut image, 180);
    let expected = image.list_dir("/dataset").unwrap();
    let generation = image.generation();
    let mut collected = Vec::new();
    let mut after: Option<String> = None;
    let mut total_reads = 0usize;
    loop {
        let page = image.scan_files_disk(generation, "/dataset/", after.as_deref(), 7).unwrap();
        assert_eq!(page.generation, generation);
        total_reads += page.metadata_pages_read;
        for (path, stat) in page.entries {
            assert!(!stat.directory);
            assert_eq!(stat.generation, generation);
            collected.push(path);
        }
        after = page.next_after;
        if after.is_none() { break; }
    }
    assert_eq!(collected, expected);
    assert!(total_reads > 0);
    let selective = image.scan_files_disk(
        generation, "/dataset/doc_0127", None, 10,
    ).unwrap();
    assert_eq!(selective.entries.len(),1);
    assert_eq!(selective.entries[0].0, "/dataset/doc_0127");
    assert!(selective.metadata_pages_read < 10,
        "selective disk seek should avoid unrelated leaves");
    assert!(matches!(
        image.scan_files_disk(generation + 1, "/dataset/", None, 7),
        Err(Error::StaleTransaction)
    ));
    assert!(matches!(
        image.scan_files_disk(generation, "/dataset/", Some("/wrong"), 7),
        Err(Error::InvalidPath)
    ));
    assert!(matches!(
        image.scan_files_disk(generation, "/dataset/", None, 0),
        Err(Error::TooLarge)
    ));
    image = reopen(image);
    assert_eq!(image.scan_files_disk(image.generation(), "/dataset/doc_0127", None, 5)
        .unwrap().entries.len(), 1);
}

#[test]
fn pinned_snapshot_disk_cursors_survive_heavy_live_deletions() {
    let mut image = fs(2048);
    populate(&mut image, 120);
    let mut tx = image.begin();
    tx.pin_snapshot().unwrap();
    for n in 0..100 {
        tx.remove_file(&format!("/dataset/doc_{n:04}")).unwrap();
    }
    image.commit(tx).unwrap();
    image = reopen(image);
    assert_eq!(image.scan_snapshot_files_disk(1, "/dataset/", None, 512)
        .unwrap().entries.len(),120);
    assert_eq!(image.scan_files_disk(image.generation(), "/dataset/", None, 512)
        .unwrap().entries.len(),20);
    let mut tx = image.begin();
    tx.drop_snapshot(1).unwrap();
    image.commit(tx).unwrap();
    assert!(matches!(
        image.scan_snapshot_files_disk(1, "/dataset/", None, 10),
        Err(Error::InvalidSnapshot)
    ));
}

#[test]
fn f2_space_audit_reports_shared_snapshot_blocks_and_eventual_reuse() {
    let mut image = fs(128);
    let fresh = image.allocation_report().unwrap();
    assert_eq!(fresh.referenced_blocks, 0);
    assert_eq!(fresh.reclaimable_blocks, 0);
    for n in 0..3 {
        let mut tx=image.begin();
        tx.write_file("/data", format!("rev:{n}").as_bytes()).unwrap();
        if n==1 { tx.pin_snapshot().unwrap(); }
        image.commit(tx).unwrap();
    }
    let report = image.allocation_report().unwrap();
    assert!(report.root_candidates>=1);
    assert!(report.historical_roots>=1);
    assert!(report.referenced_blocks>=4);
    assert!(report.root_generations_remaining>0);
    assert!(!report.reuse_blocked);
    let pinned_file = image.read_snapshot_file(1, "/data").unwrap();
    assert_eq!(pinned_file,b"rev:0");
    let mut tx=image.begin();tx.drop_snapshot(1).unwrap();image.commit(tx).unwrap();
    for n in 4..9 {
        let mut tx=image.begin();
        tx.write_file("/data", format!("rev:{n}").as_bytes()).unwrap();
        image.commit(tx).unwrap();
    }
    let report=image.allocation_report().unwrap();
    assert!(report.reclaimable_blocks>0);
    assert!(!report.reuse_blocked);
    let mut image=reopen(image);
    assert_eq!(image.read_file("/data").unwrap(),b"rev:8");
    assert_eq!(image.allocation_report().unwrap().generation, image.generation());
}

#[test]
fn f2_quarantines_reuse_if_newest_f0_candidate_has_corrupt_data() {
    let mut image = fs(128);
    for n in 0..3 {
        let mut tx=image.begin();
        tx.write_file("/data", format!("rev{n}").as_bytes()).unwrap();
        image.commit(tx).unwrap();
    }
    let mut device = image.into_device().power_loss();
    // Corrupt latest file data but leave its F0 root slot checksummed.
    let mut latest = None;
    for at in 3..device.block_count() {
        let mut block = [0; BLOCK_SIZE];
        device.read_block(at, &mut block).unwrap();
        if &block[..8] == b"BFSNGD1\0" && &block[32..36] == b"rev2" {
            latest = Some(at as usize);
        }
    }
    device.corrupt_stable_for_testing(latest.expect("latest data block"), 34, 0xaa);
    let mut recovered = FileSystem::open(device).unwrap();
    assert_eq!(recovered.generation(), 2);
    let report = recovered.allocation_report().unwrap();
    assert!(report.reuse_blocked);
    assert_eq!(report.reclaimable_blocks,0);
    let mut tx=recovered.begin();
    tx.write_file("/data",b"repair").unwrap();
    recovered.commit(tx).unwrap(); // fresh blocks only
    assert_eq!(reopen(recovered).read_file("/data").unwrap(),b"repair");
}

#[test]
fn f4_attributes_and_snapshot_queries_survive_balanced_namespace_deletes() {
    let mut image = fs(4096);
    let schema = Schema::new(vec![AttributeDef {
        id: 1, name: "asset.kind".into(),
        ty: AttributeType::Text { case: TextCase::AsciiFold, max_bytes: 32 },
        indexed: true,
    }]).unwrap();
    let mut tx = image.begin();
    tx.install_schema(schema).unwrap();
    tx.mkdir("/dve").unwrap();
    for n in 0..100 {
        let path=format!("/dve/shader_{n:04}");
        tx.write_file(&path,b"shader").unwrap();
        tx.set_attributes(&path,vec![
            ("asset.kind".into(),Value::Text("SHADER".into())),
        ]).unwrap();
    }
    image.commit(tx).unwrap();
    let mut tx=image.begin();
    tx.pin_snapshot().unwrap();
    for n in 0..75 {
        tx.remove_file(&format!("/dve/shader_{n:04}")).unwrap();
    }
    image.commit(tx).unwrap();
    let query = [Predicate::eq("asset.kind",Value::Text("shader".into()))];
    assert_eq!(image.search_attributes(&query, 500).unwrap().files.len(),25);
    assert_eq!(image.search_snapshot_attributes(1,&query,500).unwrap().files.len(),100);
    let dev=image.into_device().power_loss();
    let (mut dev, root)=current_tree(dev);
    tree_occupancy(&mut dev,root,true,&mut BTreeSet::new());
    let mut image=FileSystem::open(dev).unwrap();
    assert_eq!(image.search_attributes(&query,500).unwrap().files.len(),25);
    assert_eq!(image.scan_files_disk(image.generation(),"/dve/",None,512)
        .unwrap().entries.len(),25);
}

#[test]
fn deleting_a_page_during_crash_preserves_complete_old_or_new_namespace() {
    for operation in 1..=10 {
        for torn in [None,Some(32),Some(124),Some(BLOCK_SIZE)] {
            let mut image=fs(256);
            populate(&mut image,25);
            let mut dev=image.into_device();
            dev.inject(Fault { operation, torn_bytes:torn }).unwrap();
            let mut image=FileSystem::open(dev).unwrap();
            let mut tx=image.begin();
            tx.remove_file("/dataset/doc_0008").unwrap();
            tx.remove_file("/dataset/doc_0009").unwrap();
            let result=image.commit(tx);
            match result {
                Ok(_) | Err(Error::InjectedCrash) => {}
                Err(other) => panic!("unexpected commit failure: {other}"),
            }
            let image = reopen(image);
            let exists08=image.stat("/dataset/doc_0008").is_ok();
            let exists09=image.stat("/dataset/doc_0009").is_ok();
            assert_eq!(exists08,exists09,
                "mixed namespace publication at operation {operation}, torn={torn:?}");
            assert!(image.stat("/dataset/doc_0010").is_ok());
        }
    }
}
