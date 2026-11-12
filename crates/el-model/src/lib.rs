//! Core data types, binary codec and merkle helpers for e-lite.

pub mod codec;
pub mod merkle;
pub mod types;

pub use codec::{CodecError, SCHEMA_VERSION};
pub use merkle::{merkle_branch, merkle_root, merkle_root_from_branch};
pub use types::*;
