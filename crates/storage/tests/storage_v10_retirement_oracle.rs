//! Independent, deterministic boundary/oracle checks for the opt-in F5.3
//! Free/DeadList planner. These are NOT a proof of graph-level accounting.
use bfs_ng_storage::experimental_v10::{
    DeadRun, FreeRun, SpaceDomain, TreeId, dead_item, free_item, retire_step_cursor,
    stage_space_delta,
};
use bfs_ng_storage::{BLOCK_SIZE, BlockDevice, CrashDevice, Error};
use std::collections::{BTreeMap, BTreeSet};

const VID: u128 = 0xf533_0a11_0c1e_2026;

#[test]
fn retirement_epoch_requires_representable_two_generation_fence() {
    for death in [u64::MAX - 1, u64::MAX] {
        let invalid = DeadRun {
            dead_generation: death,
            start: 80,
            blocks: 1,
            eligible_after: u64::MAX,
            domain: SpaceDomain::Global,
        };
        assert!(matches!(dead_item(invalid, 256), Err(Error::Geometry)));

        // Inject a syntactically valid wire item without using the encoder,
        // so that the authenticated-tree insertion gate is tested separately.
        let mut key = death.to_be_bytes().to_vec();
        key.extend_from_slice(&80_u64.to_be_bytes());
        let mut value = vec![0_u8; 16];
        value[..4].copy_from_slice(&1_u32.to_le_bytes());
        value[4] = SpaceDomain::Global as u8;
        value[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        let changes = BTreeMap::from([(key, Some(value))]);
        let mut disk = CrashDevice::new(256);
        assert!(matches!(
            stage_space_delta(
                &mut disk,
                TreeId::DeadList,
                None,
                &changes,
                &[250],
                VID,
                u64::MAX,
                256,
            ),
            Err(Error::CorruptTree)
        ));

        // The bounded planner must not interpret the same invalid record
        // as mature just because the current generation has saturated.
        let pending = BTreeMap::from([((death, 80), invalid)]);
        assert!(matches!(
            retire_step_cursor(
                &BTreeMap::new(),
                &pending,
                &BTreeSet::new(),
                u64::MAX,
                1,
                None,
            ),
            Err(Error::CorruptTree)
        ));
    }

    // The last representable death generation really can be delayed by two.
    let boundary = DeadRun {
        dead_generation: u64::MAX - 2,
        start: 80,
        blocks: 1,
        eligible_after: u64::MAX,
        domain: SpaceDomain::Snapshot,
    };
    assert!(dead_item(boundary, 256).is_ok());
}

#[test]
fn undersized_index_reserve_cannot_partially_write_tree_pages() {
    let mut disk = CrashDevice::new(10_000);
    let mut changes = BTreeMap::new();
    for i in 0..750_u64 {
        let (key, value) = free_item(
            FreeRun {
                start: 100 + 2 * i,
                blocks: 1,
                eligible_after: 5,
            },
            10_000,
        )
        .unwrap();
        changes.insert(key, Some(value));
    }

    // Two slots cannot contain 750 free-run records. The planner is purely
    // staging: its failed intermediate pages must never reach the device.
    let mut before_top = [0_u8; BLOCK_SIZE];
    let mut before_next = [0_u8; BLOCK_SIZE];
    disk.read_block(9_999, &mut before_top).unwrap();
    disk.read_block(9_998, &mut before_next).unwrap();
    let result = stage_space_delta(
        &mut disk,
        TreeId::Free,
        None,
        &changes,
        &[9_999, 9_998],
        VID,
        6,
        10_000,
    );
    assert!(matches!(result, Err(Error::NoSpace)));
    let mut after = [0_u8; BLOCK_SIZE];
    disk.read_block(9_999, &mut after).unwrap();
    assert_eq!(after, before_top);
    disk.read_block(9_998, &mut after).unwrap();
    assert_eq!(after, before_next);
}

#[test]
fn bounded_retirement_matches_an_independent_small_state_oracle() {
    // Exhaust all 32 ownership masks over five separated runs, seven clock
    // generations, six budgets, and six cursor positions. The slow reference
    // determines visited keys by rotating a vector rather than traversing
    // the implementation's BTreeMap ranges.
    let dead: BTreeMap<(u64, u64), DeadRun> = (0..5_u64)
        .map(|i| {
            let start = 40 + i * 2;
            (
                (4, start),
                DeadRun {
                    dead_generation: 4,
                    start,
                    blocks: 1,
                    eligible_after: 6 + i % 3,
                    domain: if i % 2 == 0 {
                        SpaceDomain::Snapshot
                    } else {
                        SpaceDomain::Global
                    },
                },
            )
        })
        .collect();

    for owner_mask in 0_u32..32 {
        let owners: BTreeSet<u64> = (0..5_u64)
            .filter(|&i| owner_mask & (1_u32 << i) != 0)
            .map(|i| 40 + i * 2)
            .collect();
        for generation in 4_u64..=10 {
            for budget in 0..=5 {
                for anchor_index in 0..=5 {
                    let after = if anchor_index == 0 {
                        None
                    } else {
                        Some((4, 40 + (anchor_index - 1) * 2))
                    };
                    let mut order: Vec<(u64, u64)> = dead.keys().copied().collect();
                    if let Some(anchor) = after {
                        let offset = order.iter().position(|&key| key > anchor).unwrap_or(0);
                        order.rotate_left(offset);
                    }
                    let visited: Vec<_> = order.into_iter().take(budget).collect();
                    let mut expected_dead = dead.clone();
                    let mut expected_free = BTreeMap::new();
                    for &key in &visited {
                        let item = dead[&key];
                        if item.eligible_after <= generation && !owners.contains(&item.start) {
                            expected_dead.remove(&key);
                            expected_free.insert(
                                item.start,
                                FreeRun {
                                    start: item.start,
                                    blocks: 1,
                                    eligible_after: item.eligible_after,
                                },
                            );
                        }
                    }

                    let actual = retire_step_cursor(
                        &BTreeMap::new(),
                        &dead,
                        &owners,
                        generation,
                        budget,
                        after,
                    )
                    .unwrap();
                    assert_eq!(actual.scanned, visited.len());
                    assert_eq!(actual.cursor, visited.last().copied().or(after));
                    assert_eq!(actual.released, expected_free.len() as u64);
                    assert_eq!(actual.dead, expected_dead);
                    assert_eq!(actual.free, expected_free);
                    assert!(actual.free.keys().all(|start| !owners.contains(start)));
                }
            }
        }
    }
}
