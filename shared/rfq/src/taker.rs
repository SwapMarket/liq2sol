//! The taker's side of a negotiation, over one relay. Each answer is checked before the next step, and nothing here
//! ever touches funds: the caller derives the HTLC itself, and funds only after this returns.

use std::time::{Duration, Instant};

use nostr::prelude::*;

use crate::messages::*;
use crate::relay::{Relay, RelayEvent};
use crate::rules::*;
use crate::wrap::{self, Received};

pub struct Negotiated {
    pub quote: Quote,
    pub terms: MakerTerms,
}

/// What the taker commits to in its accept.
pub struct TakerSide {
    pub payment_hash: [u8; 32],
    pub taker_liquid_pk: [u8; 32],
    pub taker_solana: String,
    pub taker_liquid_script: Option<Vec<u8>>,
    pub taker_fee_script: Option<Vec<u8>>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn random_id() -> String {
    hex(&Keys::generate().secret_key().to_secret_bytes())
}

/// Find live offers. Only a signature and the network are checked here: which makers to trust is the caller's choice.
pub fn find_offers(relay: &mut Relay, network: &str, wait: Duration) -> Result<Vec<(PublicKey, Offer)>, String> {
    relay.subscribe("liq2sol-offers", &[Kind::from(OFFER_KIND)], None, now_secs().saturating_sub(3_600))?;
    let start = Instant::now();
    let mut offers: Vec<(PublicKey, Offer)> = vec![];
    while start.elapsed() < wait {
        match relay.next_event()? {
            Some(RelayEvent::Event(e)) => {
                if let Some(offer) = wrap::read_offer(&e, network, now_secs()) {
                    offers.retain(|(pk, _)| *pk != e.pubkey);
                    offers.push((e.pubkey, offer));
                }
            }
            Some(RelayEvent::EndOfStored) if !offers.is_empty() => break,
            _ => {}
        }
    }
    Ok(offers)
}

/// Wait for a message of this negotiation from exactly this maker. Anything else is ignored, not an error, because
/// anyone can send us anything.
fn wait_from(
    relay: &mut Relay,
    me: &Keys,
    maker: &PublicKey,
    id: &str,
    network: &str,
    limit: Duration,
) -> Result<Received, String> {
    let start = Instant::now();
    while start.elapsed() < limit {
        if let Some(RelayEvent::Event(e)) = relay.next_event()? {
            let Ok(got) = wrap::unwrap(me, &e) else { continue };
            if got.sender != *maker
                || got.envelope.id != id
                || check_envelope(&got.envelope, network, now_secs()).is_err()
            {
                continue;
            }
            if let Body::RfqReject(r) = &got.envelope.body {
                return Err(format!("the maker declined: {}", r.reason.chars().take(200).collect::<String>()));
            }
            return Ok(got);
        }
    }
    Err("the maker did not answer in time".into())
}

/// Run one negotiation. `tip` reads the taker's own view of the Liquid tip, used to judge the terms.
#[allow(clippy::too_many_arguments)]
pub fn negotiate(
    relay: &mut Relay,
    me: &Keys,
    maker: &PublicKey,
    rules: &TakerRules,
    request: Request,
    side: TakerSide,
    tip: impl Fn() -> Result<u32, String>,
    limit: Duration,
) -> Result<Negotiated, String> {
    let id = random_id();
    let direction = request.direction;
    let envelope =
        |body| Envelope { v: VERSION, id: id.clone(), network: rules.network.clone(), sent_at: now_secs(), body };
    relay.subscribe(
        &id[..16],
        &[Kind::from(WRAP_KIND)],
        Some(&me.public_key()),
        now_secs().saturating_sub(2 * 86_400),
    )?;

    let (event, _) = wrap::wrap(me, maker, &envelope(Body::RfqRequest(request.clone())), now_secs())
        .map_err(|e| format!("{e:?}"))?;
    relay.publish(&event)?;

    let got = wait_from(relay, me, maker, &id, &rules.network, limit)?;
    let Body::RfqQuote(quote) = got.envelope.body else {
        return Err("expected a quote".into());
    };
    check_quote(rules, &request, &quote, now_secs()).map_err(|e| format!("bad quote: {e:?}"))?;

    let accept = Accept {
        quote_event: got.rumor_id,
        payment_hash: hex(&side.payment_hash),
        taker_liquid_pk: hex(&side.taker_liquid_pk),
        taker_solana: side.taker_solana,
        taker_liquid_script: side.taker_liquid_script.as_deref().map(hex),
        taker_fee_script: side.taker_fee_script.as_deref().map(hex),
    };
    let (event, accept_id) =
        wrap::wrap(me, maker, &envelope(Body::RfqAccept(accept)), now_secs()).map_err(|e| format!("{e:?}"))?;
    relay.publish(&event)?;

    let got = wait_from(relay, me, maker, &id, &rules.network, limit)?;
    let Body::RfqTerms(terms) = got.envelope.body else {
        return Err("expected terms".into());
    };
    check_terms(rules, direction, &quote, &accept_id, &terms, tip()?).map_err(|e| format!("bad terms: {e:?}"))?;
    Ok(Negotiated { quote, terms })
}
