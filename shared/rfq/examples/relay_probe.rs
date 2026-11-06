//! Check one public relay with throwaway keys and test-only messages: does it accept and deliver what RFQ needs?
//!   cargo run --example relay_probe -- wss://relay.example

use std::time::{Duration, Instant};

use liq2sol_rfq::messages::*;
use liq2sol_rfq::relay::{Relay, RelayEvent};
use liq2sol_rfq::wrap;
use nostr::prelude::*;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

/// Publish on one connection, and see whether a second connection receives it.
fn probe(
    url: &str,
    name: &str,
    event: &Event,
    sub_kind: u16,
    to: Option<&PublicKey>,
    since: u64,
) -> Result<(), String> {
    let mut reader = Relay::connect(url, Duration::from_millis(300))?;
    reader.subscribe("probe", &[Kind::from(sub_kind)], to, since)?;
    // Let the subscription settle, and see what the relay has stored.
    let settle = Instant::now();
    while settle.elapsed() < Duration::from_secs(5) {
        if let Some(RelayEvent::EndOfStored) = reader.next_event()? {
            break;
        }
    }
    let mut writer = Relay::connect(url, Duration::from_millis(300))?;
    let sent = Instant::now();
    writer.publish(event)?;
    let id = event.id.to_hex();
    let mut accepted = None;
    let mut delivered = None;
    while sent.elapsed() < Duration::from_secs(12) && (accepted.is_none() || delivered.is_none()) {
        match writer.next_event() {
            Ok(Some(RelayEvent::Ok(ok_id, ok, message))) if ok_id == id => accepted = Some((ok, message)),
            Ok(Some(RelayEvent::Notice(n))) => println!("  notice: {n}"),
            Ok(_) => {}
            Err(e) => println!("  writer: {e}"),
        }
        match reader.next_event() {
            Ok(Some(RelayEvent::Event(e))) if e.id == event.id => delivered = Some(sent.elapsed()),
            Ok(Some(RelayEvent::Notice(n))) => println!("  notice: {n}"),
            Ok(_) => {}
            Err(e) => println!("  reader: {e}"),
        }
    }
    println!("{name}: accepted = {accepted:?}, delivered after = {delivered:?}");
    Ok(())
}

fn main() -> Result<(), String> {
    let url = std::env::args().nth(1).ok_or("usage: relay_probe <url>")?;
    let (a, b) = (Keys::generate(), Keys::generate());
    let t = now();

    let envelope = Envelope {
        v: VERSION,
        id: hex(&[0; 32]),
        network: "probe".into(),
        sent_at: t,
        body: Body::RfqReject(Reject { reason: "liq2sol relay probe".into() }),
    };
    let (ephemeral, _) = wrap::wrap(&a, &b.public_key(), &envelope, t).map_err(|e| format!("{e:?}"))?;
    let since = t - 2 * 86_400;
    probe(&url, "kind 21059 ephemeral wrap", &ephemeral, WRAP_KIND, Some(&b.public_key()), since)?;

    let offer = Offer {
        v: VERSION,
        network: "probe".into(),
        program_id: String::new(),
        template_version: 0,
        directions: vec![],
        min_sol_units: Units(0),
        max_sol_units: Units(0),
        available_l2s: Units(0),
        available_s2l: Units(0),
        l2s_prices: vec![],
        s2l_prices: vec![],
        fee_coin_sats: 0,
        confirmations: 0,
        relays: vec![],
    };
    let event = wrap::offer_event(&a, &offer, t, 120).map_err(|e| format!("{e:?}"))?;
    probe(&url, "kind 38401 offer, expiring in 120s", &event, OFFER_KIND, None, t - 60)?;

    // How old may an ephemeral wrapper's outer timestamp be before the relay refuses it?
    for age in [0u64, 5, 30, 60, 300, 3_600] {
        let e = EventBuilder::new(Kind::from(WRAP_KIND), "probe")
            .tag(Tag::public_key(b.public_key()))
            .custom_created_at(Timestamp::from(t - age))
            .finalize(&Keys::generate())
            .map_err(|e| format!("{e:?}"))?;
        probe(&url, &format!("kind 21059, {age}s old"), &e, WRAP_KIND, Some(&b.public_key()), since)?;
    }

    // A wrapper's outer timestamp may be up to two days old. Does the relay still take it?
    let old = EventBuilder::new(Kind::from(WRAP_KIND), "probe")
        .tag(Tag::public_key(b.public_key()))
        .custom_created_at(Timestamp::from(t - 40 * 3_600))
        .finalize(&Keys::generate())
        .map_err(|e| format!("{e:?}"))?;
    probe(&url, "kind 21059 with a 40-hour-old timestamp", &old, WRAP_KIND, Some(&b.public_key()), since)?;
    Ok(())
}
