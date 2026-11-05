//! The maker's decisions, kept free of chain and network code. Specification: docs/maker.md, docs/protocol.md,
//! docs/confidential.md.
//!
//! - [`engine`]: what to do next for one swap, given what both chains show.
//! - [`policy`]: the signer's own check of every request, which trusts nothing from the engine.
//! - [`store`]: durable swap state and write-ahead intents.

#![forbid(unsafe_code)]

pub mod engine;
pub mod policy;
pub mod store;

use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};

/// A swap's lock point T, compressed. Whoever knows the discrete log can claim on Solana, and doing so reveals it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockPoint(pub [u8; 33]);

impl LockPoint {
    /// The swap's id: the point's x coordinate. A used secret is public, so the id is never accepted twice.
    pub fn id(&self) -> [u8; 32] {
        let mut id = [0u8; 32];
        id.copy_from_slice(&self.0[1..]);
        id
    }

    /// Whether this is the secret behind the point. A zero or out-of-range value isn't.
    pub fn opened_by(&self, secret: &[u8; 32]) -> bool {
        liq2sol_swapkey::point_of(secret).is_ok_and(|point| point == self.0)
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(text: &str) -> Option<Self> {
        if text.len() != 66 || !text.is_ascii() {
            return None;
        }
        let mut out = [0u8; 33];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Self(out))
    }
}

impl Serialize for LockPoint {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for LockPoint {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Self::from_hex(&text).ok_or_else(|| D::Error::custom("bad lock point"))
    }
}
