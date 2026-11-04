//! F4 versioned binary attribute catalog and independent posting index.
//! The catalog is stored in an ordinary *private* F3 checksummed file extent;
//! the namespace and this catalog share one atomic F0 root publication.
use super::{Error, Record, Result};
use bfs_ng_attribute_index::{
    AttributeDef, AttributeType, FileId, IndexStore, Predicate, QueryResult, Schema,
    TextCase, Value,
};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const INTERNAL_PATH: &str = "/.bfs_f4_attrs";
const MAGIC: [u8; 8] = *b"BFSAT4\0\0";
const VERSION: u16 = 1;
const MAX_BYTES: usize = 256 * 1024;
const MAX_DEFS: usize = 32;
const MAX_RECORD_ATTRS: usize = 64;
const MAX_POSTINGS: usize = 32_768;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Posting {
    attribute: u32,
    key: Vec<u8>,
    inode: u64,
}

#[derive(Clone)]
pub(super) struct Catalog {
    pub schema: Schema,
    rows: BTreeMap<u64, BTreeMap<u32, Value>>,
    postings: BTreeSet<Posting>,
}

fn attribute_error(error: impl std::fmt::Display) -> Error {
    Error::Attribute(error.to_string())
}

impl Catalog {
    pub fn empty(schema: Schema) -> Result<Self> {
        if schema.definitions().count() > MAX_DEFS {
            return Err(Error::TooLarge);
        }
        Ok(Self { schema, rows: BTreeMap::new(), postings: BTreeSet::new() })
    }

    pub fn get(&self, inode: u64) -> BTreeMap<String, Value> {
        self.rows.get(&inode).into_iter().flat_map(|attrs| attrs.iter())
            .filter_map(|(id, value)| self.schema.by_id(*id)
                .map(|def| (def.name.clone(), value.clone())))
            .collect()
    }

    pub fn upsert(
        &mut self, inode: u64,
        attrs: impl IntoIterator<Item = (String, Value)>,
    ) -> Result<()> {
        if inode < 2 {
            return Err(Error::Attribute("invalid file inode".into()));
        }
        let mut next = BTreeMap::new();
        for (name, value) in attrs {
            let def = self.schema.get(&name).map_err(attribute_error)?;
            let value = def.normalize(value).map_err(attribute_error)?;
            if next.insert(def.id, value).is_some() {
                return Err(Error::Attribute(format!("duplicate attribute {name}")));
            }
            if next.len() > MAX_RECORD_ATTRS {
                return Err(Error::TooLarge);
            }
        }
        if next.is_empty() {
            self.rows.remove(&inode);
        } else {
            self.rows.insert(inode, next);
        }
        self.rebuild_postings()
    }

    pub fn remove(&mut self, inode: u64) -> Result<()> {
        if self.rows.remove(&inode).is_some() {
            self.rebuild_postings()?;
        }
        Ok(())
    }

    fn computed_postings(&self) -> Result<BTreeSet<Posting>> {
        let mut postings = BTreeSet::new();
        for (inode, attrs) in &self.rows {
            for (id, value) in attrs {
                let def = self.schema.by_id(*id).ok_or(Error::CorruptTree)?;
                for key in def.index_keys(value) {
                    postings.insert(Posting { attribute: *id, key, inode: *inode });
                    if postings.len() > MAX_POSTINGS {
                        return Err(Error::TooLarge);
                    }
                }
            }
        }
        Ok(postings)
    }

    fn rebuild_postings(&mut self) -> Result<()> {
        self.postings = self.computed_postings()?;
        Ok(())
    }

    pub fn validate_files(&self, files: &BTreeMap<String, Record>) -> Result<()> {
        let by_inode: BTreeSet<_> = files.iter()
            .filter(|(path, record)| path.as_str() != INTERNAL_PATH && !record.directory)
            .map(|(_, record)| record.inode)
            .collect();
        if self.rows.keys().any(|inode| !by_inode.contains(inode)) {
            return Err(Error::CorruptTree);
        }
        if self.computed_postings()? != self.postings {
            return Err(Error::CorruptTree);
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        put16(&mut bytes, VERSION);
        put16(&mut bytes, self.schema.definitions().count() as u16);
        for def in self.schema.definitions() {
            put32(&mut bytes, def.id);
            put_str(&mut bytes, &def.name)?;
            let (kind, case, maxbytes, maxitems) = encode_type(&def.ty)?;
            bytes.extend_from_slice(&[kind, case, u8::from(def.indexed), 0]);
            put16(&mut bytes, maxbytes);
            put16(&mut bytes, maxitems);
        }
        put32(&mut bytes, self.rows.len() as u32);
        for (inode, attributes) in &self.rows {
            put64(&mut bytes, *inode);
            put16(&mut bytes, attributes.len() as u16);
            for (id, value) in attributes {
                let def = self.schema.by_id(*id).ok_or(Error::CorruptTree)?;
                put32(&mut bytes, *id);
                let mut raw = Vec::new();
                encode_value(&mut raw, value, &def.ty)?;
                put32(&mut bytes, raw.len() as u32);
                bytes.extend_from_slice(&raw);
            }
        }
        put32(&mut bytes, self.postings.len() as u32);
        for item in &self.postings {
            put32(&mut bytes, item.attribute);
            put16(&mut bytes, item.key.len() as u16);
            bytes.extend_from_slice(&item.key);
            put64(&mut bytes, item.inode);
        }
        if bytes.len() > MAX_BYTES {
            return Err(Error::TooLarge);
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_BYTES || bytes.len() < 16 {
            return Err(Error::CorruptTree);
        }
        let mut d = Decoder::new(bytes);
        if d.take(8)? != MAGIC.as_slice() || d.u16()? != VERSION {
            return Err(Error::CorruptTree);
        }
        let ndefs = d.u16()? as usize;
        if ndefs == 0 || ndefs > MAX_DEFS {
            return Err(Error::CorruptTree);
        }
        let mut defs = Vec::new();
        for _ in 0..ndefs {
            let id = d.u32()?;
            let name = d.text(128)?;
            let kind = d.u8()?;
            let case = d.u8()?;
            let indexed = d.u8()?;
            if d.u8()? != 0 || indexed > 1 {
                return Err(Error::CorruptTree);
            }
            let ty = decode_type(kind, case, d.u16()?, d.u16()?)?;
            defs.push(AttributeDef { id, name, ty, indexed: indexed == 1 });
        }
        let schema = Schema::new(defs).map_err(|_| Error::CorruptTree)?;
        let mut catalog = Self::empty(schema).map_err(|_| Error::CorruptTree)?;
        let count = d.u32()? as usize;
        if count > 2048 {
            return Err(Error::CorruptTree);
        }
        let mut prev_inode = 0;
        for _ in 0..count {
            let inode = d.u64()?;
            let attr_count = d.u16()? as usize;
            if inode < 2 || inode <= prev_inode || attr_count == 0
                || attr_count > MAX_RECORD_ATTRS
            {
                return Err(Error::CorruptTree);
            }
            prev_inode = inode;
            let mut attrs = BTreeMap::new();
            let mut prev_attr = 0;
            for _ in 0..attr_count {
                let id = d.u32()?;
                let size = d.u32()? as usize;
                if id <= prev_attr || size > 128 * 1024 {
                    return Err(Error::CorruptTree);
                }
                prev_attr = id;
                let raw = d.take(size)?;
                let def = catalog.schema.by_id(id).ok_or(Error::CorruptTree)?;
                let mut val = Decoder::new(raw);
                let value = decode_value(&mut val, &def.ty)?;
                if !val.finished()
                    || def.normalize(value.clone()).map_err(|_| Error::CorruptTree)? != value
                {
                    return Err(Error::CorruptTree);
                }
                attrs.insert(id, value);
            }
            catalog.rows.insert(inode, attrs);
        }
        let postings = d.u32()? as usize;
        if postings > MAX_POSTINGS {
            return Err(Error::CorruptTree);
        }
        let mut previous: Option<Posting> = None;
        for _ in 0..postings {
            let attribute = d.u32()?;
            let len = d.u16()? as usize;
            if len == 0 || len > 1100 {
                return Err(Error::CorruptTree);
            }
            let key = d.take(len)?.to_vec();
            let inode = d.u64()?;
            let posting = Posting { attribute, key, inode };
            if previous.as_ref().is_some_and(|p| posting <= *p) {
                return Err(Error::CorruptTree);
            }
            previous = Some(posting.clone());
            catalog.postings.insert(posting);
        }
        if !d.finished() || catalog.computed_postings().map_err(|_| Error::CorruptTree)?
            != catalog.postings
        {
            return Err(Error::CorruptTree);
        }
        Ok(catalog)
    }

    pub fn query(&self, volume: u128, generation: u64,
        predicates: &[Predicate], limit: usize) -> Result<QueryResult>
    {
        if predicates.is_empty() || predicates.len() > 16 || limit == 0 || limit > 10_000 {
            return Err(Error::Attribute("require 1..=16 predicates and 1..=10000 limit".into()));
        }
        let prepared = predicates.iter()
            .map(|p| self.prepare(p))
            .collect::<Result<Vec<_>>>()?;
        let mut sets = Vec::new();
        let mut examined = 0usize;
        for (id, condition) in &prepared {
            let first = Posting { attribute: *id, key: condition.lower().to_vec(), inode: 0 };
            let mut selected = BTreeSet::new();
            for post in self.postings.range(first..) {
                if post.attribute != *id || !condition.key_matches(&post.key) {
                    break;
                }
                examined += 1;
                selected.insert(post.inode);
            }
            sets.push(selected);
        }
        sets.sort_by_key(BTreeSet::len);
        let mut files = sets.remove(0);
        for next in sets {
            files.retain(|inode| next.contains(inode));
        }
        let truncated = files.len() > limit;
        Ok(QueryResult {
            generation,
            files: files.into_iter().take(limit)
                .map(|inode| FileId { volume, inode }).collect(),
            truncated, postings_examined: examined,
        })
    }

    /// An independent full record scan using M0's query semantics.
    pub fn oracle(&self, volume: u128, generation: u64,
        predicates: &[Predicate], limit: usize) -> Result<QueryResult>
    {
        let store = IndexStore::new(volume, self.schema.clone()).map_err(attribute_error)?;
        let mut tx = store.begin().map_err(attribute_error)?;
        for (inode, attrs) in &self.rows {
            let values = attrs.iter().map(|(id, value)| {
                (self.schema.by_id(*id).expect("verified schema").name.clone(), value.clone())
            });
            tx.upsert(FileId { volume, inode: *inode }, values).map_err(attribute_error)?;
        }
        tx.commit().map_err(attribute_error)?;
        let mut result = store.snapshot().map_err(attribute_error)?
            .full_scan_oracle(predicates, limit).map_err(attribute_error)?;
        result.generation = generation;
        Ok(result)
    }

    fn prepare(&self, predicate: &Predicate) -> Result<(u32, Condition)> {
        let (name, kind) = match predicate {
            Predicate::Equal { attribute, value } => (attribute, Input::Equal(value)),
            Predicate::Contains { attribute, member } => (attribute, Input::Contains(member)),
            Predicate::Prefix { attribute, prefix } => (attribute, Input::Prefix(prefix)),
            Predicate::Range { attribute, lower, upper } =>
                (attribute, Input::Range(lower, upper)),
        };
        let def = self.schema.get(name).map_err(attribute_error)?;
        if !def.indexed {
            return Err(Error::Attribute(format!("{name} is not indexed")));
        }
        let prepared = match kind {
            Input::Equal(v) => {
                if matches!(&def.ty, AttributeType::TextSet { .. }) {
                    return Err(Error::Attribute("use Contains for text sets".into()));
                }
                let v = def.normalize(v.clone()).map_err(attribute_error)?;
                Condition::Equal(def.index_keys(&v)[0].clone())
            }
            Input::Contains(value) => {
                if !matches!(&def.ty, AttributeType::TextSet { .. }) {
                    return Err(Error::Attribute("Contains requires TextSet".into()));
                }
                let set = Value::TextSet(BTreeSet::from([value.clone()]));
                let v = def.normalize(set).map_err(attribute_error)?;
                Condition::Equal(def.index_keys(&v)[0].clone())
            }
            Input::Prefix(value) => {
                if !matches!(&def.ty, AttributeType::Text { .. }) || value.is_empty() {
                    return Err(Error::Attribute("Prefix requires nonempty Text".into()));
                }
                let v = def.normalize(Value::Text(value.clone())).map_err(attribute_error)?;
                Condition::Prefix(def.index_keys(&v)[0].clone())
            }
            Input::Range(low, high) => {
                if !matches!(&def.ty, AttributeType::Signed |
                    AttributeType::Unsigned | AttributeType::TimestampNs)
                {
                    return Err(Error::Attribute("Range requires integer or timestamp".into()));
                }
                let lo = def.normalize(low.clone()).map_err(attribute_error)?;
                let hi = def.normalize(high.clone()).map_err(attribute_error)?;
                let lo = def.index_keys(&lo)[0].clone();
                let hi = def.index_keys(&hi)[0].clone();
                if lo >= hi {
                    return Err(Error::Attribute("invalid range".into()));
                }
                Condition::Range(lo, hi)
            }
        };
        Ok((def.id, prepared))
    }
}

enum Input<'a> {
    Equal(&'a Value),
    Contains(&'a String),
    Prefix(&'a String),
    Range(&'a Value, &'a Value),
}
enum Condition {
    Equal(Vec<u8>),
    Prefix(Vec<u8>),
    Range(Vec<u8>, Vec<u8>),
}
impl Condition {
    fn lower(&self) -> &[u8] {
        match self {
            Self::Equal(key) | Self::Prefix(key) | Self::Range(key, _) => key,
        }
    }
    fn key_matches(&self, key: &[u8]) -> bool {
        match self {
            Self::Equal(v) => key == v,
            Self::Prefix(v) => key.starts_with(v),
            Self::Range(lo, hi) => key >= lo.as_slice() && key < hi.as_slice(),
        }
    }
}

fn put16(out: &mut Vec<u8>, v: u16) { out.extend_from_slice(&v.to_le_bytes()); }
fn put32(out: &mut Vec<u8>, v: u32) { out.extend_from_slice(&v.to_le_bytes()); }
fn put64(out: &mut Vec<u8>, v: u64) { out.extend_from_slice(&v.to_le_bytes()); }
fn put_str(out: &mut Vec<u8>, text: &str) -> Result<()> {
    if text.len() > u16::MAX as usize { return Err(Error::TooLarge); }
    put16(out, text.len() as u16);
    out.extend_from_slice(text.as_bytes());
    Ok(())
}
fn encode_type(ty: &AttributeType) -> Result<(u8,u8,u16,u16)> {
    Ok(match ty {
        AttributeType::Bool => (1,0,0,0),
        AttributeType::Signed => (2,0,0,0),
        AttributeType::Unsigned => (3,0,0,0),
        AttributeType::TimestampNs => (4,0,0,0),
        AttributeType::Hash256 => (5,0,0,0),
        AttributeType::Text {case, max_bytes} =>
            (6, case_code(*case), *max_bytes as u16, 0),
        AttributeType::TextSet {case, max_bytes, max_items} =>
            (7, case_code(*case), *max_bytes as u16, *max_items as u16),
    })
}
fn case_code(case: TextCase) -> u8 {
    match case { TextCase::ExactUtf8 => 0, TextCase::AsciiFold => 1 }
}
fn decode_type(kind:u8,case:u8,max:u16,items:u16) -> Result<AttributeType> {
    let text_case = match case { 0=>TextCase::ExactUtf8, 1=>TextCase::AsciiFold,
        _=>return Err(Error::CorruptTree) };
    let numeric = |expected:u8| kind==expected && case==0 && max==0 && items==0;
    Ok(match kind {
        1 if numeric(1)=>AttributeType::Bool,
        2 if numeric(2)=>AttributeType::Signed,
        3 if numeric(3)=>AttributeType::Unsigned,
        4 if numeric(4)=>AttributeType::TimestampNs,
        5 if numeric(5)=>AttributeType::Hash256,
        6 if items==0 && max>0 && max<=1024 =>
            AttributeType::Text {case:text_case,max_bytes:max as usize},
        7 if max>0 && max<=1024 && items>0 && items<=128 =>
            AttributeType::TextSet {case:text_case,max_bytes:max as usize,
                max_items:items as usize},
        _=>return Err(Error::CorruptTree),
    })
}
fn encode_value(out:&mut Vec<u8>,value:&Value,ty:&AttributeType)->Result<()> {
    match (ty,value) {
        (AttributeType::Bool,Value::Bool(b))=>out.push(u8::from(*b)),
        (AttributeType::Signed,Value::Signed(v)) |
        (AttributeType::TimestampNs,Value::TimestampNs(v)) =>
            out.extend_from_slice(&v.to_le_bytes()),
        (AttributeType::Unsigned,Value::Unsigned(v))=>put64(out,*v),
        (AttributeType::Hash256,Value::Hash256(v))=>out.extend_from_slice(v),
        (AttributeType::Text{..},Value::Text(s))=>put_str(out,s)?,
        (AttributeType::TextSet{..},Value::TextSet(items))=>{
            put16(out,items.len() as u16);
            for item in items { put_str(out,item)?; }
        }
        _=>return Err(Error::CorruptTree),
    }
    Ok(())
}
fn decode_value(d:&mut Decoder<'_>,ty:&AttributeType)->Result<Value> {
    Ok(match ty {
        AttributeType::Bool => match d.u8()? {
            0=>Value::Bool(false),1=>Value::Bool(true),_=>return Err(Error::CorruptTree)
        },
        AttributeType::Signed => Value::Signed(d.u64()? as i64),
        AttributeType::TimestampNs => Value::TimestampNs(d.u64()? as i64),
        AttributeType::Unsigned => Value::Unsigned(d.u64()?),
        AttributeType::Hash256 => {
            let mut b=[0;32];b.copy_from_slice(d.take(32)?);Value::Hash256(b)
        }
        AttributeType::Text{max_bytes,..} => Value::Text(d.text(*max_bytes)?),
        AttributeType::TextSet{max_bytes,max_items,..} => {
            let count=d.u16()? as usize;
            if count>*max_items { return Err(Error::CorruptTree); }
            let mut values=BTreeSet::new();
            let mut prev=String::new();
            for _ in 0..count {
                let item=d.text(*max_bytes)?;
                if !values.is_empty() && item<=prev { return Err(Error::CorruptTree); }
                prev=item.clone();values.insert(item);
            }
            Value::TextSet(values)
        }
    })
}

struct Decoder<'a> { data:&'a [u8], pos:usize }
impl<'a> Decoder<'a> {
    fn new(data:&'a [u8])->Self { Self{data,pos:0} }
    fn take(&mut self,n:usize)->Result<&'a [u8]> {
        let end=self.pos.checked_add(n).ok_or(Error::CorruptTree)?;
        if end>self.data.len(){return Err(Error::CorruptTree);}
        let out=&self.data[self.pos..end];self.pos=end;Ok(out)
    }
    fn u8(&mut self)->Result<u8>{Ok(self.take(1)?[0])}
    fn u16(&mut self)->Result<u16>{
        Ok(u16::from_le_bytes(self.take(2)?.try_into().map_err(|_|Error::CorruptTree)?))
    }
    fn u32(&mut self)->Result<u32>{
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_|Error::CorruptTree)?))
    }
    fn u64(&mut self)->Result<u64>{
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_|Error::CorruptTree)?))
    }
    fn text(&mut self, limit:usize)->Result<String>{
        let len=self.u16()? as usize;
        if len>limit {return Err(Error::CorruptTree);}
        let raw=self.take(len)?;
        Ok(std::str::from_utf8(raw).map_err(|_|Error::CorruptTree)?.to_owned())
    }
    fn finished(&self)->bool {self.pos==self.data.len()}
}
