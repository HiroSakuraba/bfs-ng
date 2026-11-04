//! F2.R3: crash-replayable free-run checkpoints and one-rotation epochs.
use bfs_ng_storage::{BLOCK_SIZE, BlockDevice, CrashDevice, Error, Fault, FileSystem, Volume};

fn blank(blocks: usize) -> FileSystem<CrashDevice> {
    FileSystem::format(CrashDevice::new(blocks), 0xfeed_2307).unwrap()
}

fn write(fs: &mut FileSystem<CrashDevice>, value: &[u8]) {
    let mut tx = fs.begin();
    tx.write_file("/entry", value).unwrap();
    fs.commit(tx).unwrap();
}

fn reopen(fs: FileSystem<CrashDevice>) -> FileSystem<CrashDevice> {
    FileSystem::open(fs.into_device().power_loss()).unwrap()
}

fn checkpoint_header(device: CrashDevice) -> (CrashDevice, u64, u32, u32, u64) {
    let mut volume = Volume::open(device).unwrap();
    let generation = volume.info().generation;
    let payload = volume.read_root().unwrap().unwrap();
    assert_eq!(&payload[..8], b"BFSNGM9\0");
    let pins = u32::from_le_bytes(payload[56..60].try_into().unwrap()) as usize;
    let offset = 64 + pins * 40 + 16;
    let first = u64::from_le_bytes(payload[offset..offset + 8].try_into().unwrap());
    let pages = u32::from_le_bytes(payload[offset + 8..offset + 12].try_into().unwrap());
    let runs = u32::from_le_bytes(payload[offset + 12..offset + 16].try_into().unwrap());
    (volume.into_device(), first, pages, runs, generation)
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

fn commit_crc(page: &mut [u8; BLOCK_SIZE]) {
    page[24..28].fill(0);
    let crc = crc32c(page);
    page[24..28].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn journal_is_durable_and_fallback_pages_mature_only_after_rotation() {
    let mut fs = blank(256);
    write(&mut fs, b"first");
    let g1 = fs.allocation_report().unwrap();
    assert_eq!(g1.reclaim_journal_pages, 1);
    assert_eq!(g1.reclaim_journal_runs, 0);

    write(&mut fs, b"second");
    let g2 = fs.allocation_report().unwrap();
    assert!(g2.journal_pending_blocks > 0);
    // Never-published path-copy intermediates may already be free at g2.
    // Fallback-owned pages must still remain in the pending class.
    assert!(g2.journal_ready_blocks <= g2.reclaimable_blocks);
    assert!(!g2.reuse_blocked);
    fs = reopen(fs);
    assert_eq!(
        fs.allocation_report().unwrap().journal_pending_blocks,
        g2.journal_pending_blocks
    );

    write(&mut fs, b"third");
    let g3 = fs.allocation_report().unwrap();
    assert!(g3.journal_ready_blocks > 0);
    assert!(g3.journal_pending_blocks > 0);
    assert!(g3.journal_ready_blocks <= g3.reclaimable_blocks);
    fs = reopen(fs);
    assert_eq!(
        fs.allocation_report().unwrap().journal_ready_blocks,
        g3.journal_ready_blocks
    );
    write(&mut fs, b"fourth");
    assert_eq!(reopen(fs).read_file("/entry").unwrap(), b"fourth");
}

#[test]
fn corrupted_latest_journal_page_rolls_back_and_quarantines_reuse() {
    let mut fs = blank(160);
    write(&mut fs, b"before");
    write(&mut fs, b"after");
    let (mut device, first, pages, _, _) = checkpoint_header(fs.into_device().power_loss());
    assert!(pages >= 1);
    device.corrupt_stable_for_testing(first as usize, 51, 0xab);
    let mut fs = FileSystem::open(device.power_loss()).unwrap();
    assert_eq!(fs.generation(), 1);
    assert_eq!(fs.read_file("/entry").unwrap(), b"before");
    let report = fs.allocation_report().unwrap();
    assert!(report.reuse_blocked);
    assert_eq!(report.reclaimable_blocks, 0);
}

#[test]
fn checksum_correct_early_promotion_of_fallback_block_is_quarantined() {
    let mut fs = blank(160);
    write(&mut fs, b"first");
    write(&mut fs, b"second");
    let (mut device, first, _, runs, generation) = checkpoint_header(fs.into_device().power_loss());
    assert_eq!(generation, 2);
    assert!(runs > 0);
    let mut page = [0; BLOCK_SIZE];
    device.read_block(first, &mut page).unwrap();
    assert_eq!(&page[..8], b"BFSFRJ8\0");
    let count = u16::from_le_bytes(page[28..30].try_into().unwrap()) as usize;
    let eligible_at = (0..count)
        .map(|index| 32 + index * 20 + 12)
        .find(|&at| u64::from_le_bytes(page[at..at + 8].try_into().unwrap()) == generation + 1)
        .expect("the oldest fallback must have pending journal blocks");
    // Earlier ready runs can now exist for never-published staged pages.
    // Deliberately forge a *pending* fallback-owned address, not a safe
    // transient page that was abandoned within the same transaction.
    page[eligible_at..eligible_at + 8].copy_from_slice(&generation.to_le_bytes());
    commit_crc(&mut page);
    device.write_block(first, &page).unwrap();
    device.sync().unwrap();
    let mut recovered = FileSystem::open(device.power_loss()).unwrap();
    assert_eq!(recovered.generation(), 2);
    assert_eq!(recovered.read_file("/entry").unwrap(), b"second");
    let audit = recovered.allocation_report().unwrap();
    assert!(
        audit.reuse_blocked,
        "a forged ready run overlaps the fallback"
    );
    assert_eq!(audit.reclaimable_blocks, 0);
    write(&mut recovered, b"third");
    assert_eq!(reopen(recovered).read_file("/entry").unwrap(), b"third");
}

#[test]
fn corrupted_f0_payload_with_valid_superblock_never_authorizes_reuse() {
    let mut fs = blank(160);
    write(&mut fs, b"original");
    write(&mut fs, b"latest");
    let mut device = fs.into_device().power_loss();
    // The old g1 payload occupies ring block 2 and remains referenced by
    // its valid F0 superblock until that candidate is rotated out.
    device.corrupt_stable_for_testing(2, 56, 0x82);
    let mut fs = FileSystem::open(device.power_loss()).unwrap();
    assert_eq!(fs.generation(), 2);
    assert_eq!(fs.read_file("/entry").unwrap(), b"latest");
    let report = fs.allocation_report().unwrap();
    assert!(report.reuse_blocked);
    assert_eq!(report.reclaimable_blocks, 0);
    write(&mut fs, b"next");
    assert_eq!(reopen(fs).read_file("/entry").unwrap(), b"next");
}

#[test]
fn snapshot_retirement_never_recycles_a_fallback_snapshot() {
    let mut fs = blank(320);
    write(&mut fs, b"original");
    let mut tx = fs.begin();
    tx.pin_snapshot().unwrap();
    tx.write_file("/entry", b"newer").unwrap();
    fs.commit(tx).unwrap();
    assert_eq!(fs.read_snapshot_file(1, "/entry").unwrap(), b"original");
    let mut tx = fs.begin();
    tx.drop_snapshot(1).unwrap();
    tx.write_file("/entry", b"latest").unwrap();
    fs.commit(tx).unwrap();
    // Root g2 still holds the snapshot while g3 is latest.
    assert!(fs.allocation_report().unwrap().delayed_free_blocks > 0);
    fs = reopen(fs);
    assert!(matches!(
        fs.read_snapshot_file(1, "/entry"),
        Err(Error::InvalidSnapshot)
    ));
    write(&mut fs, b"rotated");
    assert_eq!(reopen(fs).read_file("/entry").unwrap(), b"rotated");
}

#[test]
fn interrupted_journal_publication_recovers_one_whole_generation() {
    for operation in 1..=20 {
        for torn_bytes in [None, Some(24), Some(124), Some(BLOCK_SIZE)] {
            let mut fs = blank(160);
            write(&mut fs, b"old");
            let mut device = fs.into_device();
            device
                .inject(Fault {
                    operation,
                    torn_bytes,
                })
                .unwrap();
            let mut fs = FileSystem::open(device).unwrap();
            let mut tx = fs.begin();
            tx.write_file("/entry", b"new").unwrap();
            let result = fs.commit(tx);
            assert!(result.is_ok() || matches!(result, Err(Error::InjectedCrash)));
            let mut fs = reopen(fs);
            let bytes = fs.read_file("/entry").unwrap();
            assert!(bytes == b"old" || bytes == b"new");
            let report = fs.allocation_report().unwrap();
            assert!(report.reclaim_journal_pages >= 1);
            assert!(report.journal_ready_blocks <= report.reclaimable_blocks);
        }
    }
}
