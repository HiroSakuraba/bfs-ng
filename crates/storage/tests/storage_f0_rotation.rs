//! F0.R1: permanent three-block root-payload ring under F2.R4 CoW load.
//! All images in these tests are disposable; CrashDevice is a durability model.
use bfs_ng_storage::{BLOCK_SIZE, BlockDevice, CrashDevice, Error, Fault, FileSystem, Volume};

fn image() -> FileSystem<CrashDevice> {
    FileSystem::format(CrashDevice::new(384), 0xf001_2026).unwrap()
}

fn replace(fs: &mut FileSystem<CrashDevice>, generation: u64) {
    let mut tx = fs.begin();
    tx.write_file("/entry", format!("value-{generation}").as_bytes())
        .unwrap();
    assert_eq!(fs.commit(tx).unwrap(), generation);
}

fn crc32c(input: &[u8]) -> u32 {
    let mut state = !0_u32;
    for &b in input {
        state ^= b as u32;
        for _ in 0..8 {
            state = (state >> 1) ^ (0x82f6_3b78 & (0_u32.wrapping_sub(state & 1)));
        }
    }
    !state
}

#[test]
fn f0_v1_header_with_repaired_crc_cannot_mount_as_rotating_v2() {
    let volume = Volume::format(CrashDevice::new(5), 0xcafe).unwrap();
    let mut disk = volume.into_device();
    let mut superblock = [0_u8; BLOCK_SIZE];
    disk.read_block(0, &mut superblock).unwrap();
    assert_eq!(u32::from_le_bytes(superblock[8..12].try_into().unwrap()), 2);
    superblock[8..12].copy_from_slice(&1_u32.to_le_bytes());
    let checksum = crc32c(&superblock[..120]);
    superblock[120..124].copy_from_slice(&checksum.to_le_bytes());
    disk.write_block(0, &superblock).unwrap();
    disk.sync().unwrap();
    assert!(matches!(Volume::open(disk.power_loss()), Err(Error::NoValidRoot)));
}

#[test]
fn more_generations_than_image_blocks_with_repeated_mounts() {
    // Under F0 v1, the payload would reach data blocks permanently before
    // this loop could complete. F0 v2 never allocates a root payload >4.
    let mut fs = image();
    for generation in 1..=440 {
        replace(&mut fs, generation);
        if generation % 23 == 0 {
            let mut volume = Volume::open(fs.into_device().power_loss()).unwrap();
            let info = volume.info();
            assert_eq!(info.generation, generation);
            assert_eq!(info.root_block, Some(2 + (generation - 1) % 3));
            assert!(!volume.read_root().unwrap().unwrap().is_empty());
            fs = FileSystem::open(volume.into_device()).unwrap();
            assert_eq!(
                fs.read_file("/entry").unwrap(),
                format!("value-{generation}").into_bytes()
            );
            let report = fs.allocation_report().unwrap();
            assert!(!report.reuse_blocked);
            assert!(report.journal_ready_blocks <= report.reclaimable_blocks);
            assert!(report.journal_deferred_blocks <= report.reclaimable_blocks);
            assert!(report.high_watermark > 4);
        }
    }
    assert_eq!(fs.generation(), 440);
    assert_eq!(
        FileSystem::open(fs.into_device().power_loss())
            .unwrap()
            .read_file("/entry")
            .unwrap(),
        b"value-440"
    );
}

#[test]
fn root_ring_wrap_faults_preserve_entire_f2_generation_and_ownership() {
    // Simulate every write/sync fault position around generation 8 after
    // several complete rotations of root payload addresses [2, 3, 4].
    for operation in 1..=13 {
        for torn_bytes in [0, 64, 124, BLOCK_SIZE] {
            let mut fs = FileSystem::format(CrashDevice::new(192), 0xfa17).unwrap();
            for generation in 1..=7 {
                replace(&mut fs, generation);
            }
            let mut device = fs.into_device().power_loss();
            device.inject(Fault {
                operation,
                torn_bytes: Some(torn_bytes),
            }).unwrap();
            let mut fs = FileSystem::open(device).unwrap();
            let mut tx = fs.begin();
            tx.write_file("/entry", b"value-8").unwrap();
            let result = fs.commit(tx);
            assert!(
                result.is_ok() || matches!(&result, Err(Error::InjectedCrash)),
                "unexpected error for operation={operation}, torn={torn_bytes}: {result:?}"
            );
            let mut recovered = FileSystem::open(fs.into_device().power_loss()).unwrap();
            assert!(matches!(recovered.generation(), 7 | 8));
            assert_eq!(
                recovered.read_file("/entry").unwrap(),
                if recovered.generation() == 7 { b"value-7" } else { b"value-8" }
            );
            // Independently inspect two F0 candidates. A corrupt candidate
            // can quarantine reuse, but never authorize an unsafe free block.
            let report = recovered.allocation_report().unwrap();
            assert!(report.journal_ready_blocks <= report.image_blocks as usize);
            if !report.reuse_blocked {
                assert!(report.journal_ready_blocks <= report.reclaimable_blocks);
                assert!(report.journal_deferred_blocks <= report.reclaimable_blocks);
            }
        }
    }
}
