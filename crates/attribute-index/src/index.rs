//! Atomic in-memory index publication and snapshot-consistent query engine.
use crate::schema::{AttributeType, IndexError, Schema, Value, scalar_key};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

pub const MAX_QUERY_LIMIT: usize = 10_000;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileId {
    pub volume: u128,
    pub inode: u64,
}

#[derive(Clone, Debug)]
pub enum Predicate {
    Equal {
        attribute: String,
        value: Value,
    },
    Contains {
        attribute: String,
        member: String,
    },
    Prefix {
        attribute: String,
        prefix: String,
    },
    /// Inclusive lower, exclusive upper. Supported for i64/u64/timestamps.
    Range {
        attribute: String,
        lower: Value,
        upper: Value,
    },
}

impl Predicate {
    pub fn eq(attribute: &str, value: Value) -> Self {
        Self::Equal {
            attribute: attribute.into(),
            value,
        }
    }

    pub fn contains(attribute: &str, member: &str) -> Self {
        Self::Contains {
            attribute: attribute.into(),
            member: member.into(),
        }
    }

    pub fn prefix(attribute: &str, prefix: &str) -> Self {
        Self::Prefix {
            attribute: attribute.into(),
            prefix: prefix.into(),
        }
    }

    pub fn range(attribute: &str, lower: Value, upper: Value) -> Self {
        Self::Range {
            attribute: attribute.into(),
            lower,
            upper,
        }
    }

    fn attribute(&self) -> &str {
        match self {
            Self::Equal { attribute, .. }
            | Self::Contains { attribute, .. }
            | Self::Prefix { attribute, .. }
            | Self::Range { attribute, .. } => attribute,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Posting {
    attribute_id: u32,
    value: Vec<u8>,
    file_id: FileId,
}

#[derive(Clone, Debug)]
struct State {
    generation: u64,
    records: BTreeMap<FileId, BTreeMap<u32, Value>>,
    postings: BTreeSet<Posting>,
}

impl State {
    fn empty() -> Self {
        Self {
            generation: 0,
            records: BTreeMap::new(),
            postings: BTreeSet::new(),
        }
    }

    fn add_postings(&mut self, schema: &Schema, file: FileId, attrs: &BTreeMap<u32, Value>) {
        for (id, value) in attrs {
            if let Some(def) = schema.by_id(*id) {
                for key in def.index_values(value) {
                    self.postings.insert(Posting {
                        attribute_id: *id,
                        value: key,
                        file_id: file,
                    });
                }
            }
        }
    }

    fn remove_postings(&mut self, schema: &Schema, file: FileId, attrs: &BTreeMap<u32, Value>) {
        for (id, value) in attrs {
            if let Some(def) = schema.by_id(*id) {
                for key in def.index_values(value) {
                    self.postings.remove(&Posting {
                        attribute_id: *id,
                        value: key,
                        file_id: file,
                    });
                }
            }
        }
    }
}

/// A cheap cloneable index handle. Its state is in-memory only.
#[derive(Clone)]
pub struct IndexStore {
    volume: u128,
    schema: Arc<Schema>,
    current: Arc<RwLock<Arc<State>>>,
}

impl IndexStore {
    pub fn new(volume: u128, schema: Schema) -> Result<Self, IndexError> {
        if volume == 0 {
            return Err(IndexError::InvalidFileId);
        }
        Ok(Self {
            volume,
            schema: Arc::new(schema),
            current: Arc::new(RwLock::new(Arc::new(State::empty()))),
        })
    }

    pub fn snapshot(&self) -> Result<Snapshot, IndexError> {
        let guard = self.current.read().map_err(|_| IndexError::LockPoisoned)?;
        Ok(Snapshot {
            volume: self.volume,
            schema: Arc::clone(&self.schema),
            state: Arc::clone(&guard),
        })
    }

    pub fn begin(&self) -> Result<Transaction, IndexError> {
        Ok(Transaction {
            owner: self.clone(),
            base_generation: self.snapshot()?.generation(),
            changes: BTreeMap::new(),
        })
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }
}

/// Optimistic whole-record transactions. A conflicting publisher must retry.
/// The entire index and all records are replaced under one write lock.
pub struct Transaction {
    owner: IndexStore,
    base_generation: u64,
    changes: BTreeMap<FileId, Option<BTreeMap<u32, Value>>>,
}

impl Transaction {
    fn validate_id(&self, id: FileId) -> Result<(), IndexError> {
        if id.volume != self.owner.volume {
            return Err(IndexError::WrongVolume);
        }
        if id.inode == 0 {
            return Err(IndexError::InvalidFileId);
        }
        Ok(())
    }

    /// Replace all attributes for a stable file ID. No namespace/path handling.
    pub fn upsert(
        &mut self,
        id: FileId,
        values: impl IntoIterator<Item = (String, Value)>,
    ) -> Result<(), IndexError> {
        self.validate_id(id)?;
        let mut attributes = BTreeMap::new();
        for (name, value) in values {
            let def = self.owner.schema.get(&name)?;
            let normalized = def.normalize(value)?;
            if attributes.insert(def.id, normalized).is_some() {
                return Err(IndexError::InvalidValue(format!(
                    "duplicate attribute in record: {name}"
                )));
            }
        }
        if attributes.len() > 128 {
            return Err(IndexError::InvalidValue(
                "record has more than 128 attributes".into(),
            ));
        }
        self.changes.insert(id, Some(attributes));
        Ok(())
    }

    pub fn delete(&mut self, id: FileId) -> Result<(), IndexError> {
        self.validate_id(id)?;
        self.changes.insert(id, None);
        Ok(())
    }

    pub fn commit(self) -> Result<u64, IndexError> {
        let mut guard = self
            .owner
            .current
            .write()
            .map_err(|_| IndexError::LockPoisoned)?;
        let published = guard.generation;
        if published != self.base_generation {
            return Err(IndexError::Conflict {
                expected: self.base_generation,
                actual: published,
            });
        }
        if self.changes.is_empty() {
            return Ok(published);
        }
        let next_generation = published
            .checked_add(1)
            .ok_or(IndexError::GenerationOverflow)?;
        let mut next = (**guard).clone();
        for (file, change) in self.changes {
            if let Some(existing) = next.records.remove(&file) {
                next.remove_postings(&self.owner.schema, file, &existing);
            }
            if let Some(attributes) = change {
                next.add_postings(&self.owner.schema, file, &attributes);
                next.records.insert(file, attributes);
            }
        }
        next.generation = next_generation;
        *guard = Arc::new(next);
        Ok(next_generation)
    }
}

#[derive(Clone)]
pub struct Snapshot {
    volume: u128,
    schema: Arc<Schema>,
    state: Arc<State>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryResult {
    pub generation: u64,
    pub files: Vec<FileId>,
    pub postings_examined: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
enum Condition {
    Equal {
        value: Value,
        key: Vec<u8>,
    },
    Contains {
        value: String,
        key: Vec<u8>,
    },
    Prefix {
        value: String,
        key_prefix: Vec<u8>,
    },
    Range {
        lower: Value,
        upper: Value,
        lower_key: Vec<u8>,
        upper_key: Vec<u8>,
    },
}

#[derive(Clone, Debug)]
struct Prepared {
    attribute_id: u32,
    condition: Condition,
}

impl Prepared {
    fn matches(&self, attrs: &BTreeMap<u32, Value>) -> bool {
        let Some(current) = attrs.get(&self.attribute_id) else {
            return false;
        };
        match &self.condition {
            Condition::Equal { value, .. } => current == value,
            Condition::Contains { value, .. } => match current {
                Value::TextSet(values) => values.contains(value),
                _ => false,
            },
            Condition::Prefix { value, .. } => match current {
                Value::Text(text) => text.starts_with(value),
                _ => false,
            },
            Condition::Range { lower, upper, .. } => current >= lower && current < upper,
        }
    }
}

impl Snapshot {
    pub fn generation(&self) -> u64 {
        self.state.generation
    }

    pub fn file_count(&self) -> usize {
        self.state.records.len()
    }

    pub fn index_postings(&self) -> usize {
        self.state.postings.len()
    }

    pub fn value(&self, file: FileId, attribute: &str) -> Result<Option<&Value>, IndexError> {
        if file.volume != self.volume {
            return Err(IndexError::WrongVolume);
        }
        let def = self.schema.get(attribute)?;
        Ok(self.state.records.get(&file).and_then(|r| r.get(&def.id)))
    }

    fn prepare(&self, predicate: &Predicate) -> Result<Prepared, IndexError> {
        let def = self.schema.get(predicate.attribute())?;
        if !def.indexed {
            return Err(IndexError::NotIndexed(def.name.clone()));
        }
        let condition = match predicate {
            Predicate::Equal { value, .. } => {
                if matches!(&def.ty, AttributeType::TextSet { .. }) {
                    return Err(IndexError::InvalidPredicate(
                        "use Contains for multivalue attributes".into(),
                    ));
                }
                let normalized = def.normalize(value.clone())?;
                let key = scalar_key(&normalized);
                Condition::Equal {
                    value: normalized,
                    key,
                }
            }
            Predicate::Contains { member, .. } => {
                if !matches!(&def.ty, AttributeType::TextSet { .. }) {
                    return Err(IndexError::InvalidPredicate(
                        "Contains requires a text-set attribute".into(),
                    ));
                }
                let value = def.canonical_text(member)?;
                let mut key = vec![0x70];
                key.extend_from_slice(value.as_bytes());
                Condition::Contains { value, key }
            }
            Predicate::Prefix { prefix, .. } => {
                if !matches!(&def.ty, AttributeType::Text { .. }) || prefix.is_empty() {
                    return Err(IndexError::InvalidPredicate(
                        "Prefix requires a nonempty single-valued text attribute".into(),
                    ));
                }
                let value = def.canonical_text(prefix)?;
                let mut key_prefix = vec![0x50];
                key_prefix.extend_from_slice(value.as_bytes());
                Condition::Prefix { value, key_prefix }
            }
            Predicate::Range { lower, upper, .. } => {
                if !matches!(
                    def.ty,
                    AttributeType::Signed | AttributeType::Unsigned | AttributeType::TimestampNs
                ) {
                    return Err(IndexError::InvalidPredicate(
                        "Range requires signed, unsigned or timestamp attribute".into(),
                    ));
                }
                let lower = def.normalize(lower.clone())?;
                let upper = def.normalize(upper.clone())?;
                let lower_key = scalar_key(&lower);
                let upper_key = scalar_key(&upper);
                if lower_key >= upper_key {
                    return Err(IndexError::InvalidPredicate(
                        "range must have lower < upper".into(),
                    ));
                }
                Condition::Range {
                    lower,
                    upper,
                    lower_key,
                    upper_key,
                }
            }
        };
        Ok(Prepared {
            attribute_id: def.id,
            condition,
        })
    }

    fn validate_query(
        &self,
        predicates: &[Predicate],
        limit: usize,
    ) -> Result<Vec<Prepared>, IndexError> {
        if limit == 0 || limit > MAX_QUERY_LIMIT {
            return Err(IndexError::InvalidLimit);
        }
        if predicates.is_empty() || predicates.len() > 16 {
            return Err(IndexError::InvalidPredicate(
                "require 1..=16 indexed predicates".into(),
            ));
        }
        predicates.iter().map(|p| self.prepare(p)).collect()
    }

    fn posting_candidates(&self, predicate: &Prepared) -> (BTreeSet<FileId>, usize) {
        let (start_key, kind) = match &predicate.condition {
            Condition::Equal { key, .. } => (key, 0),
            Condition::Contains { key, .. } => (key, 0),
            Condition::Prefix { key_prefix, .. } => (key_prefix, 1),
            Condition::Range { lower_key, .. } => (lower_key, 2),
        };
        let start = Posting {
            attribute_id: predicate.attribute_id,
            value: start_key.clone(),
            file_id: FileId {
                volume: 0,
                inode: 0,
            },
        };
        let mut files = BTreeSet::new();
        let mut examined = 0;
        for posting in self.state.postings.range(start..) {
            if posting.attribute_id != predicate.attribute_id {
                break;
            }
            let matches = match (&predicate.condition, kind) {
                (Condition::Equal { key, .. }, 0) | (Condition::Contains { key, .. }, 0) => {
                    posting.value.as_slice() == key.as_slice()
                }
                (Condition::Prefix { key_prefix, .. }, 1) => posting.value.starts_with(key_prefix),
                (Condition::Range { upper_key, .. }, 2) => {
                    posting.value.as_slice() < upper_key.as_slice()
                }
                _ => unreachable!("prepared condition code"),
            };
            if !matches {
                break;
            }
            examined += 1;
            files.insert(posting.file_id);
        }
        (files, examined)
    }

    pub fn indexed_query(
        &self,
        predicates: &[Predicate],
        limit: usize,
    ) -> Result<QueryResult, IndexError> {
        let prepared = self.validate_query(predicates, limit)?;
        let mut posting_sets = Vec::with_capacity(prepared.len());
        let mut examined = 0;
        for predicate in &prepared {
            let (files, rows) = self.posting_candidates(predicate);
            posting_sets.push(files);
            examined += rows;
        }
        posting_sets.sort_by_key(BTreeSet::len);
        let mut result = posting_sets.remove(0);
        for other in posting_sets {
            result.retain(|id| other.contains(id));
            if result.is_empty() {
                break;
            }
        }
        let truncated = result.len() > limit;
        Ok(QueryResult {
            generation: self.state.generation,
            files: result.into_iter().take(limit).collect(),
            postings_examined: examined,
            truncated,
        })
    }

    /// Independent reference oracle scans file records, never the posting tree.
    /// Query normalization is shared, matching the public type contract.
    pub fn full_scan_oracle(
        &self,
        predicates: &[Predicate],
        limit: usize,
    ) -> Result<QueryResult, IndexError> {
        let prepared = self.validate_query(predicates, limit)?;
        let mut files = Vec::new();
        for (file, attributes) in &self.state.records {
            if prepared.iter().all(|p| p.matches(attributes)) {
                files.push(*file);
            }
        }
        let truncated = files.len() > limit;
        files.truncate(limit);
        Ok(QueryResult {
            generation: self.state.generation,
            files,
            postings_examined: self.state.records.len(),
            truncated,
        })
    }

    /// Rebuild a reference index from all records and compare exact postings.
    pub fn index_is_consistent(&self) -> bool {
        let mut rebuilt = State::empty();
        for (id, attrs) in &self.state.records {
            rebuilt.add_postings(&self.schema, *id, attrs);
        }
        rebuilt.postings == self.state.postings
    }
}
