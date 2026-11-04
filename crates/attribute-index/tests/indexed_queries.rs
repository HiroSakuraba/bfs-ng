use bfs_ng_attribute_index::{
    AttributeDef, AttributeType, FileId, IndexError, IndexStore, Predicate, Schema, TextCase, Value,
};
use std::collections::BTreeSet;
use std::sync::{Arc, Barrier};

fn schema() -> Schema {
    Schema::new(vec![
        AttributeDef {
            id: 1,
            name: "asset.kind".into(),
            ty: AttributeType::Text {
                case: TextCase::AsciiFold,
                max_bytes: 128,
            },
            indexed: true,
        },
        AttributeDef {
            id: 2,
            name: "asset.depends_on".into(),
            ty: AttributeType::TextSet {
                case: TextCase::ExactUtf8,
                max_bytes: 128,
                max_items: 16,
            },
            indexed: true,
        },
        AttributeDef {
            id: 3,
            name: "corpus.validation".into(),
            ty: AttributeType::Text {
                case: TextCase::ExactUtf8,
                max_bytes: 64,
            },
            indexed: true,
        },
        AttributeDef {
            id: 4,
            name: "corpus.score".into(),
            ty: AttributeType::Signed,
            indexed: true,
        },
        AttributeDef {
            id: 5,
            name: "private.note".into(),
            ty: AttributeType::Text {
                case: TextCase::ExactUtf8,
                max_bytes: 100,
            },
            indexed: false,
        },
        AttributeDef {
            id: 6,
            name: "megadoc.split".into(),
            ty: AttributeType::Text {
                case: TextCase::AsciiFold,
                max_bytes: 16,
            },
            indexed: true,
        },
        AttributeDef {
            id: 7,
            name: "asset.updated_ns".into(),
            ty: AttributeType::TimestampNs,
            indexed: true,
        },
        AttributeDef {
            id: 8,
            name: "source.sha256".into(),
            ty: AttributeType::Hash256,
            indexed: true,
        },
        AttributeDef {
            id: 9,
            name: "fs.sequence".into(),
            ty: AttributeType::Unsigned,
            indexed: true,
        },
        AttributeDef {
            id: 10,
            name: "fs.active".into(),
            ty: AttributeType::Bool,
            indexed: true,
        },
    ])
    .unwrap()
}

fn id(n: u64) -> FileId {
    FileId {
        volume: 0xabc,
        inode: n,
    }
}
fn store() -> IndexStore {
    IndexStore::new(0xabc, schema()).unwrap()
}

fn text(x: &str) -> Value {
    Value::Text(x.into())
}
fn tags(xs: &[&str]) -> Value {
    Value::TextSet(xs.iter().map(|v| (*v).to_owned()).collect())
}
fn compare(snap: &bfs_ng_attribute_index::Snapshot, query: &[Predicate], limit: usize) {
    let actual = snap.indexed_query(query, limit).unwrap();
    let oracle = snap.full_scan_oracle(query, limit).unwrap();
    assert_eq!(actual.generation, oracle.generation);
    assert_eq!(actual.files, oracle.files);
    assert_eq!(actual.truncated, oracle.truncated);
    assert!(snap.index_is_consistent());
}

#[test]
fn axiom_dve_and_megadoc_queries_match_independent_scan() {
    let store = store();
    let mut tx = store.begin().unwrap();
    tx.upsert(
        id(1),
        vec![
            ("asset.kind".into(), text("SHADER")),
            ("asset.depends_on".into(), tags(&["texture:7", "shader:base"])),
            ("fs.sequence".into(), Value::Unsigned(12)),
            ("asset.updated_ns".into(), Value::TimestampNs(-3)),
            ("source.sha256".into(), Value::Hash256([7; 32])),
            ("fs.active".into(), Value::Bool(true)),
        ],
    )
    .unwrap();
    tx.upsert(
        id(2),
        vec![
            ("asset.kind".into(), text("texture")),
            ("asset.depends_on".into(), tags(&["source:abc"])),
            ("fs.sequence".into(), Value::Unsigned(13)),
            ("asset.updated_ns".into(), Value::TimestampNs(20)),
            ("fs.active".into(), Value::Bool(true)),
        ],
    )
    .unwrap();
    tx.upsert(
        id(3),
        vec![
            ("corpus.validation".into(), text("compiler_checked")),
            ("corpus.score".into(), Value::Signed(-1)),
            ("megadoc.split".into(), text("TRAIN")),
            ("fs.active".into(), Value::Bool(false)),
        ],
    )
    .unwrap();
    tx.upsert(
        id(4),
        vec![
            ("corpus.validation".into(), text("schema_only")),
            ("corpus.score".into(), Value::Signed(42)),
            ("megadoc.split".into(), text("test")),
        ],
    )
    .unwrap();
    assert_eq!(tx.commit().unwrap(), 1);
    let s = store.snapshot().unwrap();
    assert_eq!(s.file_count(), 4);
    let cases = vec![
        (vec![Predicate::eq("asset.kind", text("shader"))], vec![id(1)]),
        (
            vec![Predicate::contains("asset.depends_on", "texture:7")],
            vec![id(1)],
        ),
        (
            vec![Predicate::prefix("asset.kind", "SH")],
            vec![id(1)],
        ),
        (
            vec![Predicate::range(
                "corpus.score",
                Value::Signed(-2),
                Value::Signed(0),
            )],
            vec![id(3)],
        ),
        (
            vec![
                Predicate::eq("megadoc.split", text("TRAIN")),
                Predicate::eq("corpus.validation", text("compiler_checked")),
            ],
            vec![id(3)],
        ),
        (
            vec![Predicate::range(
                "fs.sequence",
                Value::Unsigned(12),
                Value::Unsigned(13),
            )],
            vec![id(1)],
        ),
        (
            vec![Predicate::range(
                "asset.updated_ns",
                Value::TimestampNs(-5),
                Value::TimestampNs(0),
            )],
            vec![id(1)],
        ),
        (
            vec![Predicate::eq("source.sha256", Value::Hash256([7; 32]))],
            vec![id(1)],
        ),
        (
            vec![Predicate::eq("fs.active", Value::Bool(true))],
            vec![id(1), id(2)],
        ),
    ];
    for (query, expected) in cases {
        compare(&s, &query, 100);
        assert_eq!(s.indexed_query(&query, 100).unwrap().files, expected);
    }
    assert_eq!(s.value(id(1), "asset.kind").unwrap(), Some(&text("shader")));
    assert_eq!(s.generation(), 1);
}

#[test]
fn replacing_and_deleting_records_removes_old_postings_atomically() {
    let store = store();
    let mut tx = store.begin().unwrap();
    tx.upsert(
        id(1),
        vec![
            ("asset.kind".into(), text("shader")),
            ("asset.depends_on".into(), tags(&["texture:a", "texture:b"])),
        ],
    )
    .unwrap();
    tx.commit().unwrap();
    let before = store.snapshot().unwrap();
    let mut update = store.begin().unwrap();
    update
        .upsert(
            id(1),
            vec![
                ("asset.kind".into(), text("material")),
                ("asset.depends_on".into(), tags(&["texture:c"])),
            ],
        )
        .unwrap();
    update.commit().unwrap();
    let after = store.snapshot().unwrap();
    assert_eq!(
        before
            .indexed_query(&[Predicate::contains("asset.depends_on", "texture:a")], 10)
            .unwrap()
            .files,
        vec![id(1)]
    );
    assert!(
        after
            .indexed_query(&[Predicate::contains("asset.depends_on", "texture:a")], 10)
            .unwrap()
            .files
            .is_empty()
    );
    assert_eq!(
        after
            .indexed_query(&[Predicate::contains("asset.depends_on", "texture:c")], 10)
            .unwrap()
            .files,
        vec![id(1)]
    );
    assert!(before.index_is_consistent() && after.index_is_consistent());

    let mut remove = store.begin().unwrap();
    remove.delete(id(1)).unwrap();
    remove.commit().unwrap();
    let empty = store.snapshot().unwrap();
    assert_eq!(empty.file_count(), 0);
    assert_eq!(empty.index_postings(), 0);
    assert_eq!(before.file_count(), 1);
    assert_eq!(after.file_count(), 1);
}

#[test]
fn stale_writers_are_rejected_without_partial_publication() {
    let store = store();
    let mut first = store.begin().unwrap();
    let mut second = store.begin().unwrap();
    first
        .upsert(id(1), vec![("asset.kind".into(), text("shader"))])
        .unwrap();
    second
        .upsert(id(2), vec![("asset.kind".into(), text("texture"))])
        .unwrap();
    assert_eq!(first.commit().unwrap(), 1);
    assert!(matches!(
        second.commit(),
        Err(IndexError::Conflict {
            expected: 0,
            actual: 1
        })
    ));
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.file_count(), 1);
    assert_eq!(
        snapshot
            .indexed_query(&[Predicate::eq("asset.kind", text("texture"))], 100)
            .unwrap()
            .files,
        Vec::new()
    );
    assert!(snapshot.index_is_consistent());
}

#[test]
fn concurrent_readers_keep_immutable_snapshots() {
    let store = store();
    let mut writer = store.begin().unwrap();
    writer
        .upsert(id(1), vec![("asset.kind".into(), text("shader"))])
        .unwrap();
    writer.commit().unwrap();
    let pinned = Arc::new(store.snapshot().unwrap());
    let barrier = Arc::new(Barrier::new(9));
    let mut readers = Vec::new();
    for _ in 0..8 {
        let snap = Arc::clone(&pinned);
        let b = Arc::clone(&barrier);
        readers.push(std::thread::spawn(move || {
            b.wait();
            for _ in 0..100 {
                assert_eq!(snap.generation(), 1);
                assert_eq!(
                    snap.indexed_query(&[Predicate::eq("asset.kind", text("shader"))], 10)
                        .unwrap()
                        .files,
                    vec![id(1)]
                );
            }
        }));
    }
    barrier.wait();
    let mut writer = store.begin().unwrap();
    writer
        .upsert(id(1), vec![("asset.kind".into(), text("material"))])
        .unwrap();
    writer.commit().unwrap();
    for reader in readers {
        reader.join().unwrap();
    }
    assert_eq!(pinned.generation(), 1);
    assert_eq!(store.snapshot().unwrap().generation(), 2);
    assert!(store.snapshot().unwrap().index_is_consistent());
}

#[test]
fn schema_and_query_errors_are_bounded() {
    let s = store();
    let invalid_file = FileId {
        volume: 11,
        inode: 1,
    };
    let mut tx = s.begin().unwrap();
    assert!(matches!(
        tx.upsert(invalid_file, vec![("asset.kind".into(), text("shader"))]),
        Err(IndexError::WrongVolume)
    ));
    assert!(matches!(tx.delete(id(0)), Err(IndexError::InvalidFileId)));
    assert!(matches!(
        tx.upsert(id(1), vec![("asset.kind".into(), text("šhader"))]),
        Err(IndexError::InvalidValue(_))
    ));
    assert!(matches!(
        tx.upsert(id(1), vec![("unknown".into(), Value::Bool(true))]),
        Err(IndexError::UnknownAttribute(_))
    ));
    assert!(matches!(
        tx.upsert(
            id(1),
            vec![
                ("asset.kind".into(), text("shader")),
                ("asset.kind".into(), text("material")),
            ],
        ),
        Err(IndexError::InvalidValue(_))
    ));
    assert!(matches!(
        tx.upsert(id(1), vec![("asset.depends_on".into(), tags(&["a"]))]),
        Ok(())
    ));
    assert_eq!(tx.commit().unwrap(), 1);
    let snap = s.snapshot().unwrap();
    let queries = vec![
        vec![Predicate::eq("private.note", text("private"))],
        vec![Predicate::eq("asset.depends_on", tags(&["a"]))],
        vec![Predicate::contains("asset.kind", "shader")],
        vec![Predicate::prefix("asset.kind", "")],
        vec![Predicate::range(
            "asset.kind",
            text("a"),
            text("z"),
        )],
        vec![Predicate::range(
            "fs.sequence",
            Value::Unsigned(9),
            Value::Unsigned(3),
        )],
    ];
    for query in queries {
        assert!(snap.indexed_query(&query, 10).is_err());
    }
    assert!(snap.indexed_query(&[], 10).is_err());
    assert!(matches!(
        snap.indexed_query(&[Predicate::contains("asset.depends_on", "a")], 0),
        Err(IndexError::InvalidLimit)
    ));
    assert!(matches!(
        snap.indexed_query(&[Predicate::contains("asset.depends_on", "a")], 10001),
        Err(IndexError::InvalidLimit)
    ));
    assert_eq!(snap.index_postings(), 1);
}

#[test]
fn casefold_canonicalization_deduplicates_multivalues() {
    let schema = Schema::new(vec![AttributeDef {
        id: 1,
        name: "labels.tags".into(),
        ty: AttributeType::TextSet {
            case: TextCase::AsciiFold,
            max_bytes: 32,
            max_items: 4,
        },
        indexed: true,
    }])
    .unwrap();
    let store = IndexStore::new(9, schema).unwrap();
    let file = FileId { volume: 9, inode: 12 };
    let mut tx = store.begin().unwrap();
    tx.upsert(
        file,
        vec![(
            "labels.tags".into(),
            Value::TextSet(BTreeSet::from([
                "Glass".to_owned(),
                "glass".to_owned(),
                "WOOD".to_owned(),
            ])),
        )],
    )
    .unwrap();
    tx.commit().unwrap();
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.index_postings(), 2);
    assert_eq!(
        snapshot
            .indexed_query(&[Predicate::contains("labels.tags", "GLASS")], 1)
            .unwrap()
            .files,
        vec![file]
    );
    assert!(snapshot.index_is_consistent());
}

fn advance(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

#[test]
fn seeded_randomized_index_vs_scan_differential() {
    let store = store();
    let mut rng = 0x84b4_2ee1u64;
    for step in 0..500 {
        let n = advance(&mut rng) % 85 + 1;
        let mut tx = store.begin().unwrap();
        if step % 11 == 0 {
            tx.delete(id(n)).unwrap();
        } else {
            let cat = match advance(&mut rng) % 4 {
                0 => "shader",
                1 => "texture",
                2 => "material",
                _ => "voxel",
            };
            let tag = match advance(&mut rng) % 3 {
                0 => "texture:a",
                1 => "texture:b",
                _ => "shader:base",
            };
            let status = if step % 7 == 0 {
                "compiler_checked"
            } else {
                "schema_only"
            };
            tx.upsert(
                id(n),
                vec![
                    ("asset.kind".into(), text(cat)),
                    ("asset.depends_on".into(), tags(&[tag])),
                    ("corpus.validation".into(), text(status)),
                    ("corpus.score".into(), Value::Signed((step as i64 % 31) - 15)),
                    ("megadoc.split".into(), text(if step % 5 == 0 { "test" } else { "train" })),
                    ("asset.updated_ns".into(), Value::TimestampNs(step as i64 - 200)),
                    ("fs.sequence".into(), Value::Unsigned(step as u64)),
                    ("fs.active".into(), Value::Bool(step % 2 == 0)),
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        if step % 9 == 0 {
            let snap = store.snapshot().unwrap();
            assert!(snap.index_is_consistent(), "step {step}");
            compare(
                &snap,
                &[Predicate::eq("asset.kind", text("shader"))],
                10000,
            );
            compare(
                &snap,
                &[
                    Predicate::contains("asset.depends_on", "texture:a"),
                    Predicate::eq("megadoc.split", text("train")),
                ],
                10000,
            );
            compare(
                &snap,
                &[Predicate::range(
                    "corpus.score",
                    Value::Signed(-9),
                    Value::Signed(5),
                )],
                10000,
            );
            compare(
                &snap,
                &[Predicate::range(
                    "asset.updated_ns",
                    Value::TimestampNs(-15),
                    Value::TimestampNs(260),
                )],
                10000,
            );
            compare(
                &snap,
                &[Predicate::prefix("asset.kind", "ma")],
                10000,
            );
            compare(
                &snap,
                &[Predicate::eq("fs.active", Value::Bool(true))],
                7,
            );
        }
    }
}

#[test]
fn query_limit_is_deterministic_and_reported() {
    let store = store();
    let mut tx = store.begin().unwrap();
    for n in 1..=10 {
        tx.upsert(id(n), vec![("asset.kind".into(), text("shader"))])
            .unwrap();
    }
    tx.commit().unwrap();
    let result = store
        .snapshot()
        .unwrap()
        .indexed_query(&[Predicate::eq("asset.kind", text("shader"))], 3)
        .unwrap();
    assert_eq!(result.files, vec![id(1), id(2), id(3)]);
    assert!(result.truncated);
    assert_eq!(result.postings_examined, 10);
}
