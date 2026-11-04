//! F2.R1: durable reference counts are never authoritative without the
//! independent live + snapshot graph reconstruction.
use bfs_ng_storage::{
    AllocationReport, AttributeDef, AttributeType, BlockDevice, CrashDevice, Error,
    Fault, FileSystem, Predicate, Schema, TextCase, Value, Volume, BLOCK_SIZE,
};

fn blank(blocks: usize) -> FileSystem<CrashDevice> {
    FileSystem::format(CrashDevice::new(blocks), 0xfeed_2505).unwrap()
}
fn reopened(fs: FileSystem<CrashDevice>) -> FileSystem<CrashDevice> {
    FileSystem::open(fs.into_device().power_loss()).unwrap()
}
fn write(fs: &mut FileSystem<CrashDevice>, value: &[u8]) {
    let mut tx = fs.begin();
    tx.write_file("/record", value).unwrap();
    fs.commit(tx).unwrap();
}
fn index_header(device: CrashDevice) -> (CrashDevice, u64, u32, u32) {
    let mut vol = Volume::open(device).unwrap();
    let manifest = vol.read_root().unwrap().unwrap();
    let snap_count = u32::from_le_bytes(manifest[56..60].try_into().unwrap()) as usize;
    let offset = 64 + snap_count * 40;
    assert_eq!(&manifest[..8], b"BFSNGM5\0");
    let first = u64::from_le_bytes(manifest[offset..offset+8].try_into().unwrap());
    let pages = u32::from_le_bytes(manifest[offset+8..offset+12].try_into().unwrap());
    let entries = u32::from_le_bytes(manifest[offset+12..offset+16].try_into().unwrap());
    (vol.into_device(), first, pages, entries)
}
fn observed(fs: &mut FileSystem<CrashDevice>) -> AllocationReport {
    let report = fs.allocation_report().unwrap();
    assert!(!report.reuse_blocked);
    assert!(report.reference_index_pages >= 1);
    assert!(report.reference_index_entries <= report.referenced_blocks);
    report
}

#[test]
fn checksummed_reference_index_persists_across_reopen_and_commits() {
    let mut fs = blank(192);
    write(&mut fs, b"g1");
    let report = observed(&mut fs);
    assert_eq!(report.reference_index_pages, 1);
    assert!(report.reference_index_entries >= 2); // data + metadata
    fs = reopened(fs);
    assert_eq!(fs.read_file("/record").unwrap(), b"g1");
    assert_eq!(observed(&mut fs).reference_index_entries, report.reference_index_entries);
    write(&mut fs, b"g2");
    write(&mut fs, b"g3");
    let report = observed(&mut fs);
    assert!(report.reclaimable_blocks > 0 || report.shared_blocks > 0);
    fs = reopened(fs);
    assert_eq!(fs.read_file("/record").unwrap(), b"g3");
    assert!(!fs.allocation_report().unwrap().reuse_blocked);
}

#[test]
fn snapshot_root_retains_indexed_blocks_and_file_data_after_replacement() {
    let mut fs = blank(256);
    write(&mut fs, b"original");
    let mut tx = fs.begin();
    tx.pin_snapshot().unwrap();
    tx.write_file("/record", b"next").unwrap();
    fs.commit(tx).unwrap();
    let snap_report = observed(&mut fs);
    assert!(snap_report.reference_index_entries >= 4);
    assert_eq!(fs.read_snapshot_file(1, "/record").unwrap(), b"original");
    for i in 0..6 {
        write(&mut fs, format!("later {i}").as_bytes());
    }
    fs = reopened(fs);
    assert_eq!(fs.read_snapshot_file(1, "/record").unwrap(), b"original");
    let before = observed(&mut fs);
    let mut tx = fs.begin();
    tx.drop_snapshot(1).unwrap();
    fs.commit(tx).unwrap();
    assert_eq!(fs.snapshots(), Vec::<u64>::new());
    let immediately = observed(&mut fs);
    // The preceding F0 fallback may still refer to the retired snapshot.
    assert!(immediately.referenced_blocks >= 2);
    for i in 0..5 {
        write(&mut fs, format!("unpin {i}").as_bytes());
    }
    assert!(observed(&mut fs).root_candidates >= 1);
    assert!(before.root_candidates >= 1);
    fs = reopened(fs);
    assert_eq!(fs.read_file("/record").unwrap(), b"unpin 4");
    assert!(matches!(fs.read_snapshot_file(1, "/record"), Err(Error::InvalidSnapshot)));
}

#[test]
fn multi_page_reference_chain_is_sorted_and_recovers_large_namespace() {
    let mut fs = blank(2048);
    let mut tx = fs.begin();
    tx.mkdir("/bulk").unwrap();
    let data: Vec<u8> = (0..17_000).map(|i| (i % 251) as u8).collect();
    for n in 0..96 {
        tx.write_file(&format!("/bulk/item_{n:04}"), &data).unwrap();
    }
    fs.commit(tx).unwrap();
    let report = observed(&mut fs);
    assert!(report.reference_index_entries > 405, "test must span reference pages");
    assert!(report.reference_index_pages >= 2);
    let (mut device, first, pages, entries) = index_header(fs.into_device().power_loss());
    assert!(first > 2 && pages >= 2 && entries > 405);
    let mut raw = [0; BLOCK_SIZE];
    device.read_block(first, &mut raw).unwrap();
    assert_eq!(&raw[..8], b"BFSREF5\0");
    fs = FileSystem::open(device).unwrap();
    assert_eq!(fs.file_count(), 97);
    assert_eq!(fs.read_file("/bulk/item_0095").unwrap(), data);
    assert_eq!(observed(&mut fs).reference_index_pages, pages as usize);
}

#[test]
fn corrupted_latest_reference_page_rolls_back_without_exposing_new_data() {
    let mut fs = blank(128);
    write(&mut fs, b"old");
    write(&mut fs, b"new");
    let (mut device, first, _, _) = index_header(fs.into_device().power_loss());
    device.corrupt_stable_for_testing(first as usize, 42, 0xb9);
    let mut recovered = FileSystem::open(device).unwrap();
    assert_eq!(recovered.generation(), 1);
    assert_eq!(recovered.read_file("/record").unwrap(), b"old");
    assert!(recovered.allocation_report().unwrap().reuse_blocked,
        "corrupt F0-valid candidate cannot allow block reuse");
}

fn crc32c(data: &[u8]) -> u32 {
    let mut state = !0_u32;
    for &byte in data {
        state ^= byte as u32;
        for _ in 0..8 {
            state = (state >> 1) ^ (0x82f6_3b78_u32 & (0_u32.wrapping_sub(state & 1)));
        }
    }
    !state
}

#[test]
fn valid_crc_but_forged_reference_count_is_rejected_by_graph_oracle() {
    let mut fs = blank(128);
    write(&mut fs, b"first");
    write(&mut fs, b"second");
    let (mut device, first, _, _) = index_header(fs.into_device().power_loss());
    let mut block = [0; BLOCK_SIZE];
    device.read_block(first, &mut block).unwrap();
    assert_eq!(&block[..8], b"BFSREF5\0");
    let refs = u16::from_le_bytes(block[40..42].try_into().unwrap());
    block[40..42].copy_from_slice(&(refs + 1).to_le_bytes());
    block[24..28].fill(0);
    let checksum = crc32c(&block);
    block[24..28].copy_from_slice(&checksum.to_le_bytes());
    // Explicitly write the *entire*, internally checksummed forged page so
    // rejecting it demonstrates independent semantics, not mere CRC failure.
    device.write_block(first, &block).unwrap();
    device.sync().unwrap();
    device = device.power_loss();
    let mut recovered = FileSystem::open(device).unwrap();
    assert_eq!(recovered.generation(), 1);
    assert_eq!(recovered.read_file("/record").unwrap(), b"first");
    assert!(recovered.allocation_report().unwrap().reuse_blocked);
}

#[test]
fn invalid_reference_link_is_rejected_without_unsafe_reclamation() {
    let mut fs = blank(2048);
    let mut tx = fs.begin();
    tx.mkdir("/data").unwrap();
    for n in 0..95 {
        tx.write_file(&format!("/data/f{n:03}"), &vec![n as u8; 19000]).unwrap();
    }
    fs.commit(tx).unwrap();
    let mut second = fs.begin();
    second.write_file("/data/f000", b"new").unwrap();
    fs.commit(second).unwrap();
    let (mut device, first, pages, _) = index_header(fs.into_device().power_loss());
    assert!(pages >= 2);
    let mut block = [0; BLOCK_SIZE];
    device.read_block(first, &mut block).unwrap();
    // Make first page loop back to itself, with valid block checksum.
    block[16..24].copy_from_slice(&first.to_le_bytes());
    block[24..28].fill(0);
    let checksum = crc32c(&block);
    block[24..28].copy_from_slice(&checksum.to_le_bytes());
    device.write_block(first, &block).unwrap();
    device.sync().unwrap();
    let mut recovered = FileSystem::open(device.power_loss()).unwrap();
    assert_eq!(recovered.generation(), 1);
    assert!(recovered.allocation_report().unwrap().reuse_blocked);
}

#[test]
fn crash_injections_publish_whole_namespace_and_reference_index_only() {
    for operation in 1..=13 {
        for torn_bytes in [None, Some(24), Some(124), Some(BLOCK_SIZE)] {
            let mut fs = blank(128);
            write(&mut fs, b"before");
            let mut device = fs.into_device();
            device.inject(Fault { operation, torn_bytes }).unwrap();
            let mut fs = FileSystem::open(device).unwrap();
            let mut tx = fs.begin();
            tx.write_file("/record", b"after").unwrap();
            let result = fs.commit(tx);
            assert!(result.is_ok() || matches!(result, Err(Error::InjectedCrash)));
            let mut fs = reopened(fs);
            let value = fs.read_file("/record").unwrap();
            assert!(value == b"before" || value == b"after",
                "mixed or damaged generation at operation={operation}, torn={torn_bytes:?}");
            let allocation = fs.allocation_report().unwrap();
            // All accepted roots have a valid independently checked index.
            assert!(allocation.reference_index_pages >= 1);
            assert!(allocation.reference_index_entries >= 2);
        }
    }
}

#[test]
fn f4_attributes_and_reference_catalog_advance_in_same_root() {
    let mut fs = blank(256);
    let mut tx = fs.begin();
    tx.install_schema(Schema::new(vec![AttributeDef {
        id: 1, name: "asset.kind".into(),
        ty: AttributeType::Text { case: TextCase::AsciiFold, max_bytes: 40 },
        indexed: true,
    }]).unwrap()).unwrap();
    tx.write_file("/material", b"alpha").unwrap();
    tx.set_attributes("/material", [
        ("asset.kind".into(), Value::Text("SHADER".into())),
    ]).unwrap();
    fs.commit(tx).unwrap();
    let mut tx = fs.begin();
    tx.pin_snapshot().unwrap();
    tx.write_file("/material", b"beta").unwrap();
    tx.set_attributes("/material", [
        ("asset.kind".into(), Value::Text("MATERIAL".into())),
    ]).unwrap();
    fs.commit(tx).unwrap();
    fs = reopened(fs);
    let current = fs.search_attributes(&[
        Predicate::eq("asset.kind", Value::Text("material".into())),
    ], 10).unwrap();
    assert_eq!(current.files.len(), 1);
    let old = fs.search_snapshot_attributes(1, &[
        Predicate::eq("asset.kind", Value::Text("shader".into())),
    ], 10).unwrap();
    assert_eq!(old.files.len(), 1);
    assert_eq!(fs.read_snapshot_file(1, "/material").unwrap(), b"alpha");
    assert!(!observed(&mut fs).reuse_blocked);
}
