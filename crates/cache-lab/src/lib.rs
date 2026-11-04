//! BFS-NG cache laboratory: **offline trace replay**, not a production block cache.
//! No actual file contents, device operations, or commit logic live in this crate.
//! All policies share exactly the same access stream and charged-byte capacity.

pub mod policies;
pub mod simulator;
pub mod trace;
