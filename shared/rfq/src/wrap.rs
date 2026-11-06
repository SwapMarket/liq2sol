//! NIP-59: an unsigned rumor, sealed and signed by the sender, then wrapped by a throwaway key. Relays see only the
//! recipient and timing. The kind 21059 wrapper is built here from NIP-44 and event signing, following the NIP's layout.

use nostr::nips::nip44::{self, Version};
use nostr::prelude::*;

use crate::messages::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WrapError {
    TooLarge,
    NotAWrap,
    BadEvent,
    CannotDecrypt,
    BadSeal,
    /// The rumor claims a different author from the key that signed the seal: a forgery.
    Spoofed,
    BadRumor,
    Malformed,
}

/// A message that has passed every cryptographic check. `sender` is authenticated by the seal's signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub sender: PublicKey,
    pub rumor_id: String,
    pub envelope: Envelope,
}

/// NIP-59 asks for randomized timestamps up to two days back, so a relay can't correlate by time.
fn tweaked(now: u64, random: u8) -> Timestamp {
    Timestamp::from(now.saturating_sub(u64::from(random) * 600))
}

fn random_byte() -> u8 {
    Keys::generate().secret_key().as_secret_bytes()[0]
}

/// Seal and wrap one message for `to`. Returns the event to publish, and the rumor's id for later references.
pub fn wrap(sender: &Keys, to: &PublicKey, envelope: &Envelope, now: u64) -> Result<(Event, String), WrapError> {
    let content = serde_json::to_string(envelope).map_err(|_| WrapError::Malformed)?;
    let mut rumor = UnsignedEvent::new(
        sender.public_key(),
        Timestamp::from(now),
        Kind::from(RUMOR_KIND),
        Vec::<Tag>::new(),
        content,
    );
    rumor.ensure_id();
    let rumor_id = rumor.id.ok_or(WrapError::BadRumor)?.to_hex();
    let sealed =
        nip44::encrypt(sender.secret_key(), to, rumor.as_json(), Version::V2).map_err(|_| WrapError::CannotDecrypt)?;
    let seal = EventBuilder::new(Kind::from(SEAL_KIND), sealed)
        .custom_created_at(tweaked(now, random_byte()))
        .finalize(sender)
        .map_err(|_| WrapError::BadSeal)?;
    let throwaway = Keys::generate();
    let wrapped = nip44::encrypt(throwaway.secret_key(), to, seal.as_json(), Version::V2)
        .map_err(|_| WrapError::CannotDecrypt)?;
    // The outer timestamp is the real time. Relays drop ephemeral events more than a few seconds old, and they see when
    // this arrives anyway, so randomizing it would hide nothing. The seal inside keeps a randomized one.
    let event = EventBuilder::new(Kind::from(WRAP_KIND), wrapped)
        .tag(Tag::public_key(*to))
        .custom_created_at(Timestamp::from(now))
        .finalize(&throwaway)
        .map_err(|_| WrapError::BadEvent)?;
    Ok((event, rumor_id))
}

/// Open a wrapped message addressed to `me`, checking every layer.
pub fn unwrap(me: &Keys, event: &Event) -> Result<Received, WrapError> {
    if event.kind != Kind::from(WRAP_KIND) {
        return Err(WrapError::NotAWrap);
    }
    if event.as_json().len() > MAX_EVENT_BYTES {
        return Err(WrapError::TooLarge);
    }
    event.verify().map_err(|_| WrapError::BadEvent)?;
    let seal_json =
        nip44::decrypt(me.secret_key(), &event.pubkey, &event.content).map_err(|_| WrapError::CannotDecrypt)?;
    let seal = Event::from_json(&seal_json).map_err(|_| WrapError::BadSeal)?;
    if seal.kind != Kind::from(SEAL_KIND) || !seal.tags.is_empty() {
        return Err(WrapError::BadSeal);
    }
    seal.verify().map_err(|_| WrapError::BadSeal)?;
    let rumor_json =
        nip44::decrypt(me.secret_key(), &seal.pubkey, &seal.content).map_err(|_| WrapError::CannotDecrypt)?;
    let mut rumor = UnsignedEvent::from_json(&rumor_json).map_err(|_| WrapError::BadRumor)?;
    // The check that makes the sender real: only the seal is signed, so the rumor's claim must match its signer.
    if rumor.pubkey != seal.pubkey {
        return Err(WrapError::Spoofed);
    }
    if rumor.kind != Kind::from(RUMOR_KIND) {
        return Err(WrapError::BadRumor);
    }
    // Recompute the id, so a sender can't claim another message's identity.
    rumor.id = None;
    rumor.ensure_id();
    let rumor_id = rumor.id.ok_or(WrapError::BadRumor)?.to_hex();
    let envelope: Envelope = serde_json::from_str(&rumor.content).map_err(|_| WrapError::Malformed)?;
    Ok(Received { sender: seal.pubkey, rumor_id, envelope })
}

/// A maker's public offer, addressable and expiring so a dead maker disappears from clients.
pub fn offer_event(maker: &Keys, offer: &Offer, now: u64, ttl_secs: u64) -> Result<Event, WrapError> {
    let content = serde_json::to_string(offer).map_err(|_| WrapError::Malformed)?;
    EventBuilder::new(Kind::from(OFFER_KIND), content)
        .tags([
            Tag::identifier(format!("liq2sol-v{VERSION}-{}", offer.network)),
            Tag::expiration(Timestamp::from(now + ttl_secs)),
            Tag::hashtag(TOPIC),
        ])
        .custom_created_at(Timestamp::from(now))
        .finalize(maker)
        .map_err(|_| WrapError::BadEvent)
}

/// Read an offer. A wrong-network offer, or one past its expiry, is treated as absent.
pub fn read_offer(event: &Event, network: &str, now: u64) -> Option<Offer> {
    if event.kind != Kind::from(OFFER_KIND) || event.verify().is_err() || event.as_json().len() > MAX_EVENT_BYTES {
        return None;
    }
    let expiry = event.tags.expiration().map(|t| t.as_secs())?;
    if expiry < now {
        return None;
    }
    let offer: Offer = serde_json::from_str(&event.content).ok()?;
    let usable = crate::rules::prices_ok(&offer.l2s_prices, offer.max_sol_units.0)
        && crate::rules::prices_ok(&offer.s2l_prices, offer.max_sol_units.0);
    (offer.v == VERSION && offer.network == network && usable).then_some(offer)
}
