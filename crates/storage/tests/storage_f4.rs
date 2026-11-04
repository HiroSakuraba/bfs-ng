use bfs_ng_storage::{
    AttributeDef, AttributeType, BlockDevice, CrashDevice, Error, Fault,
    FileDevice, FileSystem, Predicate, Schema, TextCase, Value, BLOCK_SIZE,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

fn schema() -> Schema {
    Schema::new(vec![
        AttributeDef { id:1, name:"asset.kind".into(),
            ty:AttributeType::Text{case:TextCase::AsciiFold,max_bytes:64}, indexed:true },
        AttributeDef { id:2, name:"asset.depends_on".into(),
            ty:AttributeType::TextSet{case:TextCase::ExactUtf8,max_bytes:64,max_items:8}, indexed:true },
        AttributeDef { id:3, name:"corpus.split".into(),
            ty:AttributeType::Text{case:TextCase::AsciiFold,max_bytes:24}, indexed:true },
        AttributeDef { id:4, name:"corpus.score".into(),
            ty:AttributeType::Signed, indexed:true },
        AttributeDef { id:5, name:"asset.updated_ns".into(),
            ty:AttributeType::TimestampNs, indexed:true },
        AttributeDef { id:6, name:"source.sha256".into(),
            ty:AttributeType::Hash256, indexed:true },
        AttributeDef { id:7, name:"internal.note".into(),
            ty:AttributeType::Text{case:TextCase::ExactUtf8,max_bytes:128}, indexed:false },
        AttributeDef { id:8, name:"fs.active".into(),
            ty:AttributeType::Bool, indexed:true },
        AttributeDef { id:9, name:"asset.sequence".into(),
            ty:AttributeType::Unsigned, indexed:true },
    ]).unwrap()
}
fn text(s:&str)->Value{Value::Text(s.to_owned())}
fn tags(xs:&[&str])->Value {
    Value::TextSet(xs.iter().map(|v|v.to_string()).collect::<BTreeSet<_>>())
}
fn fresh(blocks:usize)->FileSystem<CrashDevice>{
    FileSystem::format(CrashDevice::new(blocks),0xfeed).unwrap()
}
fn reopen(fs:FileSystem<CrashDevice>)->FileSystem<CrashDevice>{
    FileSystem::open(fs.into_device().power_loss()).unwrap()
}
fn compare(fs:&FileSystem<CrashDevice>, query:&[Predicate], limit:usize) {
    let fast=fs.search_attributes(query,limit).unwrap();
    let scan=fs.attribute_scan_oracle(query,limit).unwrap();
    assert_eq!(fast.files,scan.files);
    assert_eq!(fast.truncated,scan.truncated);
    assert_eq!(fast.generation,scan.generation);
}
fn two_files(fs:&mut FileSystem<CrashDevice>) {
    let mut tx=fs.begin();
    tx.install_schema(schema()).unwrap();
    tx.write_file("/shader",b"shader source").unwrap();
    tx.write_file("/texture",b"texture data").unwrap();
    tx.set_attributes("/shader", vec![
        ("asset.kind".into(),text("SHADER")),
        ("asset.depends_on".into(),tags(&["texture:7","shader:base"])),
        ("corpus.split".into(),text("TRAIN")),
        ("corpus.score".into(),Value::Signed(-19)),
        ("asset.updated_ns".into(),Value::TimestampNs(-3)),
        ("source.sha256".into(),Value::Hash256([7;32])),
        ("fs.active".into(),Value::Bool(true)),
        ("asset.sequence".into(),Value::Unsigned(22)),
    ]).unwrap();
    tx.set_attributes("/texture",vec![
        ("asset.kind".into(),text("texture")),
        ("corpus.split".into(),text("test")),
        ("corpus.score".into(),Value::Signed(11)),
    ]).unwrap();
    fs.commit(tx).unwrap();
}

#[test]
fn typed_queries_and_postings_survive_actual_image_reopen() {
    let mut fs=fresh(256);
    two_files(&mut fs);
    assert_eq!(fs.file_count(),2);
    assert_eq!(fs.list_dir("/").unwrap(),vec!["/shader","/texture"]);
    assert!(matches!(fs.read_file("/.bfs_f4_attrs"),Err(Error::InvalidPath)));
    assert!(matches!(fs.stat("/.bfs_f4_attrs"),Err(Error::InvalidPath)));
    let queries=vec![
        (vec![Predicate::eq("asset.kind",text("ShAdEr"))],1),
        (vec![Predicate::contains("asset.depends_on","texture:7")],1),
        (vec![Predicate::prefix("asset.kind","sh")],1),
        (vec![Predicate::range("corpus.score",Value::Signed(-20),Value::Signed(0))],1),
        (vec![Predicate::range("asset.updated_ns",Value::TimestampNs(-5),
            Value::TimestampNs(0))],1),
        (vec![Predicate::eq("source.sha256",Value::Hash256([7;32]))],1),
        (vec![Predicate::eq("fs.active",Value::Bool(true))],1),
        (vec![Predicate::range("asset.sequence",Value::Unsigned(22),
            Value::Unsigned(23))],1),
        (vec![Predicate::eq("corpus.split",text("train")),
            Predicate::eq("asset.kind",text("shader"))],1),
    ];
    for (query,count) in &queries {
        compare(&fs,query,100);
        assert_eq!(fs.search_attributes(query,100).unwrap().files.len(),*count);
    }
    assert_eq!(fs.file_attributes("/shader").unwrap()["asset.kind"],text("shader"));
    let fs=reopen(fs);
    assert_eq!(fs.schema().unwrap().get("asset.depends_on").unwrap().id,2);
    for (query,count) in &queries {
        compare(&fs,query,100);
        assert_eq!(fs.search_attributes(query,100).unwrap().files.len(),*count);
    }
}

#[test]
fn delete_and_recreate_file_does_not_inherit_stale_attributes() {
    let mut fs=fresh(192);two_files(&mut fs);
    let old_inode=fs.stat("/shader").unwrap().inode;
    let mut tx=fs.begin();
    tx.write_file("/shader",b"new contents same inode").unwrap();
    fs.commit(tx).unwrap();
    assert_eq!(fs.stat("/shader").unwrap().inode,old_inode);
    assert_eq!(fs.search_attributes(&[Predicate::eq("asset.kind",text("shader"))],10)
        .unwrap().files.len(),1);
    let mut tx=fs.begin();
    tx.remove_file("/shader").unwrap();
    tx.write_file("/shader",b"fresh inode").unwrap();
    fs.commit(tx).unwrap();
    assert_ne!(fs.stat("/shader").unwrap().inode,old_inode);
    assert!(fs.file_attributes("/shader").unwrap().is_empty());
    assert!(fs.search_attributes(&[Predicate::eq("asset.kind",text("shader"))],10)
        .unwrap().files.is_empty());
    let fs=reopen(fs);
    compare(&fs,&[Predicate::eq("asset.kind",text("texture"))],10);
    assert!(fs.file_attributes("/shader").unwrap().is_empty());
}

#[test]
fn snapshot_search_keeps_previous_index_and_file_contents() {
    let mut fs=fresh(256);two_files(&mut fs);
    let old=fs.search_attributes(&[Predicate::eq("asset.kind",text("shader"))],10)
        .unwrap().files;
    let mut tx=fs.begin();
    tx.pin_snapshot().unwrap();
    tx.write_file("/shader",b"updated code").unwrap();
    tx.set_attributes("/shader",vec![
        ("asset.kind".into(),text("material")),
        ("corpus.split".into(),text("DEV")),
    ]).unwrap();
    fs.commit(tx).unwrap();
    assert!(fs.search_attributes(&[Predicate::eq("asset.kind",text("shader"))],10)
        .unwrap().files.is_empty());
    assert_eq!(fs.search_snapshot_attributes(1,
        &[Predicate::eq("asset.kind",text("shader"))],10).unwrap().files,old);
    assert_eq!(fs.read_snapshot_file(1,"/shader").unwrap(),b"shader source");
    fs=reopen(fs);
    assert_eq!(fs.search_snapshot_attributes(1,
        &[Predicate::eq("asset.kind",text("shader"))],10).unwrap().files,old);
    let mut tx=fs.begin();tx.drop_snapshot(1).unwrap();fs.commit(tx).unwrap();
    assert!(matches!(fs.search_snapshot_attributes(1,
        &[Predicate::eq("asset.kind",text("shader"))],10),Err(Error::InvalidSnapshot)));
}

#[test]
fn hidden_catalog_is_private_and_query_errors_are_explicit() {
    let mut fs=fresh(96);
    assert!(fs.search_attributes(&[Predicate::eq("asset.kind",text("shader"))],10).is_err());
    let mut tx=fs.begin();
    for method in [tx.mkdir("/.bfs_f4_attrs"), tx.write_file("/.bfs_f4_attrs",b"bad")] {
        assert!(matches!(method,Err(Error::InvalidPath)));
    }
    tx.install_schema(schema()).unwrap();
    assert!(tx.install_schema(schema()).is_err());
    tx.write_file("/shader",b"payload").unwrap();
    assert!(tx.set_attributes("/shader",vec![("asset.kind".into(),Value::Signed(6))]).is_err());
    assert!(tx.set_attributes("/shader",vec![("unknown".into(),text("x"))]).is_err());
    tx.set_attributes("/shader",vec![("internal.note".into(),text("private"))]).unwrap();
    fs.commit(tx).unwrap();
    let snap=fs.schema().unwrap();
    assert_eq!(snap.definitions().count(),9);
    assert!(matches!(fs.search_attributes(
        &[Predicate::eq("internal.note",text("private"))],10),Err(Error::Attribute(_))));
    assert!(matches!(fs.search_attributes(&[],10),Err(Error::Attribute(_))));
    assert!(matches!(fs.search_attributes(
        &[Predicate::eq("asset.kind",text("x"))],0),Err(Error::Attribute(_))));
    assert!(matches!(fs.list_dir("/.bfs_f4_attrs"),Err(Error::InvalidPath)));
}

#[test]
fn entire_attribute_catalog_is_coherent_with_file_and_bounded_metadata_writes() {
    let mut fs=fresh(1024);
    let mut tx=fs.begin();tx.install_schema(schema()).unwrap();
    tx.mkdir("/corpus").unwrap();
    for i in 0..100 {
        let p=format!("/corpus/doc_{i:04}");
        tx.write_file(&p,format!("doc {i}").as_bytes()).unwrap();
        tx.set_attributes(&p,vec![
            ("corpus.split".into(),text(if i%3==0{"train"}else{"test"})),
            ("asset.sequence".into(),Value::Unsigned(i)),
        ]).unwrap();
    }
    fs.commit(tx).unwrap();
    let mut tx=fs.begin();
    tx.set_attributes("/corpus/doc_0042",vec![
        ("corpus.split".into(),text("dev")),
        ("asset.sequence".into(),Value::Unsigned(42)),
    ]).unwrap();
    fs.commit(tx).unwrap();
    assert!(fs.last_metadata_blocks_written()<=4,
        "attribute update must not rebuild entire F3 tree");
    compare(&fs,&[Predicate::eq("corpus.split",text("train"))],1000);
    let mut fs=reopen(fs);
    compare(&fs,&[Predicate::eq("corpus.split",text("dev"))],1000);
    let rare = [Predicate::eq("corpus.split",text("dev"))];
    let indexed = fs.search_attributes(&rare,1000).unwrap();
    let scanned = fs.attribute_scan_oracle(&rare,1000).unwrap();
    assert_eq!(indexed.files.len(),1);
    assert!(indexed.postings_examined < scanned.postings_examined,
        "selective query must scan fewer postings than files");
    assert_eq!(fs.read_file("/corpus/doc_0042").unwrap(),b"doc 42");
}

#[test]
fn corrupt_latest_index_payload_falls_back_to_prior_consistent_root() {
    let mut fs=fresh(256);two_files(&mut fs);
    let mut tx=fs.begin();
    tx.set_attributes("/shader",vec![("asset.kind".into(),text("material"))]).unwrap();
    fs.commit(tx).unwrap();
    let mut device=fs.into_device().power_loss();
    let mut target=None;
    for i in 3..device.block_count() {
        let mut buf=[0;BLOCK_SIZE];device.read_block(i,&mut buf).unwrap();
        if &buf[..8]==b"BFSNGD1\0" && &buf[32..40]==b"BFSAT4\0\0" {
            let birth=u32::from_le_bytes(buf[28..32].try_into().unwrap());
            if birth==2 { target=Some(i as usize); }
        }
    }
    let block=target.expect("attribute catalog committed as checked data extent");
    device.corrupt_stable_for_testing(block,46,0xf9);
    let fs=FileSystem::open(device).unwrap();
    assert_eq!(fs.generation(),1);
    assert_eq!(fs.search_attributes(&[Predicate::eq("asset.kind",text("shader"))],10)
        .unwrap().files.len(),1);
    assert!(fs.search_attributes(&[Predicate::eq("asset.kind",text("material"))],10)
        .unwrap().files.is_empty());
}

#[test]
fn crash_during_file_plus_index_publish_never_returns_mixed_generation() {
    for operation in 1..=14 {
        for torn_bytes in [None,Some(24),Some(124),Some(BLOCK_SIZE)] {
            let mut fs=fresh(256);two_files(&mut fs);
            let mut device=fs.into_device();
            device.inject(Fault{operation,torn_bytes}).unwrap();
            let mut fs=FileSystem::open(device).unwrap();
            let mut tx=fs.begin();
            tx.write_file("/shader",b"new material").unwrap();
            tx.set_attributes("/shader",vec![("asset.kind".into(),text("material"))]).unwrap();
            let result=fs.commit(tx);
            if result.is_ok() {continue;}
            assert!(matches!(result,Err(Error::InjectedCrash)));
            let mut fs=reopen(fs);
            let bytes=fs.read_file("/shader").unwrap();
            let new=fs.search_attributes(&[Predicate::eq("asset.kind",text("material"))],10)
                .unwrap().files.len();
            assert_eq!(bytes==b"new material",new==1,
                "mixed file/index generation at op {operation}, torn={torn_bytes:?}");
        }
    }
}

#[test]
fn randomized_search_postings_match_independent_m0_scan() {
    let mut fs=fresh(1024);two_files(&mut fs);
    let mut oracle=BTreeMap::new();
    for i in 0..45 {
        let path=format!("/z_{i:03}");
        let mut tx=fs.begin();
        tx.write_file(&path,format!("payload{i}").as_bytes()).unwrap();
        tx.set_attributes(&path,vec![
            ("asset.kind".into(),text(if i%2==0{"shader"}else{"texture"})),
            ("corpus.score".into(),Value::Signed((i as i64)-22)),
            ("asset.sequence".into(),Value::Unsigned(i as u64)),
        ]).unwrap();
        fs.commit(tx).unwrap();
        oracle.insert(i,path);
        if i%7==0 { fs=reopen(fs); }
        let queries=vec![
            vec![Predicate::eq("asset.kind",text("SHADER"))],
            vec![Predicate::prefix("asset.kind","te")],
            vec![Predicate::range("corpus.score",Value::Signed(-12),Value::Signed(4))],
            vec![Predicate::range("asset.sequence",Value::Unsigned(10),Value::Unsigned(30))],
            vec![Predicate::eq("asset.kind",text("shader")),
                Predicate::range("asset.sequence",Value::Unsigned(9),Value::Unsigned(27))],
        ];
        for query in queries { compare(&fs,&query,100); }
    }
    assert_eq!(oracle.len(),45);
}

#[test]
fn actual_file_backed_image_persists_attributes_and_snapshot_searches() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let path = std::env::temp_dir().join(format!(
        "bfs-f4-{}-{nanos}.img", std::process::id(),
    ));
    let mut fs=FileSystem::format(FileDevice::create_new(&path,256).unwrap(),0xface).unwrap();
    let mut tx=fs.begin();
    tx.install_schema(schema()).unwrap();
    tx.write_file("/axiom",b"source revision 1").unwrap();
    tx.set_attributes("/axiom",vec![
        ("corpus.split".into(),text("train")),
        ("asset.sequence".into(),Value::Unsigned(1)),
    ]).unwrap();
    fs.commit(tx).unwrap();
    let mut tx=fs.begin();
    tx.pin_snapshot().unwrap();
    tx.write_file("/axiom",b"source revision 2").unwrap();
    tx.set_attributes("/axiom",vec![
        ("corpus.split".into(),text("test")),
        ("asset.sequence".into(),Value::Unsigned(2)),
    ]).unwrap();
    fs.commit(tx).unwrap();
    drop(fs);
    let mut fs=FileSystem::open(FileDevice::open(&path).unwrap()).unwrap();
    assert_eq!(fs.read_file("/axiom").unwrap(),b"source revision 2");
    assert_eq!(fs.read_snapshot_file(1,"/axiom").unwrap(),b"source revision 1");
    assert_eq!(fs.search_attributes(
        &[Predicate::eq("corpus.split",text("test"))],10).unwrap().files.len(),1);
    assert_eq!(fs.search_snapshot_attributes(1,
        &[Predicate::eq("corpus.split",text("train"))],10).unwrap().files.len(),1);
    drop(fs);
    std::fs::remove_file(path).unwrap();
}
