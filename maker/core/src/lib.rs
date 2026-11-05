//! The maker's decisions, kept free of chain and network code. Specification: docs/maker.md, docs/protocol.md.
//!
//! - [`engine`]: what to do next for one swap, given what both chains show.
//! - [`policy`]: the signer's own check of every request, which trusts nothing from the engine.
//! - [`store`]: durable swap state and write-ahead intents.

#![forbid(unsafe_code)]

pub mod engine;
pub mod policy;
pub mod store;

use sha2::{Digest, Sha256};

/// SHA-256 of an exact 32-byte preimage, as both contracts compute it.
pub fn hash_preimage(preimage: &[u8; 32]) -> [u8; 32] {
    Sha256::digest(preimage).into()
}
