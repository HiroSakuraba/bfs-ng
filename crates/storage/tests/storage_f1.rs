use bfs_ng_storage::{
    BLOCK_SIZE, BlockDevice, CrashDevice, Error, Fault, FileDevice, FileSystem, Volume,
};
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn fresh(n: usize) -> FileSystem<CrashDevice> {
    FileSystem::format(CrashDevice::new(n), 0xabc).unwrap()
}

fn reopen(fs: FileSystem<CrashDevice>) -> FileSystem<CrashDevice> {
    FileSystem::open(fs.into_device().power_loss()).unwrap()
}

#[test]
fn nested_directories_and_multiblock_data_survive_reopen() {
    let mut fs = fresh(128);
    let mut tx = fs.begin();
    tx.mkdir("/projects").unwrap();
    tx.mkdir("/projects/dve").unwrap();
    let inode = tx.write_file("/projects/dve/shader", b"first").unwrap();
    tx.write_file("/empty", b"").unwrap();
    let large: Vec<u8> = (0..11_000).map(|i| (i % 251) as u8).collect();
    tx.write_file("/projects/dve/blob", &large).unwrap();
    assert_eq!(fs.file_count(), 0);
    fs.commit(tx).unwrap();
    assert_eq!(fs.list_dir("/").unwrap(), vec!["/empty", "/projects"]);
    assert_eq!(fs.read_file("/projects/dve/blob").unwrap(), large);
    let mut fs = reopen(fs);
    assert_eq!(fs.generation(), 1);
    assert_eq!(fs.file_count(), 5);
    assert_eq!(fs.read_file("/empty").unwrap(), b"");
    assert_eq!(fs.read_file("/projects/dve/blob").unwrap(), large);
    assert_eq!(fs.stat("/projects/dve/shader").unwrap().inode, inode);
}

#[test]
fn multilevel_sorted_tree_and_update_delete_roundtrip() {
    let mut fs = fresh(1024);
    let mut tx = fs.begin();
    tx.mkdir("/docs").unwrap();
    for i in 0..150 {
        tx.write_file(
            &format!("/docs/record_{i:04}"),
            format!("v1-{i}").as_bytes(),
        )
        .unwrap();
    }
    fs.commit(tx).unwrap();
    fs = reopen(fs);
    assert_eq!(fs.file_count(), 151);
    let mut tx = fs.begin();
    tx.write_file("/docs/record_0000", b"revised").unwrap();
    tx.remove_file("/docs/record_0100").unwrap();
    tx.write_file("/docs/record_0150", b"new").unwrap();
    fs.commit(tx).unwrap();
    let mut fs = reopen(fs);
    assert_eq!(fs.list_dir("/docs").unwrap().len(), 150);
    assert_eq!(fs.read_file("/docs/record_0000").unwrap(), b"revised");
    assert_eq!(fs.read_file("/docs/record_0099").unwrap(), b"v1-99");
    assert!(matches!(
        fs.read_file("/docs/record_0100"),
        Err(Error::NotFound)
    ));
}

#[test]
fn invalid_paths_and_namespace_invariants_are_rejected() {
    let mut fs = fresh(80);
    let mut tx = fs.begin();
    for path in [
        "/",
        "",
        "relative",
        "/a//b",
        "/a/",
        "/../b",
        "/a/./b",
        "/back\\slash",
        "/bad\0name",
    ] {
        assert!(
            matches!(tx.mkdir(path), Err(Error::InvalidPath)),
            "{path:?}"
        );
        assert!(
            matches!(tx.write_file(path, b"x"), Err(Error::InvalidPath)),
            "{path:?}"
        );
    }
    assert!(matches!(tx.mkdir("/missing/child"), Err(Error::NotFound)));
    tx.mkdir("/a").unwrap();
    assert!(matches!(tx.mkdir("/a"), Err(Error::AlreadyExists)));
    assert!(matches!(tx.write_file("/a", b"x"), Err(Error::IsDirectory)));
    tx.write_file("/a/file", b"x").unwrap();
    assert!(matches!(tx.mkdir("/a/file/nope"), Err(Error::NotDirectory)));
    assert!(matches!(tx.rmdir("/a"), Err(Error::NotEmpty)));
    tx.remove_file("/a/file").unwrap();
    tx.rmdir("/a").unwrap();
    fs.commit(tx).unwrap();
    assert_eq!(reopen(fs).file_count(), 0);
}

#[test]
fn stale_and_empty_transactions_do_not_publish() {
    let mut fs = fresh(64);
    assert_eq!(fs.commit(fs.begin()).unwrap(), 0);
    let mut a = fs.begin();
    let mut b = fs.begin();
    a.write_file("/a", b"one").unwrap();
    b.write_file("/b", b"two").unwrap();
    fs.commit(a).unwrap();
    assert!(matches!(fs.commit(b), Err(Error::StaleTransaction)));
    assert_eq!(reopen(fs).file_count(), 1);
}

#[test]
fn append_only_exhaustion_never_overwrites_old_root() {
    let mut fs = fresh(10);
    let mut tx = fs.begin();
    tx.write_file("/a", b"old").unwrap();
    fs.commit(tx).unwrap();
    let mut failed = false;
    for i in 0..10 {
        let mut tx = fs.begin();
        tx.write_file("/a", format!("generation-{i}").as_bytes())
            .unwrap();
        match fs.commit(tx) {
            Ok(_) => {}
            Err(Error::NoSpace) => {
                failed = true;
                break;
            }
            Err(e) => panic!("{e}"),
        }
    }
    assert!(failed);
    let generation = fs.generation();
    let mut reopened = reopen(fs);
    assert_eq!(reopened.generation(), generation);
    assert!(!reopened.read_file("/a").unwrap().is_empty());
}

#[test]
fn each_commit_crash_point_keeps_old_or_entire_new_namespace() {
    // One new data block + one new leaf block + stage sync + F0's four operations.
    for fail_at in 1..=7 {
        let mut fs = fresh(48);
        let mut tx = fs.begin();
        tx.write_file("/file", b"old-valid").unwrap();
        fs.commit(tx).unwrap();
        let mut device = fs.into_device();
        device
            .inject(Fault {
                operation: fail_at,
                torn_bytes: None,
            })
            .unwrap();
        let mut fs = FileSystem::open(device).unwrap();
        let mut tx = fs.begin();
        tx.write_file("/file", b"new-valid").unwrap();
        assert!(matches!(fs.commit(tx), Err(Error::InjectedCrash)));
        let mut fs = reopen(fs);
        let data = fs.read_file("/file").unwrap();
        assert!(data == b"old-valid" || data == b"new-valid");
        if fail_at < 7 {
            assert_eq!(data, b"old-valid");
        } else {
            assert_eq!(data, b"new-valid");
        }
    }
}

#[test]
fn torn_writes_never_accept_partial_data() {
    for operation in [1, 2, 4, 6] {
        for torn_bytes in [0, 1, 24, 32, 123, 124, 512, BLOCK_SIZE] {
            let mut fs = fresh(48);
            let mut tx = fs.begin();
            tx.write_file("/a", b"old").unwrap();
            fs.commit(tx).unwrap();
            let mut d = fs.into_device();
            d.inject(Fault {
                operation,
                torn_bytes: Some(torn_bytes),
            })
            .unwrap();
            let mut fs = FileSystem::open(d).unwrap();
            let mut tx = fs.begin();
            tx.write_file("/a", b"new").unwrap();
            assert!(matches!(fs.commit(tx), Err(Error::InjectedCrash)));
            let mut fs = reopen(fs);
            let data = fs.read_file("/a").unwrap();
            assert!(data == b"old" || data == b"new");
        }
    }
}

#[test]
fn corrupted_newest_metadata_tree_falls_back() {
    let mut fs = fresh(96);
    for content in [b"first".as_slice(), b"second".as_slice()] {
        let mut tx = fs.begin();
        tx.write_file("/file", content).unwrap();
        fs.commit(tx).unwrap();
    }
    let mut d = fs.into_device().power_loss();
    let mut volume = Volume::open(d).unwrap();
    let manifest = volume.read_root().unwrap().unwrap();
    let root = u64::from_le_bytes(manifest[24..32].try_into().unwrap());
    d = volume.into_device();
    d.corrupt_stable_for_testing(root as usize, 52, 0xfa);
    let mut fs = FileSystem::open(d).unwrap();
    assert_eq!(fs.generation(), 1);
    assert_eq!(fs.read_file("/file").unwrap(), b"first");
}

#[test]
fn corrupted_newest_file_extent_falls_back() {
    let mut fs = fresh(96);
    for content in [b"first".as_slice(), b"second".as_slice()] {
        let mut tx = fs.begin();
        tx.write_file("/file", content).unwrap();
        fs.commit(tx).unwrap();
    }
    let mut d = fs.into_device().power_loss();
    let mut target = None;
    for n in 3..d.block_count() {
        let mut b = [0; BLOCK_SIZE];
        d.read_block(n, &mut b).unwrap();
        if &b[..8] == b"BFSNGD1\0" && &b[32..38] == b"second" {
            target = Some(n as usize);
        }
    }
    d.corrupt_stable_for_testing(target.unwrap(), 38, 0xff);
    let mut fs = FileSystem::open(d).unwrap();
    assert_eq!(fs.generation(), 1);
    assert_eq!(fs.read_file("/file").unwrap(), b"first");
}

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!("bfs-ng-f1-{}-{time}-{n}.img", std::process::id())))
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn actual_file_image_persists_records_across_commits() {
    let path = Temp::new();
    let mut fs = FileSystem::format(FileDevice::create_new(&path.0, 96).unwrap(), 0x44).unwrap();
    let mut tx = fs.begin();
    tx.mkdir("/axiom").unwrap();
    let inode = tx.write_file("/axiom/example.az", b"initial").unwrap();
    fs.commit(tx).unwrap();
    drop(fs);
    let mut fs = FileSystem::open(FileDevice::open(&path.0).unwrap()).unwrap();
    assert_eq!(fs.read_file("/axiom/example.az").unwrap(), b"initial");
    let mut tx = fs.begin();
    assert_eq!(
        tx.write_file("/axiom/example.az", b"modified").unwrap(),
        inode
    );
    fs.commit(tx).unwrap();
    drop(fs);
    let mut fs = FileSystem::open(FileDevice::open(&path.0).unwrap()).unwrap();
    assert_eq!(fs.read_file("/axiom/example.az").unwrap(), b"modified");
    assert_eq!(fs.stat("/axiom/example.az").unwrap().inode, inode);
    assert_eq!(fs.generation(), 2);
}

#[test]
fn file_image_tree_corruption_recovers_preceding_generation() {
    let path = Temp::new();
    let mut fs = FileSystem::format(FileDevice::create_new(&path.0, 96).unwrap(), 0x55).unwrap();
    for content in [b"before".as_slice(), b"after".as_slice()] {
        let mut tx = fs.begin();
        tx.write_file("/x", content).unwrap();
        fs.commit(tx).unwrap();
    }
    drop(fs);
    let mut f0 = Volume::open(FileDevice::open(&path.0).unwrap()).unwrap();
    let manifest = f0.read_root().unwrap().unwrap();
    let root = u64::from_le_bytes(manifest[24..32].try_into().unwrap());
    drop(f0);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path.0)
        .unwrap();
    file.seek(SeekFrom::Start(root * BLOCK_SIZE as u64 + 52))
        .unwrap();
    file.write_all(&[0x88]).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let mut fs = FileSystem::open(FileDevice::open(&path.0).unwrap()).unwrap();
    assert_eq!(fs.generation(), 1);
    assert_eq!(fs.read_file("/x").unwrap(), b"before");
}
