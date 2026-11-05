//! F5.3 end-to-end physical ownership cross-check, independent of the
//! writer's private `audit`/`collect` routines. This test intentionally
//! decodes both F0 slots and reconstructs namespace/data/snapshot/attribute
//! reachability through public read-only page codecs.
//! It is an oracle for the current indexed Free set, NOT an Accounted-mode
//! implementation or a proof of all conceivable graph histories.
use bfs_ng_storage::experimental_v10::{
    BlockPointer, FileExtents, Manifest, SnapshotState, TreeId, UnifiedV10,
    MANIFEST_BYTES, read_checksum_tree, read_namespace_tree, read_page,
    read_snapshot_tree, read_space_tree,
};
use bfs_ng_storage::{
    AttributeDef, AttributeType, BLOCK_SIZE, BlockDevice, CrashDevice, RootInfo,
    Schema, TextCase, Value,
};
use std::collections::BTreeSet;

const VID: u128 = 0xa110_c0de_f533_2026;

fn u32_le(raw: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(raw[at..at + 4].try_into().unwrap())
}

fn u64_le(raw: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(raw[at..at + 8].try_into().unwrap())
}

// Independent bitwise CRC32C check for the F0 superblock and root payload.
// The v10 tree/page decoder is shared with the production implementation.
fn crc32c_reference(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x82f6_3b78
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

// Independently parse F0 superblock + payload framing; the v10 manifest
// decoder is reused for canonical field interpretation.
fn f0_candidates(disk: &mut CrashDevice) -> Vec<Manifest> {
    let mut out = Vec::new();
    for slot in 0..2_u64 {
        let mut superblock = [0_u8; BLOCK_SIZE];
        disk.read_block(slot, &mut superblock).unwrap();
        assert_eq!(&superblock[..8], b"BFSNGF0\0");
        assert_eq!(u32_le(&superblock, 8), 2);
        assert_eq!(u32_le(&superblock, 12), BLOCK_SIZE as u32);
        let id = u128::from_le_bytes(superblock[16..32].try_into().unwrap());
        assert_eq!(id, VID);
        let generation = u64_le(&superblock, 32);
        assert_ne!(generation, 0);
        assert_eq!(generation % 2, slot);
        let payload_address = u64_le(&superblock, 40);
        assert_eq!(payload_address, 2 + (generation - 1) % 3);
        assert_eq!(u64_le(&superblock, 56), disk.block_count());
        assert_eq!(
            u32_le(&superblock, 120),
            crc32c_reference(&superblock[..120]),
            "corrupt F0 superblock CRC in slot {slot}"
        );

        let mut payload = [0_u8; BLOCK_SIZE];
        disk.read_block(payload_address, &mut payload).unwrap();
        assert_eq!(&payload[..8], b"BFSNGR0\0");
        assert_eq!(
            u32_le(&superblock, 48),
            crc32c_reference(&payload),
            "corrupt F0 payload CRC in slot {slot}"
        );
        let length = u32_le(&payload, 8) as usize;
        assert_eq!(length, MANIFEST_BYTES);
        let root = RootInfo {
            volume_id: id,
            generation,
            root_block: Some(payload_address),
        };
        out.push(Manifest::decode(&payload[16..16 + length], root, disk.block_count()).unwrap());
    }
    out.sort_by_key(|manifest| manifest.generation);
    assert_eq!(out.len(), 2);
    assert_eq!(out[1].generation, out[0].generation + 1);
    out
}

/// Recursively follow each historical attribute family. These pages are
/// snapshot-scoped; the snapshot catalog and checksums are volume-global.
fn attribute_pages(
    disk: &mut CrashDevice,
    ptr: BlockPointer,
    tree: TreeId,
    generation: u64,
    owners: &mut BTreeSet<u64>,
) {
    if !owners.insert(ptr.address) {
        return;
    }
    let page = read_page(disk, ptr, tree, VID, generation).unwrap();
    if page.level > 0 {
        for item in page.items {
            let child = BlockPointer::decode_required(&item.value).unwrap();
            attribute_pages(disk, child, tree, generation, owners);
        }
    }
}

fn visit_attributes(
    disk: &mut CrashDevice,
    rows: Option<BlockPointer>,
    postings: Option<BlockPointer>,
    schema: Option<BlockPointer>,
    generation: u64,
    owners: &mut BTreeSet<u64>,
) {
    assert_eq!(rows.is_some(), postings.is_some());
    assert_eq!(rows.is_some(), schema.is_some());
    for (tree, root) in [
        (TreeId::AttributeRows, rows),
        (TreeId::AttributePostings, postings),
        (TreeId::Schema, schema),
    ] {
        if let Some(root) = root {
            attribute_pages(disk, root, tree, generation, owners);
        }
    }
}

fn visit_namespace(
    disk: &mut CrashDevice,
    manifest: Manifest,
    root: BlockPointer,
    owners: &mut BTreeSet<u64>,
    data: &mut BTreeSet<u64>,
) {
    let namespace = read_namespace_tree(
        disk,
        root,
        VID,
        manifest.generation,
        manifest.next_inode,
    )
    .unwrap();
    owners.extend(namespace.metadata_pages);
    for entry in namespace.records.values() {
        if let Some(file_root) = entry.file_root {
            owners.insert(file_root.address);
            let page = read_page(disk, file_root, TreeId::Fs, VID, manifest.generation).unwrap();
            let extents = FileExtents::decode(&page, disk.block_count()).unwrap();
            for reference in extents.blocks.values() {
                owners.insert(reference.physical);
                data.insert(reference.physical);
            }
        }
    }
}

// Intentionally reconstruct every retained snapshot and each F0 candidate
// independently instead of calling the writer's private owner scan.
fn graph_for_manifest(disk: &mut CrashDevice, m: Manifest) -> BTreeSet<u64> {
    let mut owners = BTreeSet::new();
    let mut data = BTreeSet::new();

    visit_namespace(disk, m, m.roots.fs, &mut owners, &mut data);
    visit_attributes(
        disk,
        m.roots.attribute_rows,
        m.roots.attribute_postings,
        m.roots.schema,
        m.generation,
        &mut owners,
    );
    let snapshots = read_snapshot_tree(disk, m.roots.snapshots, VID, m.generation).unwrap();
    owners.extend(snapshots.owned_pages);
    for snapshot in snapshots.descriptors {
        visit_namespace(disk, m, snapshot.fs_root, &mut owners, &mut data);
        visit_attributes(
            disk,
            snapshot.attribute_rows_root,
            snapshot.attribute_postings_root,
            snapshot.attribute_schema_root,
            m.generation,
            &mut owners,
        );
    }

    let checksums = read_checksum_tree(disk, m.roots.checksum, VID, m.generation).unwrap();
    assert_eq!(data, checksums.checksums.keys().copied().collect());
    owners.extend(checksums.metadata_pages);

    let free = read_space_tree(disk, TreeId::Free, m.roots.free, VID, m.generation).unwrap();
    let dead = read_space_tree(disk, TreeId::DeadList, m.roots.dead_list, VID, m.generation).unwrap();
    owners.extend(free.pages);
    owners.extend(dead.pages);
    owners
}

fn check_two_f0_roots(disk: &mut CrashDevice) {
    let manifests = f0_candidates(disk);
    let mut protected = BTreeSet::new();
    for &manifest in &manifests {
        protected.extend(graph_for_manifest(disk, manifest));
    }

    let latest = manifests[1];
    assert_eq!(latest.metadata_reserve, 64);
    let free = read_space_tree(disk, TreeId::Free, latest.roots.free, VID, latest.generation).unwrap();
    let dead = read_space_tree(disk, TreeId::DeadList, latest.roots.dead_list, VID, latest.generation).unwrap();
    assert_eq!(
        latest.free_blocks,
        free.free.values().map(|run| u64::from(run.blocks)).sum::<u64>()
    );
    assert_eq!(
        latest.pending_blocks,
        dead.dead.values().map(|run| u64::from(run.blocks)).sum::<u64>()
    );
    for run in free.free.values() {
        let end = run.start + u64::from(run.blocks);
        assert!(run.start >= 69, "fixed index arena must not become Free");
        assert!(
            protected.range(run.start..end).next().is_none(),
            "Free interval {}..{} aliases a physical page reachable from an F0 root",
            run.start,
            end
        );
        assert!(dead.dead.values().all(|pending| {
            let last = pending.start + u64::from(pending.blocks);
            !(run.start < last && pending.start < end)
        }));
    }
}

fn write_model(model: &mut Vec<u8>, offset: usize, bytes: &[u8]) {
    if model.len() < offset + bytes.len() {
        model.resize(offset + bytes.len(), 0);
    }
    model[offset..offset + bytes.len()].copy_from_slice(bytes);
}

#[test]
fn indexed_ownership_matches_two_root_oracle_through_snapshot_and_namespace_churn() {
    let mut fs = UnifiedV10::format_indexed(CrashDevice::new(768), VID).unwrap();
    fs.mkdir("/assets").unwrap();
    fs.write_at("/assets/payload", 0, b"initial").unwrap();
    let schema = Schema::new(vec![AttributeDef {
        id: 1,
        name: "kind".into(),
        ty: AttributeType::Text {
            case: TextCase::AsciiFold,
            max_bytes: 48,
        },
        indexed: true,
    }])
    .unwrap();
    fs.install_schema(&schema).unwrap();
    fs.set_attributes(
        "/assets/payload",
        &[("kind".into(), Value::Text("material".into()))],
    )
    .unwrap();
    let mut path = "/assets/payload";
    let mut model = b"initial".to_vec();
    let inode = fs.stat(path).unwrap().0;
    let mut history: Vec<(u64, String, Vec<u8>)> = Vec::new();

    for i in 0..96_u64 {
        match i % 6 {
            0 | 4 => {
                let data = (i ^ 0xcafe_babe).to_le_bytes();
                fs.write_at(path, 0, &data).unwrap();
                write_model(&mut model, 0, &data);
            }
            1 => {
                let data = [i as u8, (i / 2) as u8];
                fs.write_at(path, 4100, &data).unwrap();
                write_model(&mut model, 4100, &data);
            }
            2 => {
                fs.truncate(path, 13).unwrap();
                model.resize(13, 0);
            }
            3 => {
                let new_path = if path == "/assets/payload" {
                    "/assets/renamed"
                } else {
                    "/assets/payload"
                };
                fs.rename(path, new_path).unwrap();
                path = new_path;
            }
            5 => {
                fs.reclaim_step().unwrap();
            }
            _ => unreachable!(),
        }
        if i == 44 {
            // A changed live posting/root must not erase a pinned historical
            // triple, even when the original directory entry was renamed.
            fs.set_attributes(
                path,
                &[("kind".into(), Value::Text("texture".into()))],
            )
            .unwrap();
        }
        assert_eq!(fs.read_file(path).unwrap(), model, "step {i}");
        assert_eq!(fs.stat(path).unwrap().0, inode, "stable inode at step {i}");

        if [12, 30, 48].contains(&i) {
            let snapshot_generation = fs.manifest().generation;
            fs.pin_current().unwrap();
            history.push((snapshot_generation, path.to_owned(), model.clone()));
        }
        if i == 65 {
            fs.mark_dropping(history[1].0).unwrap();
            assert_eq!(
                fs.snapshots().unwrap()[1].state,
                SnapshotState::Dropping
            );
        }
        if i % 8 == 0 || i == 95 {
            let mut device = fs.into_device().power_loss();
            check_two_f0_roots(&mut device);
            fs = UnifiedV10::open(device).unwrap();
            assert_eq!(fs.read_file(path).unwrap(), model);
            for (generation, historical_path, bytes) in &history {
                assert_eq!(
                    fs.read_snapshot_file(*generation, historical_path).unwrap(),
                    *bytes,
                    "snapshot {generation} at step {i}"
                );
            }
        }
    }
    assert_eq!(history.len(), 3);
}
