use bfs_ng_storage::experimental_v10::{
    BlockPointer, Item, MANIFEST_BYTES, MANIFEST_MAGIC, Manifest, PAGE_HEADER_BYTES, Page,
    ReadOnlyV10, SNAPSHOT_BYTES, SnapshotDescriptor, SnapshotState, TreeId, TreeRoots,
    decode_snapshot_leaf, read_page,
};
use bfs_ng_storage::{BLOCK_SIZE, BlockDevice, CrashDevice, Error, Fault, RootInfo, Volume};

const VID: u128 = 0x86a4_32dd_98af_e10a;
const TOTAL: u64 = 80;

fn page(tree: TreeId, address: u64, birth: u64, key: &[u8], value: &[u8]) -> Page {
    Page {
        tree,
        level: 0,
        birth,
        address,
        volume_id: VID,
        items: vec![Item {
            key: key.to_vec(),
            value: value.to_vec(),
        }],
    }
}
fn fs_root(address: u64, birth: u64) -> Page {
    page(TreeId::Fs, address, birth, b"/", b"inode-table-leaf")
}
fn roots(fs: &Page) -> TreeRoots {
    TreeRoots {
        fs: fs.pointer().unwrap(),
        checksum: None,
        free: None,
        dead_list: None,
        snapshots: None,
        attribute_rows: None,
        attribute_postings: None,
        schema: None,
    }
}
fn manifest(generation: u64, high_water: u64, roots: TreeRoots) -> Manifest {
    Manifest {
        generation,
        high_water,
        next_inode: 2,
        roots,
        sweep_snapshot: 0,
        sweep_key: 0,
        metadata_reserve: 0,
        inode_count: 1,
        used_blocks: 1,
        free_blocks: 0,
        pending_blocks: 0,
    }
}
fn info(generation: u64) -> RootInfo {
    RootInfo {
        volume_id: VID,
        generation,
        root_block: Some(2 + (generation - 1) % 3),
    }
}
fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ ((0_u32.wrapping_sub(crc & 1)) & 0x82f6_3b78);
        }
    }
    !crc
}
fn crc_field(block: &mut [u8], offset: usize) {
    block[offset..offset + 4].fill(0);
    let crc = crc32c(block);
    block[offset..offset + 4].copy_from_slice(&crc.to_le_bytes());
}
fn install_pages(device: &mut CrashDevice, pages: &[Page]) {
    for p in pages {
        device.write_block(p.address, &p.encode().unwrap()).unwrap();
    }
    device.sync().unwrap();
}
fn commit_with_pages(
    volume: Volume<CrashDevice>,
    m: Manifest,
    pages: &[Page],
) -> Volume<CrashDevice> {
    let mut dev = volume.into_device();
    install_pages(&mut dev, pages);
    let mut v = Volume::open(dev.power_loss()).unwrap();
    assert_eq!(v.info().generation + 1, m.generation);
    v.commit_root(&m.encode(TOTAL).unwrap()).unwrap();
    v
}

#[test]
fn v10_pages_are_canonical_and_parent_authenticated() {
    let leaf = page(TreeId::AttributeRows, 70, 7, b"alpha", b"one");
    let raw = leaf.encode().unwrap();
    let pointer = leaf.pointer().unwrap();
    let decoded =
        Page::decode_verified(&raw, pointer, TreeId::AttributeRows, VID, TOTAL, 7).unwrap();
    assert_eq!(decoded, leaf);
    assert!(raw[PAGE_HEADER_BYTES..].iter().any(|&b| b != 0));

    // A mutation that fixes the page's self CRC but not the parent's
    // checksum remains invalid: a self-consistent page is not authoritative.
    let mut forged = raw;
    forged[BLOCK_SIZE - 1] ^= 1;
    crc_field(&mut forged, 12);
    assert!(Page::decode_verified(&forged, pointer, TreeId::AttributeRows, VID, TOTAL, 7).is_err());
    let mut wrong_crc_pointer = pointer;
    wrong_crc_pointer.crc32c ^= 0x80;
    assert!(
        Page::decode_verified(
            &raw,
            wrong_crc_pointer,
            TreeId::AttributeRows,
            VID,
            TOTAL,
            7
        )
        .is_err()
    );

    // Even a freshly forged BP matching an uncanonical slotted directory
    // cannot legitimize overlapping offsets, nonzero padding, bad key order.
    let mut malformed = raw;
    malformed[48..50].copy_from_slice(&0_u16.to_le_bytes());
    crc_field(&mut malformed, 12);
    let mut forged_pointer = pointer;
    forged_pointer.crc32c = crc32c(&malformed);
    assert!(
        Page::decode_verified(
            &malformed,
            forged_pointer,
            TreeId::AttributeRows,
            VID,
            TOTAL,
            7
        )
        .is_err()
    );

    for bad_addr in [0, 1, 2, 3, 4, TOTAL] {
        let mut ptr = pointer;
        ptr.address = bad_addr;
        assert!(Page::decode_verified(&raw, ptr, TreeId::AttributeRows, VID, TOTAL, 7).is_err());
    }
    assert!(
        Page::decode_verified(&raw, pointer, TreeId::AttributePostings, VID, TOTAL, 7).is_err()
    );
    assert!(
        Page::decode_verified(&raw, pointer, TreeId::AttributeRows, VID + 1, TOTAL, 7).is_err()
    );
    assert!(Page::decode_verified(&raw, pointer, TreeId::AttributeRows, VID, TOTAL, 6).is_err());
}

#[test]
fn branch_pointers_validate_child_level_and_birth_generation() {
    let child = page(TreeId::Fs, 72, 11, b"/a", b"data");
    let branch = Page {
        tree: TreeId::Fs,
        level: 1,
        birth: 12,
        address: 71,
        volume_id: VID,
        items: vec![Item {
            key: b"/a".to_vec(),
            value: child.pointer().unwrap().encode().to_vec(),
        }],
    };
    let encoded = branch.encode().unwrap();
    assert_eq!(
        Page::decode_verified(
            &encoded,
            branch.pointer().unwrap(),
            TreeId::Fs,
            VID,
            TOTAL,
            12
        )
        .unwrap(),
        branch
    );
    let mut wrong_level = branch.clone();
    wrong_level.items[0].value[20] = 1;
    assert!(wrong_level.encode().is_err());
    let mut future_child = branch.clone();
    future_child.items[0].value[8..16].copy_from_slice(&13_u64.to_le_bytes());
    assert!(future_child.encode().is_err());
    let mut reserved = branch;
    reserved.items[0].value[21] = 1;
    assert!(reserved.encode().is_err());

    let empty = Page {
        items: Vec::new(),
        level: 0,
        ..child
    };
    let raw = empty.encode().unwrap();
    assert_eq!(
        Page::decode_verified(&raw, empty.pointer().unwrap(), TreeId::Fs, VID, TOTAL, 11)
            .unwrap()
            .items
            .len(),
        0
    );
}

#[test]
fn manifest_and_snapshot_wire_formats_support_u64_generations() {
    let big = u64::from(u32::MAX) + 17;
    let f = fs_root(70, big - 1);
    let a = page(TreeId::AttributeRows, 69, big - 1, b"x", b"old");
    let p = page(TreeId::AttributePostings, 68, big - 1, b"x", b"");
    let s = page(TreeId::Schema, 67, big - 1, b"schema", b"v1");
    let desc = SnapshotDescriptor {
        generation: big - 1,
        fs_root: f.pointer().unwrap(),
        attribute_rows_root: Some(a.pointer().unwrap()),
        attribute_postings_root: Some(p.pointer().unwrap()),
        attribute_schema_root: Some(s.pointer().unwrap()),
        state: SnapshotState::Pinned,
    };
    let raw = desc.encode(big, TOTAL).unwrap();
    assert_eq!(raw.len(), SNAPSHOT_BYTES);
    assert_eq!(SnapshotDescriptor::decode(&raw, big, TOTAL).unwrap(), desc);
    let mut malformed = raw;
    malformed[108] = 1;
    assert!(SnapshotDescriptor::decode(&malformed, big, TOTAL).is_err());
    malformed = raw;
    malformed[104..108].copy_from_slice(&3_u32.to_le_bytes());
    assert!(SnapshotDescriptor::decode(&malformed, big, TOTAL).is_err());

    let mut absent_posting = desc;
    absent_posting.attribute_postings_root = None;
    assert!(absent_posting.encode(big, TOTAL).is_err());
    let mut future = desc;
    future.attribute_rows_root = Some(BlockPointer {
        birth: big,
        ..a.pointer().unwrap()
    });
    assert!(future.encode(big, TOTAL).is_err());

    let cat = Page {
        tree: TreeId::Snapshots,
        level: 0,
        birth: big,
        address: 66,
        volume_id: VID,
        items: vec![Item {
            key: (big - 1).to_be_bytes().to_vec(),
            value: raw.to_vec(),
        }],
    };
    let mut root_set = roots(&f);
    root_set.snapshots = Some(cat.pointer().unwrap());
    root_set.attribute_rows = Some(a.pointer().unwrap());
    root_set.attribute_postings = Some(p.pointer().unwrap());
    root_set.schema = Some(s.pointer().unwrap());
    let m = manifest(big, 66, root_set);
    let bytes = m.encode(TOTAL).unwrap();
    assert_eq!(bytes.len(), MANIFEST_BYTES);
    assert_eq!(bytes[..8], MANIFEST_MAGIC);
    assert_eq!(Manifest::decode(&bytes, info(big), TOTAL).unwrap(), m);
    assert!(Manifest::decode(&bytes, info(big - 1), TOTAL).is_err());

    let mut wrong_magic = bytes;
    wrong_magic[..8].copy_from_slice(b"BFSNGM9\0");
    crc_field(&mut wrong_magic, MANIFEST_BYTES - 4);
    assert!(Manifest::decode(&wrong_magic, info(big), TOTAL).is_err());

    let mut wrong_slot = bytes;
    // Attribute schema root: bytes [208..232] when encoded as root #7.
    wrong_slot[208..232].fill(0);
    crc_field(&mut wrong_slot, MANIFEST_BYTES - 4);
    assert!(Manifest::decode(&wrong_slot, info(big), TOTAL).is_err());

    assert_eq!(decode_snapshot_leaf(&cat, big, TOTAL).unwrap(), vec![desc]);
    let mut mismatched_key = cat;
    mismatched_key.items[0].key = big.to_be_bytes().to_vec();
    assert!(decode_snapshot_leaf(&mismatched_key, big, TOTAL).is_err());
}

#[test]
fn v10_read_only_view_retains_historical_attribute_roots_after_f0_rotation() {
    let mut v = Volume::format(CrashDevice::new(TOTAL as usize), VID).unwrap();

    // Generation 1: all four snapshot-scoped tree roots have distinct pages.
    let old_fs = fs_root(75, 1);
    let old_rows = page(TreeId::AttributeRows, 74, 1, b"kind", b"old");
    let old_postings = page(TreeId::AttributePostings, 73, 1, b"kind:old", b"");
    let old_schema = page(TreeId::Schema, 72, 1, b"kind", b"string");
    let mut r1 = roots(&old_fs);
    r1.attribute_rows = Some(old_rows.pointer().unwrap());
    r1.attribute_postings = Some(old_postings.pointer().unwrap());
    r1.schema = Some(old_schema.pointer().unwrap());
    v = commit_with_pages(
        v,
        manifest(1, 72, r1),
        &[
            old_fs.clone(),
            old_rows.clone(),
            old_postings.clone(),
            old_schema.clone(),
        ],
    );
    let pinned = SnapshotDescriptor {
        generation: 1,
        fs_root: old_fs.pointer().unwrap(),
        attribute_rows_root: Some(old_rows.pointer().unwrap()),
        attribute_postings_root: Some(old_postings.pointer().unwrap()),
        attribute_schema_root: Some(old_schema.pointer().unwrap()),
        state: SnapshotState::Pinned,
    };

    // Generation 2 changes all live attribute roots, but pins exactly g1.
    let new_fs = fs_root(71, 2);
    let new_rows = page(TreeId::AttributeRows, 70, 2, b"kind", b"new");
    let new_postings = page(TreeId::AttributePostings, 69, 2, b"kind:new", b"");
    let new_schema = page(TreeId::Schema, 68, 2, b"kind", b"string-v2");
    let snapshots = Page {
        tree: TreeId::Snapshots,
        level: 0,
        birth: 2,
        address: 67,
        volume_id: VID,
        items: vec![Item {
            key: 1_u64.to_be_bytes().to_vec(),
            value: pinned.encode(2, TOTAL).unwrap().to_vec(),
        }],
    };
    let mut latest_roots = roots(&new_fs);
    latest_roots.attribute_rows = Some(new_rows.pointer().unwrap());
    latest_roots.attribute_postings = Some(new_postings.pointer().unwrap());
    latest_roots.schema = Some(new_schema.pointer().unwrap());
    latest_roots.snapshots = Some(snapshots.pointer().unwrap());
    v = commit_with_pages(
        v,
        manifest(2, 67, latest_roots),
        &[new_fs, new_rows, new_postings, new_schema, snapshots],
    );

    // Rotate both F0 root slots away from g1 and g2. All four g1 pages
    // still have to verify through the immutable snapshot descriptor.
    for generation in 3..=7_u64 {
        let head = fs_root(67 - generation, generation);
        let mut current = latest_roots;
        current.fs = head.pointer().unwrap();
        v = commit_with_pages(v, manifest(generation, head.address, current), &[head]);
    }

    let mut view = ReadOnlyV10::open(v.into_device().power_loss()).unwrap();
    assert_eq!(view.root_info().generation, 7);
    assert_eq!(view.snapshot_descriptors().unwrap(), vec![pinned]);
    let live = view.read_root_page(TreeId::AttributeRows).unwrap();
    assert_eq!(live.items[0].value, b"new");
    let mut disk = view.into_device();
    let old = read_page(
        &mut disk,
        pinned.attribute_rows_root.unwrap(),
        TreeId::AttributeRows,
        VID,
        7,
    )
    .unwrap();
    assert_eq!(old.items[0].value, b"old");

    // Corrupting a pinned historical page should not make the global v10
    // root appear invalid at open; the later snapshot-specific read fails.
    disk.corrupt_stable_for_testing(74, BLOCK_SIZE - 1, 0xaa);
    let mut later = ReadOnlyV10::open(disk.power_loss()).unwrap();
    assert_eq!(later.root_info().generation, 7);
    assert!(later.snapshot_descriptors().is_err());
}

#[test]
fn corrupt_latest_root_page_falls_back_to_previous_whole_manifest() {
    let v = Volume::format(CrashDevice::new(TOTAL as usize), VID).unwrap();
    let first = fs_root(75, 1);
    let v = commit_with_pages(v, manifest(1, 75, roots(&first)), &[first]);
    let second = fs_root(74, 2);
    let v = commit_with_pages(v, manifest(2, 74, roots(&second)), &[second]);
    let mut disk = v.into_device().power_loss();
    disk.corrupt_stable_for_testing(74, 80, 0x57);
    let recovered = ReadOnlyV10::open(disk).unwrap();
    assert_eq!(recovered.root_info().generation, 1);
    assert_eq!(recovered.manifest().generation, 1);
}

#[test]
fn torn_f0_root_publication_of_v10_manifest_never_returns_mixed_generation() {
    for operation in 1..=4 {
        for torn_bytes in [0, 8, 32, 124, BLOCK_SIZE] {
            let v = Volume::format(CrashDevice::new(TOTAL as usize), VID).unwrap();
            let p1 = fs_root(75, 1);
            let v = commit_with_pages(v, manifest(1, 75, roots(&p1)), &[p1]);
            let p2 = fs_root(74, 2);
            let v = commit_with_pages(v, manifest(2, 74, roots(&p2)), &[p2]);
            let p3 = fs_root(73, 3);
            let mut disk = v.into_device().power_loss();
            install_pages(&mut disk, &[p3.clone()]);
            disk.inject(Fault {
                operation,
                torn_bytes: Some(torn_bytes),
            })
            .unwrap();
            let mut attempted = Volume::open(disk).unwrap();
            assert!(matches!(
                attempted.commit_root(&manifest(3, 73, roots(&p3)).encode(TOTAL).unwrap()),
                Err(Error::InjectedCrash)
            ));
            let result = ReadOnlyV10::open(attempted.into_device().power_loss()).unwrap();
            assert!(matches!(result.manifest().generation, 2 | 3));
            assert_eq!(result.root_info().generation, result.manifest().generation);
        }
    }
}

fn make_superblock(generation: u64, payload_crc: u32) -> [u8; BLOCK_SIZE] {
    let mut b = [0_u8; BLOCK_SIZE];
    b[..8].copy_from_slice(b"BFSNGF0\0");
    b[8..12].copy_from_slice(&2_u32.to_le_bytes());
    b[12..16].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
    b[16..32].copy_from_slice(&VID.to_le_bytes());
    b[32..40].copy_from_slice(&generation.to_le_bytes());
    b[40..48].copy_from_slice(&(2 + (generation - 1) % 3).to_le_bytes());
    b[48..52].copy_from_slice(&payload_crc.to_le_bytes());
    b[56..64].copy_from_slice(&TOTAL.to_le_bytes());
    let crc = crc32c(&b[..120]);
    b[120..124].copy_from_slice(&crc.to_le_bytes());
    b
}
fn make_payload(bytes: &[u8]) -> [u8; BLOCK_SIZE] {
    let mut b = [0_u8; BLOCK_SIZE];
    b[..8].copy_from_slice(b"BFSNGR0\0");
    b[8..12].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
    b[16..16 + bytes.len()].copy_from_slice(bytes);
    b
}

#[test]
fn v10_and_f0_continue_past_the_u32_generation_limit_with_test_only_seed() {
    // There is deliberately no public "set generation" API. This fixture
    // constructs two checksum-valid, consecutive F0 generations directly to
    // exercise a u64 transition without executing 2^32 commits.
    let last_u32 = u64::from(u32::MAX);
    let v = Volume::format(CrashDevice::new(TOTAL as usize), VID).unwrap();
    let mut disk = v.into_device();
    let p_prev = fs_root(75, last_u32 - 1);
    let p_last = fs_root(74, last_u32);
    install_pages(&mut disk, &[p_prev.clone(), p_last.clone()]);
    for (generation, p) in [(last_u32 - 1, p_prev), (last_u32, p_last)] {
        let m = manifest(generation, p.address, roots(&p));
        let wrapped = make_payload(&m.encode(TOTAL).unwrap());
        disk.write_block(2 + (generation - 1) % 3, &wrapped)
            .unwrap();
        disk.write_block(
            generation % 2,
            &make_superblock(generation, crc32c(&wrapped)),
        )
        .unwrap();
    }
    disk.sync().unwrap();

    let v = Volume::open(disk.power_loss()).unwrap();
    assert_eq!(v.info().generation, last_u32);
    let next = fs_root(73, last_u32 + 1);
    let v = commit_with_pages(v, manifest(last_u32 + 1, 73, roots(&next)), &[next]);
    let view = ReadOnlyV10::open(v.into_device().power_loss()).unwrap();
    assert_eq!(view.manifest().generation, last_u32 + 1);
    assert_eq!(view.root_info().generation, last_u32 + 1);
}

#[test]
fn v9_file_system_remains_independent_from_unfinished_v10_writer() {
    use bfs_ng_storage::FileSystem;
    let mut v9 = FileSystem::format(CrashDevice::new(128), VID).unwrap();
    let mut tx = v9.begin();
    tx.write_file("/still-v9", b"unchanged").unwrap();
    assert_eq!(v9.commit(tx).unwrap(), 1);
    assert_eq!(v9.read_file("/still-v9").unwrap(), b"unchanged");
    let disk = v9.into_device().power_loss();
    let mut opened = FileSystem::open(disk).unwrap();
    assert_eq!(opened.read_file("/still-v9").unwrap(), b"unchanged");
    // This v9 image has a valid F0 superblock but no v10 manifest.
    assert!(matches!(
        ReadOnlyV10::open(opened.into_device().power_loss()),
        Err(Error::CorruptRoot)
    ));
}
