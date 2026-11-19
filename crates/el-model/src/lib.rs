//! Core data types, binary codec and merkle helpers for e-lite.

pub mod codec;
pub mod hash;
pub mod logging;
pub mod merkle;
pub mod sync;
pub mod types;

pub use codec::{CodecError, SCHEMA_VERSION};
pub use hash::sha256d;
pub use merkle::{merkle_branch, merkle_root, merkle_root_from_branch, MerkleTree};
pub use types::*;
