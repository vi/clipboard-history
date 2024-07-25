use std::borrow::Cow;

pub use ring_reader::{DatabaseReader, Entry, EntryReader, Kind, LoadedEntry, RingReader};
pub use ringboard_core as core;
use ringboard_core::{protocol, protocol::IdNotFoundError};
pub use search::search;
use thiserror::Error;

pub mod api;
pub mod duplicate_detection;
mod ring_reader;
pub mod search;
pub mod ui_actor;

#[derive(Error, Debug)]
pub enum ClientError {
    #[error("{0}")]
    Core(#[from] ringboard_core::Error),
    #[error(
        "Protocol version mismatch: expected {} but got {actual}.",
        protocol::VERSION
    )]
    VersionMismatch { actual: u8 },
    #[error("The server returned an invalid response.")]
    InvalidResponse { context: Cow<'static, str> },
}

impl From<IdNotFoundError> for ClientError {
    fn from(value: IdNotFoundError) -> Self {
        Self::Core(ringboard_core::Error::IdNotFound(value))
    }
}
