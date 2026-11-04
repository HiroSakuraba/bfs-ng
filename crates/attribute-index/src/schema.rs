//! Versioned, strictly typed attribute definitions and sortable encodings.
//! This is an in-memory reference codec, NOT an on-disk wire format.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub const SCHEMA_VERSION: u16 = 1;
pub const MAX_ATTRIBUTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexError {
    InvalidSchema(String),
    DuplicateName(String),
    DuplicateId(u32),
    UnknownAttribute(String),
    InvalidValue(String),
    InvalidPredicate(String),
    NotIndexed(String),
    InvalidFileId,
    WrongVolume,
    InvalidLimit,
    Conflict { expected: u64, actual: u64 },
    GenerationOverflow,
    LockPoisoned,
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSchema(x) => write!(f, "invalid schema: {x}"),
            Self::DuplicateName(x) => write!(f, "duplicate attribute name: {x}"),
            Self::DuplicateId(id) => write!(f, "duplicate attribute ID: {id}"),
            Self::UnknownAttribute(x) => write!(f, "unknown attribute: {x}"),
            Self::InvalidValue(x) => write!(f, "invalid attribute value: {x}"),
            Self::InvalidPredicate(x) => write!(f, "invalid query predicate: {x}"),
            Self::NotIndexed(x) => write!(f, "attribute has no index: {x}"),
            Self::InvalidFileId => write!(f, "file ID must be nonzero"),
            Self::WrongVolume => write!(f, "file ID belongs to another volume"),
            Self::InvalidLimit => write!(f, "query limit must be 1..=10000"),
            Self::Conflict { expected, actual } => {
                write!(
                    f,
                    "stale transaction: expected generation {expected}, got {actual}"
                )
            }
            Self::GenerationOverflow => write!(f, "generation counter overflow"),
            Self::LockPoisoned => write!(f, "metadata index lock poisoned"),
        }
    }
}
impl std::error::Error for IndexError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextCase {
    /// UTF-8 bytes are preserved exactly. No Unicode normalization is implied.
    ExactUtf8,
    /// ASCII-only lowercasing; non-ASCII input is rejected.
    AsciiFold,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttributeType {
    Bool,
    Signed,
    Unsigned,
    TimestampNs,
    Text {
        case: TextCase,
        max_bytes: usize,
    },
    Hash256,
    TextSet {
        case: TextCase,
        max_bytes: usize,
        max_items: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Value {
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    TimestampNs(i64),
    Text(String),
    Hash256([u8; 32]),
    TextSet(BTreeSet<String>),
}

#[derive(Clone, Debug)]
pub struct AttributeDef {
    pub id: u32,
    pub name: String,
    pub ty: AttributeType,
    /// Only indexed attributes may appear in public indexed queries.
    pub indexed: bool,
}

#[derive(Clone, Debug)]
pub struct Schema {
    by_name: BTreeMap<String, AttributeDef>,
    by_id: BTreeMap<u32, String>,
}

impl Schema {
    pub fn new(defs: impl IntoIterator<Item = AttributeDef>) -> Result<Self, IndexError> {
        let mut by_name = BTreeMap::new();
        let mut by_id = BTreeMap::new();
        for def in defs {
            if by_name.len() >= MAX_ATTRIBUTES {
                return Err(IndexError::InvalidSchema("too many attributes".into()));
            }
            if def.id == 0 || !valid_name(&def.name) {
                return Err(IndexError::InvalidSchema(format!(
                    "ID must be nonzero and attribute name must be ASCII dotted: {}",
                    def.name
                )));
            }
            match &def.ty {
                AttributeType::Text { max_bytes, .. }
                | AttributeType::TextSet { max_bytes, .. } => {
                    if *max_bytes == 0 || *max_bytes > 1024 {
                        return Err(IndexError::InvalidSchema(format!(
                            "text length must be 1..=1024 for {}",
                            def.name
                        )));
                    }
                }
                _ => {}
            }
            if let AttributeType::TextSet { max_items, .. } = &def.ty {
                if *max_items == 0 || *max_items > 128 {
                    return Err(IndexError::InvalidSchema(
                        "set must allow 1..=128 elements".into(),
                    ));
                }
            }
            if by_name.contains_key(&def.name) {
                return Err(IndexError::DuplicateName(def.name));
            }
            if by_id.contains_key(&def.id) {
                return Err(IndexError::DuplicateId(def.id));
            }
            by_id.insert(def.id, def.name.clone());
            by_name.insert(def.name.clone(), def);
        }
        if by_name.is_empty() {
            return Err(IndexError::InvalidSchema("empty schema".into()));
        }
        Ok(Self { by_name, by_id })
    }

    pub fn get(&self, name: &str) -> Result<&AttributeDef, IndexError> {
        self.by_name
            .get(name)
            .ok_or_else(|| IndexError::UnknownAttribute(name.into()))
    }

    pub fn by_id(&self, id: u32) -> Option<&AttributeDef> {
        self.by_id.get(&id).and_then(|name| self.by_name.get(name))
    }

    pub fn definitions(&self) -> impl Iterator<Item = &AttributeDef> {
        self.by_name.values()
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        })
}

fn normalize_text(value: &str, case: TextCase, max_bytes: usize) -> Result<String, IndexError> {
    if value.len() > max_bytes || value.contains('\0') {
        return Err(IndexError::InvalidValue(
            "text length or embedded NUL".into(),
        ));
    }
    match case {
        TextCase::ExactUtf8 => Ok(value.into()),
        TextCase::AsciiFold if value.is_ascii() => Ok(value.to_ascii_lowercase()),
        TextCase::AsciiFold => Err(IndexError::InvalidValue(
            "ASCII-folded attribute cannot contain non-ASCII text".into(),
        )),
    }
}

impl AttributeDef {
    /// Validate and canonicalize an attribute value before storing it.
    pub fn normalize(&self, value: Value) -> Result<Value, IndexError> {
        match (&self.ty, value) {
            (AttributeType::Bool, Value::Bool(v)) => Ok(Value::Bool(v)),
            (AttributeType::Signed, Value::Signed(v)) => Ok(Value::Signed(v)),
            (AttributeType::Unsigned, Value::Unsigned(v)) => Ok(Value::Unsigned(v)),
            (AttributeType::TimestampNs, Value::TimestampNs(v)) => Ok(Value::TimestampNs(v)),
            (AttributeType::Hash256, Value::Hash256(v)) => Ok(Value::Hash256(v)),
            (AttributeType::Text { case, max_bytes }, Value::Text(v)) => {
                Ok(Value::Text(normalize_text(&v, *case, *max_bytes)?))
            }
            (
                AttributeType::TextSet {
                    case,
                    max_bytes,
                    max_items,
                },
                Value::TextSet(items),
            ) => {
                if items.len() > *max_items {
                    return Err(IndexError::InvalidValue(format!(
                        "{} has too many set members",
                        self.name
                    )));
                }
                let normalized: BTreeSet<_> = items
                    .iter()
                    .map(|v| normalize_text(v, *case, *max_bytes))
                    .collect::<Result<_, _>>()?;
                Ok(Value::TextSet(normalized))
            }
            _ => Err(IndexError::InvalidValue(format!(
                "type mismatch for {}",
                self.name
            ))),
        }
    }

    /// Canonical tagged, byte-sortable posting keys used by the F4
    /// persistent attribute catalog. These are **not** a stable disk-format
    /// contract outside the versioned F4 catalog container.
    pub fn index_keys(&self, value: &Value) -> Vec<Vec<u8>> {
        self.index_values(value)
    }

    pub(crate) fn index_values(&self, value: &Value) -> Vec<Vec<u8>> {
        if !self.indexed {
            return Vec::new();
        }
        match value {
            Value::TextSet(members) => members
                .iter()
                .map(|s| {
                    let mut bytes = vec![0x70];
                    bytes.extend_from_slice(s.as_bytes());
                    bytes
                })
                .collect(),
            other => vec![scalar_key(other)],
        }
    }

    pub(crate) fn canonical_text(&self, text: &str) -> Result<String, IndexError> {
        match &self.ty {
            AttributeType::Text { case, max_bytes }
            | AttributeType::TextSet {
                case, max_bytes, ..
            } => normalize_text(text, *case, *max_bytes),
            _ => Err(IndexError::InvalidPredicate(format!(
                "{} is not a text attribute",
                self.name
            ))),
        }
    }
}

/// Sort-preserving scalar byte keys. Kept internal: not yet a disk format.
pub(crate) fn scalar_key(value: &Value) -> Vec<u8> {
    match value {
        Value::Bool(v) => vec![0x10, u8::from(*v)],
        Value::Signed(v) => {
            let mut b = vec![0x20];
            b.extend_from_slice(&((*v as u64) ^ (1u64 << 63)).to_be_bytes());
            b
        }
        Value::Unsigned(v) => {
            let mut b = vec![0x30];
            b.extend_from_slice(&v.to_be_bytes());
            b
        }
        Value::TimestampNs(v) => {
            let mut b = vec![0x40];
            b.extend_from_slice(&((*v as u64) ^ (1u64 << 63)).to_be_bytes());
            b
        }
        Value::Text(v) => {
            let mut b = vec![0x50];
            b.extend_from_slice(v.as_bytes());
            b
        }
        Value::Hash256(v) => {
            let mut b = vec![0x60];
            b.extend_from_slice(v);
            b
        }
        Value::TextSet(_) => unreachable!("set members are indexed independently"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numeric_sorting_and_hash_encoding() {
        let signed = [-100, -1, 0, 1, 100];
        for pair in signed.windows(2) {
            assert!(scalar_key(&Value::Signed(pair[0])) < scalar_key(&Value::Signed(pair[1])));
        }
        let times = [i64::MIN, -1, 0, i64::MAX];
        for pair in times.windows(2) {
            assert!(
                scalar_key(&Value::TimestampNs(pair[0])) < scalar_key(&Value::TimestampNs(pair[1]))
            );
        }
        assert_eq!(scalar_key(&Value::Hash256([7; 32])).len(), 33);
    }
}
