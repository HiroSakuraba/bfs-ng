//! Experimental F0 block-image durability model. Not a mounted filesystem.
#![forbid(unsafe_code)]

mod device;
mod volume;

pub use device::{BlockDevice, CrashDevice, Fault, FileDevice};
pub use volume::{RootInfo, Volume};

pub const BLOCK_SIZE: usize = 4096;
pub const ROOT_CAPACITY: usize = BLOCK_SIZE - 16;

use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Geometry,
    OutOfBounds(u64),
    NotBlank,
    NoValidRoot,
    CorruptRoot,
    InconsistentVolume,
    NoSpace,
    TooLarge,
    GenerationOverflow,
    Poisoned,
    InjectedCrash,
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O: {e}"),
            Self::Geometry => write!(f, "invalid image/block geometry"),
            Self::OutOfBounds(n) => write!(f, "block {n} is out of bounds"),
            Self::NotBlank => write!(f, "root slots are not blank: refusing format"),
            Self::NoValidRoot => write!(f, "no recoverable checksummed root"),
            Self::CorruptRoot => write!(f, "root payload corrupted"),
            Self::InconsistentVolume => write!(f, "root slots disagree on image identity"),
            Self::NoSpace => write!(f, "append-only root area is full"),
            Self::TooLarge => write!(f, "root payload exceeds fixed block capacity"),
            Self::GenerationOverflow => write!(f, "root generation overflow"),
            Self::Poisoned => write!(f, "prior failed commit; reopen device to recover"),
            Self::InjectedCrash => write!(f, "injected power failure"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
