//! Experimental v10 multilevel extents and bounded-I/O streaming.
//! Full two-F0 root reachability audit remains allocation authority.
use bfs_ng_storage::experimental_v10::{
    MAX_UNIFIED_FILE_BYTES, UnifiedV10, VerifiedReadView, read_file_tree, read_namespace_tree,
};
use bfs_ng_storage::{CrashDevice, Error, Fault};
use std::io::{self, Cursor, Read};

const ID: u128 = 0x20_26_11_07_54_52;

fn indexed(n: usize) -> UnifiedV10<CrashDevice> {
    UnifiedV10::format_indexed(CrashDevice::new(n), ID).unwrap()
}

#[test]
fn sparse_logical_gib_with_tail_writes_and_bounded_read_streaming() {
    let mut fs = indexed(4000);
    fs.mkdir("/data").unwrap();
    fs.write_at("/data/big", 0, b"first").unwrap();
    fs.truncate("/data/big", MAX_UNIFIED_FILE_BYTES).unwrap();
    let tail = MAX_UNIFIED_FILE_BYTES - 17;
    fs.write_at("/data/big", tail, b"0123456789ABCDEFG")
        .unwrap();
    assert_eq!(fs.stat("/data/big").unwrap().2, MAX_UNIFIED_FILE_BYTES);
    assert!(matches!(fs.read_file("/data/big"), Err(Error::TooLarge)));
    assert_eq!(fs.read_at("/data/big", 5, 12).unwrap(), vec![0_u8; 12]);
    assert_eq!(
        fs.read_at("/data/big", tail, 17).unwrap(),
        b"0123456789ABCDEFG"
    );
    let source = vec![0x5a_u8; 256 * 1024];
    let count = fs
        .write_from_reader("/data/big", 8 * 4096, &mut Cursor::new(source.clone()))
        .unwrap();
    assert_eq!(count, source.len() as u64);
    let mut out = Vec::new();
    assert_eq!(
        fs.read_into_writer("/data/big", 8 * 4096, count, &mut out)
            .unwrap(),
        count
    );
    assert_eq!(out, source);
    let mut view = VerifiedReadView::open(fs.into_device().power_loss()).unwrap();
    let mut streamed = Vec::new();
    assert_eq!(
        view.read_into_writer("/data/big", 8 * 4096, count, &mut streamed)
            .unwrap(),
        count
    );
    assert_eq!(streamed, source);
    assert_eq!(view.stat("/data/big").unwrap().2, MAX_UNIFIED_FILE_BYTES);
    assert_eq!(
        view.read_at("/data/big", tail, 17).unwrap(),
        b"0123456789ABCDEFG"
    );
}

#[test]
fn split_file_btree_shares_unchanged_leaves_across_snapshots() {
    let mut fs = indexed(8192);
    fs.mkdir("/d").unwrap();
    let blocks: Vec<Vec<u8>> = (0..160).map(|n| vec![n as u8; 32]).collect();
    let edits: Vec<(u64, &[u8])> = blocks
        .iter()
        .enumerate()
        .map(|(n, bytes)| ((n as u64) * 8192, bytes.as_slice()))
        .collect();
    let initial = fs.write_ranges("/d/f", &edits).unwrap();
    let pinned = initial;
    fs.pin_current().unwrap();
    fs.write_at("/d/f", 81 * 8192 + 7, b"PATCH").unwrap();

    let m = *fs.manifest();
    let snapshots = fs.snapshots().unwrap();
    let historic = snapshots.iter().find(|x| x.generation == pinned).unwrap();
    let mut disk = fs.into_device().power_loss();
    let old_ns =
        read_namespace_tree(&mut disk, historic.fs_root, ID, m.generation, m.next_inode).unwrap();
    let new_ns =
        read_namespace_tree(&mut disk, m.roots.fs, ID, m.generation, m.next_inode).unwrap();
    let old_root = old_ns.records["/d/f"].file_root.unwrap();
    let new_root = new_ns.records["/d/f"].file_root.unwrap();
    assert!(
        old_root.level >= 1,
        "160 sparse runs should occupy multiple leaves"
    );
    assert!(new_root.level >= 1);
    let original = read_file_tree(&mut disk, old_root, ID, m.generation).unwrap();
    let updated = read_file_tree(&mut disk, new_root, ID, m.generation).unwrap();
    assert_eq!(original.file.size, 159 * 8192 + 32);
    assert_eq!(updated.file.size, original.file.size);
    assert!(
        original
            .metadata_pages
            .intersection(&updated.metadata_pages)
            .next()
            .is_some(),
        "path-copy must retain at least one untouched leaf across snapshots"
    );
    assert_ne!(original.metadata_pages, updated.metadata_pages);
    let mut reopened = UnifiedV10::open(disk).unwrap();
    assert_eq!(
        reopened.read_at("/d/f", 81 * 8192 + 7, 5).unwrap(),
        b"PATCH"
    );
    assert_eq!(
        reopened.read_snapshot_file(pinned, "/d/f").unwrap()[81 * 8192 + 7..81 * 8192 + 12],
        blocks[81][7..12]
    );
}

#[test]
fn large_file_crc_corruption_fails_closed_on_reopen() {
    let mut fs = indexed(2048);
    fs.mkdir("/x").unwrap();
    fs.write_at("/x/big", 0, b"start").unwrap();
    fs.write_at("/x/big", 800 * 4096, b"end").unwrap();
    let manifest = *fs.manifest();
    let mut disk = fs.into_device().power_loss();
    let ns = read_namespace_tree(
        &mut disk,
        manifest.roots.fs,
        ID,
        manifest.generation,
        manifest.next_inode,
    )
    .unwrap();
    let root = ns.records["/x/big"].file_root.unwrap();
    let scan = read_file_tree(&mut disk, root, ID, manifest.generation).unwrap();
    let physical = scan.file.blocks[&800].physical as usize;
    disk.corrupt_stable_for_testing(physical, 0, 0);
    assert!(UnifiedV10::open(disk.power_loss()).is_err());
}

#[test]
fn torn_multilevel_extent_change_recovers_only_old_or_new_bytes() {
    for operation in 1..=6 {
        let mut fs = indexed(4096);
        fs.mkdir("/d").unwrap();
        fs.write_at("/d/f", 300 * 4096, b"old").unwrap();
        let previous = fs.manifest().generation;
        let mut disk = fs.into_device().power_loss();
        disk.inject(Fault {
            operation,
            torn_bytes: Some(127),
        })
        .unwrap();
        let mut tx = UnifiedV10::open(disk).unwrap();
        assert!(matches!(
            tx.write_at("/d/f", 300 * 4096, b"new"),
            Err(Error::InjectedCrash)
        ));
        let mut reopened = UnifiedV10::open(tx.into_device().power_loss()).unwrap();
        assert!(
            reopened.manifest().generation == previous
                || reopened.manifest().generation == previous + 1
        );
        let bytes = reopened.read_at("/d/f", 300 * 4096, 3).unwrap();
        assert!(bytes == b"old" || bytes == b"new");
    }
}

struct ErrorAfter {
    sent: usize,
}
impl Read for ErrorAfter {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.sent >= 2 * 32 * 4096 {
            return Err(io::Error::other("injected input failure"));
        }
        let count = out.len().min(2 * 32 * 4096 - self.sent);
        out[..count].fill(0x8f);
        self.sent += count;
        Ok(count)
    }
}

#[test]
fn streaming_input_failure_keeps_durable_committed_prefix() {
    let mut fs = indexed(1536);
    fs.mkdir("/d").unwrap();
    let result = fs.write_from_reader("/d/f", 0, &mut ErrorAfter { sent: 0 });
    assert!(matches!(result, Err(Error::Io(_))));
    let mut reopened = UnifiedV10::open(fs.into_device().power_loss()).unwrap();
    let mut out = Vec::new();
    let prefix = 2 * 32 * 4096;
    assert_eq!(
        reopened
            .read_into_writer("/d/f", 0, prefix as u64, &mut out)
            .unwrap(),
        prefix as u64
    );
    assert_eq!(out.len(), prefix);
    assert!(out.iter().all(|&x| x == 0x8f));
}
