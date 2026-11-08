//! E6, E7 and E8 from docs/test-plan.md, plus the rules for every field a counterparty controls.

use liq2sol_rfq::messages::*;
use liq2sol_rfq::rules::*;
use liq2sol_rfq::wrap::*;
use nostr::nips::nip44::{self, Version};
use nostr::prelude::*;
use proptest::prelude::*;

const NOW: u64 = 1_793_888_350;
const NET: &str = "testnet";
const PROGRAM: &str = "EcK819kqJT6pHzQVLbzbUnK6ieaiS9JVGubPhC6Q2puU";
const SOLANA: &str = "5QRoU8iyH7auNkmSfc9L8UAN441PiKZ2dRcXa1Lo1GkK";

fn point(n: u8) -> String {
    let mut p = [n; 33];
    p[0] = 2;
    hex(&p)
}

fn id() -> String {
    hex(&[7; 32])
}

fn request() -> Request {
    Request { direction: Direction::L2S, amount_side: AmountSide::Send, amount: Units(100_000_000) }
}

fn envelope(body: Body) -> Envelope {
    Envelope { v: VERSION, id: id(), network: NET.into(), sent_at: NOW, body }
}

fn flat(bps: u32) -> Vec<PriceTier> {
    vec![PriceTier { up_to: Units(1_000_000_000), bps }]
}

fn maker_rules() -> MakerRules {
    MakerRules {
        network: NET.into(),
        min_sol_units: 10_000,
        max_sol_units: 1_000_000_000,
        available_l2s: 1_000_000_000,
        available_s2l: 1_000_000_000,
        l2s_prices: flat(30),
        s2l_prices: flat(30),
        quote_secs: 60,
        taker_lock_window: 30,
        l2s_lock_blocks: 300,
        s2l_lock_blocks: 90,
        min_deadline_slots: 8_400,
    }
}

fn taker_rules() -> TakerRules {
    TakerRules {
        network: NET.into(),
        program_id: PROGRAM.into(),
        usdt_asset: hex(&[0xce; 32]),
        template_version: TEMPLATE_VERSION,
        l2s_min_blocks: 240,
        l2s_max_blocks: 360,
        s2l_min_blocks: 50,
        s2l_max_blocks: 150,
        max_spread_bps: 100,
        max_deadline_slots: 20_000,
    }
}

fn quote() -> Quote {
    Quote { send: Units(100_000_000), receive: Units(99_700_000), quote_expiry: NOW + 60, taker_lock_window: 30 }
}

fn terms() -> MakerTerms {
    MakerTerms {
        accept_event: "accept-id".into(),
        h_accept: 1_000,
        lock_height: 1_300,
        maker_key: point(1),
        maker_solana: SOLANA.into(),
        maker_initiator: Some(SOLANA.into()),
        usdt_asset: hex(&[0xce; 32]),
        program_id: PROGRAM.into(),
        template_version: TEMPLATE_VERSION,
        liquid_amount: Units(10_000_000_000),
        solana_amount: Units(99_700_000),
        min_deadline_slots: 8_400,
    }
}

fn accept() -> Accept {
    Accept {
        quote_event: "quote-id".into(),
        lock_point: point(2),
        taker_key: point(3),
        blinding_key: hex(&[4; 32]),
        taker_solana: SOLANA.into(),
    }
}

// ---- Envelope: E6 ----

#[test]
fn stale_replayed_and_future_messages_are_refused() {
    let e = envelope(Body::RfqRequest(request()));
    assert_eq!(check_envelope(&e, NET, NOW), Ok(()));
    assert_eq!(check_envelope(&e, NET, NOW + MAX_CLOCK_SKEW_SECS), Ok(()));
    assert_eq!(check_envelope(&e, NET, NOW + MAX_CLOCK_SKEW_SECS + 1), Err(Refused::Stale), "an old replay");
    assert_eq!(check_envelope(&e, NET, NOW - MAX_CLOCK_SKEW_SECS - 1), Err(Refused::Stale), "from the future");
}

#[test]
fn another_network_or_version_is_refused() {
    let e = envelope(Body::RfqRequest(request()));
    assert_eq!(check_envelope(&e, "mainnet", NOW), Err(Refused::WrongNetwork));
    assert_eq!(check_envelope(&Envelope { v: 1, ..e.clone() }, NET, NOW), Err(Refused::WrongVersion));
    for bad in ["", "abc", &"G".repeat(64), &"A".repeat(64), &"a".repeat(62), &"a".repeat(66)] {
        assert_eq!(check_envelope(&Envelope { id: bad.into(), ..e.clone() }, NET, NOW), Err(Refused::BadId), "{bad}");
    }
}

#[test]
fn an_accept_must_name_our_quote_and_arrive_in_time() {
    let q = quote();
    assert!(check_accept(&accept(), &q, "quote-id", NOW).is_ok());
    assert_eq!(check_accept(&accept(), &q, "another-quote", NOW), Err(Refused::WrongReference));
    assert_eq!(check_accept(&accept(), &q, "quote-id", NOW + 61), Err(Refused::QuoteExpired));
}

#[test]
fn accept_fields_are_validated() {
    let q = quote();
    let bad = |a: Accept| assert!(matches!(check_accept(&a, &q, "quote-id", NOW), Err(Refused::Malformed(_))), "{a:?}");
    bad(Accept { lock_point: "zz".into(), ..accept() });
    bad(Accept { lock_point: hex(&[2; 32]), ..accept() });
    bad(Accept { lock_point: hex(&[4; 33]), ..accept() }); // Not a compressed point's prefix.
    bad(Accept { lock_point: point(0xab).to_uppercase(), ..accept() });
    bad(Accept { taker_key: hex(&[3; 32]), ..accept() });
    bad(Accept { taker_key: hex(&[0; 33]), ..accept() });
    bad(Accept { blinding_key: hex(&[4; 33]), ..accept() });
    bad(Accept { blinding_key: "".into(), ..accept() });
    bad(Accept { taker_solana: "not a solana address".into(), ..accept() });
}

/// A quote the maker can't fill would tie up the taker's funds until a refund.
#[test]
fn a_maker_never_quotes_more_than_it_can_pay() {
    let request = |direction| Request { direction, amount_side: AmountSide::Receive, amount: Units(1_000_000) };
    let rules = MakerRules { available_l2s: 1_000_000, available_s2l: 999_999, ..maker_rules() };
    assert!(make_quote(&rules, &request(Direction::L2S), NOW).is_ok());
    assert_eq!(make_quote(&rules, &request(Direction::S2L), NOW), Err(Refused::OutOfLimits));
    let rules = MakerRules { available_l2s: 999_999, available_s2l: 1_000_000, ..maker_rules() };
    assert_eq!(make_quote(&rules, &request(Direction::L2S), NOW), Err(Refused::OutOfLimits));
    assert!(make_quote(&rules, &request(Direction::S2L), NOW).is_ok());
}

#[test]
fn old_accepts_are_refused_outright() {
    // A taker on the hash-lock protocol mustn't be read as anything else.
    let old = serde_json::json!({
        "quote_event": "q", "payment_hash": hex(&[2; 32]), "taker_liquid_pk": hex(&[3; 32]), "taker_solana": SOLANA,
        "taker_liquid_script": null, "taker_fee_script": null
    });
    assert!(serde_json::from_value::<Accept>(old).is_err());
}

/// The biggest thing either side ever sends: a blinded claim, whose witness is about 5.5 KB. It must fit the event limit once
/// it has been through both layers of NIP-59 encryption.
#[test]
fn a_blinded_claim_fits_in_one_event() {
    let (maker, taker) = (Keys::generate(), Keys::generate());
    let body = Body::ClaimOffer(ClaimOffer { lock_point: point(2), tx: "ab".repeat(6_000), nonce: "cd".repeat(66) });
    let (event, _) = wrap(&maker, &taker.public_key(), &envelope(body.clone()), NOW).unwrap();
    assert!(event.as_json().len() <= MAX_EVENT_BYTES, "{} bytes", event.as_json().len());
    assert_eq!(unwrap(&taker, &event).unwrap().envelope.body, body);
}

// ---- Pricing ----

fn tiers(list: &[(u64, u32)]) -> Vec<PriceTier> {
    list.iter().map(|(up_to, bps)| PriceTier { up_to: Units(*up_to), bps: *bps }).collect()
}

#[test]
fn prices_differ_by_size_and_direction() {
    let rules = MakerRules {
        l2s_prices: tiers(&[(100_000, 20), (1_000_000, 30), (1_000_000_000, 50)]),
        s2l_prices: tiers(&[(1_000_000, 40), (1_000_000_000, 70)]),
        ..maker_rules()
    };
    let quote = |direction, amount| {
        let r = Request { direction, amount_side: AmountSide::Send, amount: Units(amount) };
        make_quote(&rules, &r, NOW).unwrap().receive.0
    };
    assert_eq!(quote(Direction::L2S, 100_000), 99_800, "the top of the first tier");
    assert_eq!(quote(Direction::L2S, 100_001), 99_700, "one unit over takes the next rate, on the whole swap");
    assert_eq!(quote(Direction::L2S, 1_000_000), 997_000);
    assert_eq!(quote(Direction::L2S, 2_000_000), 1_990_000);
    assert_eq!(quote(Direction::S2L, 100_000), 99_600, "the other direction has its own prices");
    assert_eq!(quote(Direction::S2L, 2_000_000), 1_986_000);
}

/// Asking for what to receive lands in the tier for what that costs, so both ways of asking give one price.
#[test]
fn a_tier_is_chosen_by_what_the_taker_sends() {
    let t = tiers(&[(100_000, 20), (1_000_000, 30)]);
    let recv = |a| Request { direction: Direction::L2S, amount_side: AmountSide::Receive, amount: Units(a) };
    assert_eq!(bps_for(&t, &recv(99_800)), Some(20));
    assert_eq!(bps_for(&t, &recv(99_801)), Some(30), "costs more than 100,000 to send");
    assert_eq!(bps_for(&t, &recv(2_000_000)), None, "above every tier");
}

#[test]
fn a_size_above_every_tier_gets_no_quote() {
    let rules = MakerRules { l2s_prices: tiers(&[(100_000, 20)]), ..maker_rules() };
    let r = Request { direction: Direction::L2S, amount_side: AmountSide::Send, amount: Units(100_001) };
    assert_eq!(make_quote(&rules, &r, NOW), Err(Refused::OutOfLimits));
}

#[test]
fn unusable_price_lists_are_refused() {
    assert!(prices_ok(&tiers(&[(10, 30), (20, 40)]), 20));
    assert!(!prices_ok(&[], 20), "no prices");
    assert!(!prices_ok(&tiers(&[(10, 30)]), 20), "doesn't reach the maximum");
    assert!(!prices_ok(&tiers(&[(20, 30), (10, 40), (30, 50)]), 20), "out of order");
    assert!(!prices_ok(&tiers(&[(20, 30), (20, 40)]), 20), "the same step twice");
    assert!(!prices_ok(&tiers(&[(20, 10_000)]), 20), "pays nothing");
    let many: Vec<(u64, u32)> = (1..=17).map(|i| (i, 10)).collect();
    assert!(!prices_ok(&tiers(&many), 1), "too many steps");
}

#[test]
fn prices_round_in_the_makers_favour() {
    let send = |a| Request { direction: Direction::L2S, amount_side: AmountSide::Send, amount: Units(a) };
    let recv = |a| Request { direction: Direction::L2S, amount_side: AmountSide::Receive, amount: Units(a) };
    assert_eq!(price(&send(10_000), 30), Some((10_000, 9_970)));
    assert_eq!(price(&send(10_001), 30), Some((10_001, 9_970)), "rounds the payout down");
    assert_eq!(price(&recv(9_970), 30), Some((10_000, 9_970)));
    assert_eq!(price(&recv(9_971), 30), Some((10_002, 9_971)), "rounds the cost up");
    assert_eq!(price(&send(1), 30), None, "a payout of nothing");
    assert_eq!(price(&send(u64::MAX), 0), Some((u64::MAX, u64::MAX)));
    assert_eq!(price(&recv(u64::MAX), 30), None, "no overflow");
    assert_eq!(price(&send(100), 10_000), None, "a 100% spread pays nothing");
    assert_eq!(price(&send(100), 10_001), None, "no underflow");
    assert_eq!(price(&recv(100), 10_000), None, "no division by zero");
}

#[test]
fn quotes_respect_limits() {
    let r = maker_rules();
    let q = make_quote(&r, &request(), NOW).unwrap();
    assert_eq!((q.send.0, q.receive.0, q.quote_expiry), (100_000_000, 99_700_000, NOW + 60));
    let at = |a| make_quote(&r, &Request { amount: Units(a), ..request() }, NOW);
    assert_eq!(at(1_000_000_001), Err(Refused::OutOfLimits));
    assert_eq!(at(9_999), Err(Refused::OutOfLimits));
    assert_eq!(at(10_000), Err(Refused::OutOfLimits), "the payout would be under the minimum");
    assert!(at(1_000_000_000).is_ok());
    let huge = MakerRules { max_sol_units: u64::MAX, ..maker_rules() };
    assert_eq!(
        make_quote(&huge, &Request { amount: Units(u64::MAX), ..request() }, NOW),
        Err(Refused::OutOfLimits),
        "the Liquid amount must fit"
    );
}

proptest! {
    /// The maker never quotes a payout above what it receives, at any size or spread.
    #[test]
    fn the_maker_never_quotes_at_a_loss(amount in any::<u64>(), bps in 0u32..12_000, receive_side in any::<bool>()) {
        let side = if receive_side { AmountSide::Receive } else { AmountSide::Send };
        let r = Request { direction: Direction::L2S, amount_side: side, amount: Units(amount) };
        if let Some((send, receive)) = price(&r, bps) {
            prop_assert!(receive <= send, "{send} {receive}");
            let earned = u128::from(send - receive) * 10_000;
            let floor = u128::from(send) * u128::from(bps);
            prop_assert!(earned >= floor, "the spread is at least what was asked");
        }
    }

    /// Garbage must never panic any parser, and anything that parses must survive a round trip.
    #[test]
    fn parsers_survive_garbage(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
        if let Ok(e) = serde_json::from_slice::<Envelope>(&bytes) {
            let again: Envelope = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
            prop_assert_eq!(e, again);
        }
        let _ = serde_json::from_slice::<Offer>(&bytes);
        let text = String::from_utf8_lossy(&bytes);
        let _ = hex_any(&text);
        let _ = hex32(&text);
    }

    /// Structured garbage gets further into the parser, so try that too.
    #[test]
    fn parsers_survive_near_valid_json(
        field in "[a-z_]{0,12}",
        value in prop_oneof![any::<i64>().prop_map(|n| n.to_string()), "\"[ -~]{0,40}\"", Just("null".to_string()), Just("[]".to_string())],
    ) {
        let text = format!(r#"{{"v":1,"id":"{}","network":"testnet","sent_at":1,"type":"rfq-request","body":{{"direction":"L2S","amount_side":"send","amount":"1","{field}":{value}}}}}"#, id());
        let _ = serde_json::from_str::<Envelope>(&text);
    }
}

#[test]
fn amounts_have_exactly_one_spelling() {
    let parse = |s: &str| serde_json::from_str::<Units>(s);
    assert_eq!(parse("\"100\"").unwrap(), Units(100));
    assert_eq!(parse("\"0\"").unwrap(), Units(0));
    for bad in [
        "100",
        "\"\"",
        "\"-1\"",
        "\"+1\"",
        "\"1e3\"",
        "\"1.0\"",
        "\" 1\"",
        "\"01\"",
        "\"18446744073709551616\"",
        "null",
    ] {
        assert!(parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn unknown_fields_are_refused() {
    let text = format!(
        r#"{{"v":1,"id":"{}","network":"testnet","sent_at":1,"type":"rfq-request","body":{{"direction":"L2S","amount_side":"send","amount":"1","extra":1}}}}"#,
        id()
    );
    assert!(serde_json::from_str::<Envelope>(&text).is_err());
}

// ---- The taker's checks: E8 ----

#[test]
fn the_taker_refuses_bad_quotes() {
    let r = taker_rules();
    assert!(check_quote(&r, &request(), &quote(), NOW).is_ok());
    assert_eq!(check_quote(&r, &request(), &quote(), NOW + 61), Err(Refused::QuoteExpired));
    let q = |send, receive| Quote { send: Units(send), receive: Units(receive), ..quote() };
    assert_eq!(
        check_quote(&r, &request(), &q(100_000_001, 99_700_000), NOW),
        Err(Refused::BadAmounts),
        "not what was asked"
    );
    assert_eq!(check_quote(&r, &request(), &q(100_000_000, 100_000_001), NOW), Err(Refused::BadAmounts), "impossible");
    assert_eq!(check_quote(&r, &request(), &q(100_000_000, 0), NOW), Err(Refused::BadAmounts));
    assert_eq!(
        check_quote(&r, &request(), &q(100_000_000, 98_999_999), NOW),
        Err(Refused::BadAmounts),
        "over the spread limit"
    );
    assert!(check_quote(&r, &request(), &q(100_000_000, 99_000_000), NOW).is_ok(), "exactly at the limit");
}

#[test]
fn the_taker_refuses_every_bad_term_before_funding() {
    let r = taker_rules();
    let q = quote();
    let tip = 1_000;
    let check = |t: MakerTerms| check_terms(&r, Direction::L2S, &q, "accept-id", &t, tip);
    assert_eq!(check(terms()), Ok(()));
    assert_eq!(check(MakerTerms { accept_event: "x".into(), ..terms() }), Err(Refused::WrongReference));
    assert_eq!(check(MakerTerms { program_id: SOLANA.into(), ..terms() }), Err(Refused::WrongProgram));
    assert_eq!(check(MakerTerms { usdt_asset: hex(&[1; 32]), ..terms() }), Err(Refused::WrongAsset));
    assert_eq!(check(MakerTerms { template_version: 1, ..terms() }), Err(Refused::WrongTemplate));
    assert_eq!(
        check(MakerTerms { liquid_amount: Units(10_000_000_100), ..terms() }),
        Err(Refused::BadAmounts),
        "pay more"
    );
    assert_eq!(
        check(MakerTerms { solana_amount: Units(99_699_999), ..terms() }),
        Err(Refused::BadAmounts),
        "receive less"
    );
    assert_eq!(check(MakerTerms { lock_height: tip + 239, ..terms() }), Err(Refused::BadLockHeight), "too short");
    assert_eq!(
        check(MakerTerms { lock_height: tip + 361, ..terms() }),
        Err(Refused::BadLockHeight),
        "funds stuck too long"
    );
    assert_eq!(check(MakerTerms { lock_height: tip - 1, ..terms() }), Err(Refused::BadLockHeight), "already past");
    assert_eq!(check(MakerTerms { min_deadline_slots: 20_001, ..terms() }), Err(Refused::BadLockHeight));
    assert!(matches!(check(MakerTerms { maker_key: "zz".into(), ..terms() }), Err(Refused::Malformed(_))));
    assert!(matches!(check(MakerTerms { maker_solana: "bad".into(), ..terms() }), Err(Refused::Malformed(_))));
    assert!(matches!(check(MakerTerms { maker_key: hex(&[1; 32]), ..terms() }), Err(Refused::Malformed(_))));
    assert!(matches!(check(MakerTerms { maker_initiator: None, ..terms() }), Err(Refused::Malformed(_))));
}

#[test]
fn s2l_terms_map_the_amounts_the_other_way() {
    let r = taker_rules();
    let good = MakerTerms {
        lock_height: 1_090,
        liquid_amount: Units(9_970_000_000),
        solana_amount: Units(100_000_000),
        maker_initiator: None,
        ..terms()
    };
    let check = |t: MakerTerms| check_terms(&r, Direction::S2L, &quote(), "accept-id", &t, 1_000);
    assert_eq!(check(good.clone()), Ok(()));
    assert_eq!(check(MakerTerms { liquid_amount: Units(9_969_999_900), ..good.clone() }), Err(Refused::BadAmounts));
    assert_eq!(check(MakerTerms { lock_height: 1_049, ..good.clone() }), Err(Refused::BadLockHeight));
    assert_eq!(check(MakerTerms { lock_height: 1_151, ..good }), Err(Refused::BadLockHeight));
}

#[test]
fn rate_limits_hold_and_recover() {
    let mut r = RateLimit::default();
    assert!(r.allow("a", 100, 60, 2));
    assert!(r.allow("a", 101, 60, 2));
    assert!(!r.allow("a", 102, 60, 2));
    assert!(r.allow("b", 102, 60, 2), "per sender");
    assert!(r.allow("a", 161, 60, 2), "the window moves on");
}

// ---- Wrapping: E7 ----

fn sample() -> Envelope {
    envelope(Body::RfqRequest(request()))
}

#[test]
fn a_wrapped_message_opens_and_authenticates_its_sender() {
    let (alice, bob) = (Keys::generate(), Keys::generate());
    let (event, rumor_id) = wrap(&alice, &bob.public_key(), &sample(), NOW).unwrap();
    let got = unwrap(&bob, &event).unwrap();
    assert_eq!(got.sender, alice.public_key());
    assert_eq!(got.rumor_id, rumor_id);
    assert_eq!(got.envelope, sample());
    assert_ne!(event.pubkey, alice.public_key(), "the outer key reveals nothing");
    assert_eq!(event.kind, Kind::from(WRAP_KIND));
    assert_eq!(event.created_at.as_secs(), NOW, "relays drop ephemeral events with an old timestamp");
    assert!(!event.content.contains("rfq"), "relays can't read it");
}

#[test]
fn only_the_recipient_can_open_it() {
    let (alice, bob) = (Keys::generate(), Keys::generate());
    let (event, _) = wrap(&alice, &bob.public_key(), &sample(), NOW).unwrap();
    assert_eq!(unwrap(&Keys::generate(), &event), Err(WrapError::CannotDecrypt));
}

/// Build a wrapper by hand, so a forger controls every layer.
fn forge(seal_signer: &Keys, rumor_author: PublicKey, to: &PublicKey, kind: u16, content: String) -> Event {
    let mut rumor =
        UnsignedEvent::new(rumor_author, Timestamp::from(NOW), Kind::from(kind), Vec::<Tag>::new(), content);
    rumor.ensure_id();
    let sealed = nip44::encrypt(seal_signer.secret_key(), to, rumor.as_json(), Version::V2).unwrap();
    let seal = EventBuilder::new(Kind::from(SEAL_KIND), sealed).finalize(seal_signer).unwrap();
    let throwaway = Keys::generate();
    let wrapped = nip44::encrypt(throwaway.secret_key(), to, seal.as_json(), Version::V2).unwrap();
    EventBuilder::new(Kind::from(WRAP_KIND), wrapped).tag(Tag::public_key(*to)).finalize(&throwaway).unwrap()
}

#[test]
fn a_forged_author_is_refused() {
    let (mallory, maker, bob) = (Keys::generate(), Keys::generate(), Keys::generate());
    let content = serde_json::to_string(&sample()).unwrap();
    let forged = forge(&mallory, maker.public_key(), &bob.public_key(), RUMOR_KIND, content.clone());
    assert_eq!(unwrap(&bob, &forged), Err(WrapError::Spoofed), "the rumor claims to be the maker");
    let honest = forge(&mallory, mallory.public_key(), &bob.public_key(), RUMOR_KIND, content);
    assert_eq!(
        unwrap(&bob, &honest).unwrap().sender,
        mallory.public_key(),
        "and is only ever attributed to its signer"
    );
}

#[test]
fn wrong_inner_kinds_and_tampering_are_refused() {
    let (alice, bob) = (Keys::generate(), Keys::generate());
    let content = serde_json::to_string(&sample()).unwrap();
    let wrong = forge(&alice, alice.public_key(), &bob.public_key(), 1, content);
    assert_eq!(unwrap(&bob, &wrong), Err(WrapError::BadRumor));
    let garbage = forge(&alice, alice.public_key(), &bob.public_key(), RUMOR_KIND, "not an envelope".into());
    assert_eq!(unwrap(&bob, &garbage), Err(WrapError::Malformed));

    let (event, _) = wrap(&alice, &bob.public_key(), &sample(), NOW).unwrap();
    let mut json: serde_json::Value = serde_json::from_str(&event.as_json()).unwrap();
    json["content"] = serde_json::Value::String("AAAA".into());
    let tampered = Event::from_json(json.to_string()).unwrap();
    assert_eq!(unwrap(&bob, &tampered), Err(WrapError::BadEvent), "the outer signature no longer matches");

    let plain = EventBuilder::new(Kind::TextNote, "hello").finalize(&alice).unwrap();
    assert_eq!(unwrap(&bob, &plain), Err(WrapError::NotAWrap));
}

#[test]
fn oversized_events_are_refused_before_any_crypto() {
    let (alice, bob) = (Keys::generate(), Keys::generate());
    let big = EventBuilder::new(Kind::from(WRAP_KIND), "x".repeat(MAX_EVENT_BYTES + 1)).finalize(&alice).unwrap();
    assert_eq!(unwrap(&bob, &big), Err(WrapError::TooLarge));
}

#[test]
fn an_offer_with_unusable_prices_is_ignored() {
    let maker = Keys::generate();
    let good = Offer {
        v: VERSION,
        network: NET.into(),
        program_id: PROGRAM.into(),
        template_version: TEMPLATE_VERSION,
        directions: vec![Direction::L2S],
        min_sol_units: Units(10_000),
        max_sol_units: Units(1_000_000_000),
        available_l2s: Units(1),
        available_s2l: Units(1),
        l2s_prices: flat(30),
        s2l_prices: flat(30),
        fee_coin_sats: 30,
        confirmations: 2,
        relays: vec![],
    };
    assert!(read_offer(&offer_event(&maker, &good, NOW, 60).unwrap(), NET, NOW).is_some());
    let short = Offer { s2l_prices: tiers(&[(5, 30)]), ..good };
    assert!(read_offer(&offer_event(&maker, &short, NOW, 60).unwrap(), NET, NOW).is_none());
}

#[test]
fn offers_expire_and_are_network_scoped() {
    let maker = Keys::generate();
    let offer = Offer {
        v: VERSION,
        network: NET.into(),
        program_id: PROGRAM.into(),
        template_version: TEMPLATE_VERSION,
        directions: vec![Direction::L2S, Direction::S2L],
        min_sol_units: Units(10_000),
        max_sol_units: Units(1_000_000_000),
        available_l2s: Units(1_000_000_000),
        available_s2l: Units(1_000_000_000),
        l2s_prices: flat(30),
        s2l_prices: flat(30),
        fee_coin_sats: 300,
        confirmations: 2,
        relays: vec![],
    };
    let event = offer_event(&maker, &offer, NOW, 1_800).unwrap();
    assert_eq!(read_offer(&event, NET, NOW), Some(offer.clone()));
    assert_eq!(read_offer(&event, NET, NOW + 1_800), Some(offer.clone()));
    assert_eq!(read_offer(&event, NET, NOW + 1_801), None, "a dead maker disappears");
    assert_eq!(read_offer(&event, "mainnet", NOW), None);
    assert!(event.tags.identifier().is_some_and(|d| d.contains(NET)), "addressable per network");
}

/// The page gives a checkmark by key, so nobody may publish an offer under a key they don't hold.
#[test]
fn an_offer_cannot_claim_someone_elses_key() {
    let (victim, forger) = (Keys::generate(), Keys::generate());
    let offer = Offer {
        v: VERSION,
        network: NET.into(),
        program_id: PROGRAM.into(),
        template_version: TEMPLATE_VERSION,
        directions: vec![Direction::L2S],
        min_sol_units: Units(10_000),
        max_sol_units: Units(1_000_000_000),
        available_l2s: Units(1),
        available_s2l: Units(1),
        l2s_prices: tiers(&[(1_000_000_000, 30)]),
        s2l_prices: tiers(&[(1_000_000_000, 30)]),
        fee_coin_sats: 30,
        confirmations: 2,
        relays: vec![],
    };
    let signed = offer_event(&forger, &offer, NOW, 60).unwrap();
    assert!(read_offer(&signed, NET, NOW).is_some(), "control: the forger's own offer is fine");
    let mut json: serde_json::Value = serde_json::from_str(&signed.as_json()).unwrap();
    json["pubkey"] = victim.public_key().to_hex().into();
    let forged = Event::from_json(json.to_string()).unwrap();
    assert_eq!(forged.pubkey, victim.public_key());
    assert!(read_offer(&forged, NET, NOW).is_none(), "signed by another key");

    let real = offer_event(&victim, &offer, NOW, 60).unwrap();
    let mut json: serde_json::Value = serde_json::from_str(&real.as_json()).unwrap();
    json["content"] = json["content"].as_str().unwrap().replace("\"bps\":30", "\"bps\":1").into();
    let changed = Event::from_json(json.to_string()).unwrap();
    assert_ne!(changed.content, real.content, "the edit took");
    assert!(read_offer(&changed, NET, NOW).is_none(), "a real signature over other terms");
}

/// `forge`, wrapped as a NIP-17 gift wrap.
fn forge_dm(seal_signer: &Keys, rumor_author: PublicKey, to: &PublicKey, kind: u16, content: &str) -> Event {
    let mut rumor = UnsignedEvent::new(rumor_author, Timestamp::from(NOW), Kind::from(kind), Vec::<Tag>::new(), content);
    rumor.ensure_id();
    let sealed = nip44::encrypt(seal_signer.secret_key(), to, rumor.as_json(), Version::V2).unwrap();
    let seal = EventBuilder::new(Kind::from(SEAL_KIND), sealed).finalize(seal_signer).unwrap();
    let throwaway = Keys::generate();
    let wrapped = nip44::encrypt(throwaway.secret_key(), to, seal.as_json(), Version::V2).unwrap();
    EventBuilder::new(Kind::from(GIFT_WRAP_KIND), wrapped).tag(Tag::public_key(*to)).finalize(&throwaway).unwrap()
}

#[test]
fn a_private_message_follows_nip17_and_only_its_recipient_can_read_it() {
    let (maker, operator, stranger) = (Keys::generate(), Keys::generate(), Keys::generate());
    let event = private_message(&maker, &operator.public_key(), "PAGE Rollback", NOW).unwrap();
    // Outside: a stored gift wrap to the operator, from a throwaway key, at a time up to two days back.
    assert_eq!(event.kind, Kind::from(GIFT_WRAP_KIND));
    assert_ne!(event.pubkey, maker.public_key(), "the wrap doesn't show who sent it");
    assert_eq!(event.tags.public_keys().collect::<Vec<_>>(), vec![operator.public_key()]);
    let at = event.created_at.as_secs();
    assert!((NOW - 2 * 86_400..=NOW).contains(&at), "randomized into the past, never the future");
    // Inside: the seal, then a kind 14 rumor that names the operator, as Nostr apps expect.
    let seal = Event::from_json(nip44::decrypt(operator.secret_key(), &event.pubkey, &event.content).unwrap()).unwrap();
    assert_eq!(seal.kind, Kind::from(SEAL_KIND));
    assert!(seal.created_at.as_secs() <= NOW);
    let rumor =
        UnsignedEvent::from_json(nip44::decrypt(operator.secret_key(), &seal.pubkey, &seal.content).unwrap()).unwrap();
    assert_eq!(rumor.kind, Kind::from(DM_KIND));
    assert_eq!(rumor.created_at.as_secs(), NOW, "the message itself keeps the real time");
    assert_eq!(rumor.tags.public_keys().collect::<Vec<_>>(), vec![operator.public_key()]);
    assert_eq!(open_private_message(&operator, &event), Ok((maker.public_key(), "PAGE Rollback".to_string())));
    assert_eq!(open_private_message(&stranger, &event), Err(WrapError::CannotDecrypt));
}

#[test]
fn a_private_message_with_a_forged_author_or_kind_is_refused() {
    let (mallory, maker, operator) = (Keys::generate(), Keys::generate(), Keys::generate());
    let to = operator.public_key();
    let forged = forge_dm(&mallory, maker.public_key(), &to, DM_KIND, "all is well");
    assert_eq!(open_private_message(&operator, &forged), Err(WrapError::Spoofed), "the rumor claims to be the maker");
    let wrong = forge_dm(&maker, maker.public_key(), &to, 1, "a note");
    assert_eq!(open_private_message(&operator, &wrong), Err(WrapError::BadRumor));
    // Our own swap wrapper is not a private message.
    let (ours, _) = wrap(&maker, &to, &sample(), NOW).unwrap();
    assert_eq!(open_private_message(&operator, &ours), Err(WrapError::NotAWrap));
}
