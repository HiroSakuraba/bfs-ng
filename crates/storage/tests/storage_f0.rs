use bfs_ng_storage::{
    BLOCK_SIZE, BlockDevice, CrashDevice, Error, Fault, FileDevice, ROOT_CAPACITY, Volume,
};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn vol(blocks: usize) -> Volume<CrashDevice> {
    Volume::format(CrashDevice::new(blocks), 0x1234_5678_9abc_def0).unwrap()
}

#[test]
fn round_trip_and_alternating_root_slots() {
    let mut v = vol(12);
    assert_eq!(v.info().generation, 0);
    assert_eq!(v.read_root().unwrap(), None);
    let first = v.commit_root(b"root-one").unwrap();
    assert_eq!(first.generation, 1);
    assert_eq!(first.root_block, Some(2));
    let second = v.commit_root(b"root-two").unwrap();
    assert_eq!(second.generation, 2);
    assert_eq!(second.root_block, Some(3));
    assert_eq!(v.read_root().unwrap(), Some(b"root-two".to_vec()));
    let mut recovered = Volume::open(v.into_device().power_loss()).unwrap();
    assert_eq!(recovered.info(), second);
    assert_eq!(recovered.read_root().unwrap(), Some(b"root-two".to_vec()));
    assert_eq!(recovered.commit_root(b"third").unwrap().root_block, Some(4));
}

#[test]
fn empty_and_capacity_payloads_are_distinct_from_unformatted() {
    let mut v = vol(8);
    assert_eq!(v.read_root().unwrap(), None);
    v.commit_root(b"").unwrap();
    assert_eq!(v.read_root().unwrap(), Some(vec![]));
    let biggest = vec![0xcc; ROOT_CAPACITY];
    v.commit_root(&biggest).unwrap();
    assert_eq!(v.read_root().unwrap(), Some(biggest));
    assert!(matches!(
        v.commit_root(&vec![0; ROOT_CAPACITY + 1]),
        Err(Error::TooLarge)
    ));
    assert_eq!(v.info().generation, 2);
}

#[test]
fn out_of_space_preserves_last_root() {
    let mut v = vol(4);
    v.commit_root(b"a").unwrap();
    v.commit_root(b"b").unwrap();
    assert!(matches!(v.commit_root(b"c"), Err(Error::NoSpace)));
    assert_eq!(v.info().generation, 2);
    assert_eq!(v.read_root().unwrap(), Some(b"b".to_vec()));
    assert_eq!(
        Volume::open(v.into_device().power_loss())
            .unwrap()
            .info()
            .generation,
        2
    );
}

#[test]
fn all_four_crash_points_preserve_old_or_entire_new_root() {
    for operation in 1..=4 {
        let mut v = vol(10);
        v.commit_root(b"committed-v1").unwrap();
        let mut device = v.into_device();
        device
            .inject(Fault {
                operation,
                torn_bytes: None,
            })
            .unwrap();
        let mut v = Volume::open(device).unwrap();
        assert!(matches!(
            v.commit_root(b"committed-v2"),
            Err(Error::InjectedCrash)
        ));
        assert!(matches!(
            v.commit_root(b"retry-without-reopen"),
            Err(Error::Poisoned)
        ));
        let mut recovered = Volume::open(v.into_device().power_loss()).unwrap();
        let result = recovered.read_root().unwrap().unwrap();
        assert!(
            result == b"committed-v1" || result == b"committed-v2",
            "invalid root at operation {operation}"
        );
        // Faults at write(1), sync(2), write(3) cannot publish new root.
        if operation < 4 {
            assert_eq!(result, b"committed-v1");
        } else {
            assert_eq!(result, b"committed-v2");
        }
        assert!(matches!(recovered.info().generation, 1 | 2));
    }
}

#[test]
fn every_torn_write_prefix_falls_back_or_recovers_whole_root() {
    let prefixes = [0, 1, 7, 8, 12, 40, 64, 119, 120, 124, 128, 511, BLOCK_SIZE];
    for operation in [1, 3] {
        for torn_bytes in prefixes {
            let mut v = vol(10);
            v.commit_root(b"original").unwrap();
            let mut device = v.into_device();
            device
                .inject(Fault {
                    operation,
                    torn_bytes: Some(torn_bytes),
                })
                .unwrap();
            let mut in_flight = Volume::open(device).unwrap();
            assert!(matches!(
                in_flight.commit_root(b"replacement"),
                Err(Error::InjectedCrash)
            ));
            let mut recovered = Volume::open(in_flight.into_device().power_loss()).unwrap();
            let root = recovered.read_root().unwrap().unwrap();
            assert!(
                root == b"original" || root == b"replacement",
                "op={operation} bytes={torn_bytes} returned a bad root"
            );
            // The complete v1 superblock header and its CRC occupy
            // bytes 0..124; its remaining bytes are required zero padding.
            // A torn write can therefore publish a valid *whole* new root.
            if operation == 1 || torn_bytes < 124 {
                assert_eq!(root, b"original");
            } else {
                assert_eq!(root, b"replacement");
            }
        }
    }
}

#[test]
fn damaged_latest_payload_uses_intact_older_root_slot() {
    let mut v = vol(8);
    v.commit_root(b"safe-old").unwrap();
    v.commit_root(b"corrupt-new").unwrap();
    let mut device = v.into_device().power_loss();
    device.corrupt_stable_for_testing(3, 19, 0xab);
    let mut recovered = Volume::open(device).unwrap();
    assert_eq!(recovered.info().generation, 1);
    assert_eq!(recovered.read_root().unwrap(), Some(b"safe-old".to_vec()));
    // The next commit reuses generation 2; the bad orphan is overwritten.
    assert_eq!(recovered.commit_root(b"repaired-v2").unwrap().generation, 2);
}

#[test]
fn damaged_both_slots_and_blank_images_are_rejected() {
    assert!(matches!(
        Volume::open(CrashDevice::new(8)),
        Err(Error::NoValidRoot)
    ));
    let v = vol(8);
    let mut device = v.into_device().power_loss();
    device.corrupt_stable_for_testing(0, 18, 0xee);
    assert!(matches!(Volume::open(device), Err(Error::NoValidRoot)));

    let mut v = vol(8);
    v.commit_root(b"something").unwrap();
    let mut device = v.into_device().power_loss();
    device.corrupt_stable_for_testing(0, 0, 0xaa);
    device.corrupt_stable_for_testing(1, 0, 0xbb);
    assert!(matches!(Volume::open(device), Err(Error::NoValidRoot)));
}

#[test]
fn formatting_cannot_clobber_an_existing_image() {
    let device = vol(8).into_device().power_loss();
    assert!(matches!(Volume::format(device, 19), Err(Error::NotBlank)));
    assert!(matches!(
        Volume::format(CrashDevice::new(3), 19),
        Err(Error::Geometry)
    ));
    assert!(matches!(
        Volume::format(CrashDevice::new(8), 0),
        Err(Error::Geometry)
    ));
}

#[test]
fn superblock_generation_and_payload_corruption_do_not_escape_crc() {
    let mut v = vol(8);
    v.commit_root(b"root-1").unwrap();
    v.commit_root(b"root-2").unwrap();
    let mut d = v.into_device().power_loss();
    // Slot 0 is generation 2. A payload bit flip must fail its CRC.
    d.corrupt_stable_for_testing(0, 0, 0xaa);
    let mut recovered = Volume::open(d).unwrap();
    assert_eq!(recovered.info().generation, 1);
    assert_eq!(recovered.read_root().unwrap(), Some(b"root-1".to_vec()));
}

static UNIQUE: AtomicU64 = AtomicU64::new(0);
struct TempImage(PathBuf);
impl TempImage {
    fn new() -> Self {
        let id = UNIQUE.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(
            std::env::temp_dir().join(format!("bfs-ng-f0-{}-{nanos}-{id}.img", std::process::id())),
        )
    }
}
impl Drop for TempImage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn real_file_device_persists_across_close_and_reopen() {
    let name = TempImage::new();
    let device = FileDevice::create_new(&name.0, 12).unwrap();
    assert_eq!(device.block_count(), 12);
    let mut volume = Volume::format(device, 0xabba).unwrap();
    volume.commit_root(b"durable-gen1").unwrap();
    volume.commit_root(b"durable-gen2").unwrap();
    drop(volume);
    let mut reopened = Volume::open(FileDevice::open(&name.0).unwrap()).unwrap();
    assert_eq!(reopened.info().generation, 2);
    assert_eq!(
        reopened.read_root().unwrap(),
        Some(b"durable-gen2".to_vec())
    );
    drop(reopened);
    assert!(FileDevice::create_new(&name.0, 12).is_err());
}

#[test]
fn file_backed_corruption_recovers_old_root() {
    let name = TempImage::new();
    let mut v = Volume::format(FileDevice::create_new(&name.0, 8).unwrap(), 99).unwrap();
    v.commit_root(b"old").unwrap();
    v.commit_root(b"new").unwrap();
    drop(v);
    let mut device = FileDevice::open(&name.0).unwrap();
    let mut buf = [0_u8; BLOCK_SIZE];
    device.read_block(3, &mut buf).unwrap();
    buf[18] ^= 0xff;
    device.write_block(3, &buf).unwrap();
    device.sync().unwrap();
    let mut recovered = Volume::open(device).unwrap();
    assert_eq!(recovered.info().generation, 1);
    assert_eq!(recovered.read_root().unwrap(), Some(b"old".to_vec()));
}

#[test]
fn malformed_file_images_are_refused() {
    let name = TempImage::new();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&name.0)
        .unwrap();
    file.write_all(b"not even a full block").unwrap();
    file.sync_all().unwrap();
    assert!(matches!(FileDevice::open(&name.0), Err(Error::Geometry)));
    drop(file);
}
