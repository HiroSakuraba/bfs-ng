//! F2.R4: certified allocation, durable bounded sweeping, and recovery.
use bfs_ng_storage::{BLOCK_SIZE, BlockDevice, CrashDevice, Error, Fault, FileSystem, Volume};

fn fresh(blocks: usize) -> FileSystem<CrashDevice> {
    FileSystem::format(CrashDevice::new(blocks), 0xf2a4_2026).unwrap()
}

fn write(fs: &mut FileSystem<CrashDevice>, bytes: &[u8]) {
    let mut tx = fs.begin();
    tx.write_file("/item", bytes).unwrap();
    fs.commit(tx).unwrap();
}

fn reopen(fs: FileSystem<CrashDevice>) -> FileSystem<CrashDevice> {
    FileSystem::open(fs.into_device().power_loss()).unwrap()
}

fn sweep_cursor(device: CrashDevice) -> (CrashDevice, u64, u64, u64) {
    let mut volume = Volume::open(device).unwrap();
    let generation = volume.info().generation;
    let payload = volume.read_root().unwrap().unwrap();
    assert_eq!(&payload[..8], b"BFSNGM9\0");
    let snapshots = u32::from_le_bytes(payload[56..60].try_into().unwrap()) as usize;
    let journal_off = 64 + snapshots * 40 + 16;
    let journal_root =
        u64::from_le_bytes(payload[journal_off..journal_off + 8].try_into().unwrap());
    let cursor = u64::from_le_bytes(
        payload[journal_off + 16..journal_off + 24]
            .try_into()
            .unwrap(),
    );
    (volume.into_device(), generation, journal_root, cursor)
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

fn repair_crc(page: &mut [u8; BLOCK_SIZE]) {
    page[24..28].fill(0);
    let crc = crc32c(page);
    page[24..28].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn ordinary_commits_use_one_certification_scan_per_open_handle() {
    let mut fs = fresh(500);
    assert_eq!(fs.certification_scans(), 0);
    write(&mut fs, b"one");
    assert_eq!(fs.certification_scans(), 1);
    assert!(!fs.last_commit_used_certificate());
    for i in 0..12_u8 {
        write(&mut fs, &[i; 512]);
        assert!(fs.last_commit_used_certificate());
        assert_eq!(fs.certification_scans(), 1);
        let report = fs.allocation_report().unwrap();
        assert!(!report.reuse_blocked);
        assert!(report.journal_ready_blocks <= report.reclaimable_blocks);
        assert!(report.journal_deferred_blocks <= report.reclaimable_blocks);
    }
    fs = reopen(fs);
    assert_eq!(fs.certification_scans(), 0);
    write(&mut fs, b"after restart");
    assert_eq!(fs.certification_scans(), 1);
    assert!(!fs.last_commit_used_certificate());
    write(&mut fs, b"another");
    assert_eq!(fs.certification_scans(), 1);
    assert!(fs.last_commit_used_certificate());
    assert_eq!(reopen(fs).read_file("/item").unwrap(), b"another");
}

#[test]
fn bounded_retirement_sweep_resumes_after_power_loss() {
    let mut fs = fresh(700);
    let original = vec![0x5a_u8; 240_000]; // >32 retired data blocks
    write(&mut fs, &original); // g1
    write(&mut fs, b"replaced"); // g2, g1 is still the fallback
    let g2 = fs.allocation_report().unwrap();
    assert!(g2.journal_pending_blocks > 32);
    assert_eq!(g2.journal_deferred_blocks, 0);

    fs.reclaim_step().unwrap(); // g3: g1 rotates out
    let g3 = fs.allocation_report().unwrap();
    assert_eq!(g3.generation, 3);
    assert!(g3.journal_ready_blocks > 0);
    assert!(g3.journal_ready_blocks <= 32);
    assert!(g3.journal_deferred_blocks > 0);
    let (device, gen3, _, cursor3) = sweep_cursor(fs.into_device().power_loss());
    assert_eq!(gen3, 3);
    assert!(cursor3 > 0);
    fs = FileSystem::open(device).unwrap();
    assert_eq!(fs.certification_scans(), 0);
    fs.reclaim_step().unwrap(); // g4 continues the physical-address sweep
    let g4 = fs.allocation_report().unwrap();
    assert_eq!(g4.generation, 4);
    assert!(g4.journal_deferred_blocks < g3.journal_deferred_blocks);
    assert_eq!(fs.certification_scans(), 1);
    let (device, gen4, _, cursor4) = sweep_cursor(fs.into_device().power_loss());
    assert_eq!(gen4, 4);
    assert_ne!(cursor3, cursor4);
    let mut fs = FileSystem::open(device).unwrap();
    assert_eq!(fs.read_file("/item").unwrap(), b"replaced");
    let report = fs.allocation_report().unwrap();
    assert!(report.journal_ready_blocks <= report.reclaimable_blocks);
    assert!(report.journal_deferred_blocks <= report.reclaimable_blocks);
}

#[test]
fn forged_quarantine_of_fallback_owned_block_is_fail_closed() {
    let mut fs = fresh(200);
    write(&mut fs, b"old");
    write(&mut fs, b"new"); // g2 has an explicitly pending retired g1 block
    let (mut device, generation, journal_page, _) = sweep_cursor(fs.into_device().power_loss());
    assert_eq!(generation, 2);
    let mut page = [0; BLOCK_SIZE];
    device.read_block(journal_page, &mut page).unwrap();
    assert_eq!(&page[..8], b"BFSFRJ8\0");
    assert!(u16::from_le_bytes(page[28..30].try_into().unwrap()) > 0);
    // Forcing a pending run into the deferred class changes an eligibility
    // boundary without changing the referenced roots. Even with a valid CRC,
    // it cannot become certified by the independent two-root auditor.
    assert_eq!(u64::from_le_bytes(page[44..52].try_into().unwrap()), 3);
    page[44..52].copy_from_slice(&0_u64.to_le_bytes());
    repair_crc(&mut page);
    device.write_block(journal_page, &page).unwrap();
    device.sync().unwrap();
    let mut fs = FileSystem::open(device.power_loss()).unwrap();
    let report = fs.allocation_report().unwrap();
    assert!(report.reuse_blocked);
    assert_eq!(report.reclaimable_blocks, 0);
    write(&mut fs, b"successor");
    assert_eq!(reopen(fs).read_file("/item").unwrap(), b"successor");
}

#[test]
fn torn_sweep_checkpoint_never_advances_a_partial_cursor() {
    for operation in 1..=12 {
        let mut fs = fresh(280);
        write(&mut fs, &[0x61; 42_000]);
        write(&mut fs, b"current"); // g2, old data now pending
        let (_, g2, _, cursor2) = sweep_cursor(fs.into_device().power_loss());
        assert_eq!(g2, 2);
        assert_eq!(cursor2, 0);

        let mut fs = fresh(280);
        write(&mut fs, &[0x61; 42_000]);
        write(&mut fs, b"current");
        let mut device = fs.into_device();
        device
            .inject(Fault {
                operation,
                torn_bytes: Some(64),
            })
            .unwrap();
        let mut fs = FileSystem::open(device).unwrap();
        let result = fs.reclaim_step();
        assert!(result.is_ok() || matches!(result, Err(Error::InjectedCrash)));
        let (device, generation, _, persisted_cursor) = sweep_cursor(fs.into_device().power_loss());
        assert!(generation == 2 || generation == 3);
        if generation == 2 {
            assert_eq!(persisted_cursor, cursor2);
        }
        let mut fs = FileSystem::open(device).unwrap();
        assert_eq!(fs.read_file("/item").unwrap(), b"current");
        let check = fs.allocation_report().unwrap();
        assert!(check.journal_ready_blocks <= check.reclaimable_blocks);
        assert!(check.journal_deferred_blocks <= check.reclaimable_blocks);
    }
}
