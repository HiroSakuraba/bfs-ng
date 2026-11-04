//! M0: typed, queryable file attributes with atomic in-memory snapshots.
//!
//! This crate has no filesystem persistence, crash guarantees or ACL model.
//! Query results must never be exposed to users without authorization checks.
#![forbid(unsafe_code)]

mod index;
mod schema;

pub use index::{
    FileId, IndexStore, Predicate, QueryResult, Snapshot, Transaction,
};
pub use schema::{
    AttributeDef, AttributeType, IndexError, Schema, TextCase, Value, SCHEMA_VERSION,
};
