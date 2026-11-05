//! RFQ over Nostr. Specification: docs/nostr-rfq.md.
//!
//! No message here can move funds. A forged, replayed or withheld message can at worst waste time, because both sides
//! derive every address themselves and check every lock on-chain.
//!
//! - [`messages`]: the wire format, parsed strictly.
//! - [`rules`]: what each side must check, as pure functions.
//! - [`wrap`]: NIP-59 sealing and gift wrapping.
//! - [`relay`]: a small client for talking to relays.
//! - [`taker`]: the taker's side of a negotiation.
//! - [`mock_relay`]: a local relay for tests, which can also misbehave.

#![forbid(unsafe_code)]

pub mod messages;
pub mod mock_relay;
pub mod relay;
pub mod rules;
pub mod taker;
pub mod wrap;
