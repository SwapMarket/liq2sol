//! The taker's real negotiation flow against a scripted maker on a mock relay. The scripted side can decrypt what it
//! receives, so it knows the negotiation id, which makes it a stronger attacker than any outsider could be.

use std::time::{Duration, Instant};

use liq2sol_rfq::messages::*;
use liq2sol_rfq::mock_relay::MockRelay;
use liq2sol_rfq::relay::{Relay, RelayEvent};
use liq2sol_rfq::rules::*;
use liq2sol_rfq::taker::*;
use liq2sol_rfq::wrap;
use nostr::prelude::*;

const NET: &str = "testnet";
const PROGRAM: &str = "DxVq5iSzKfRMAyis8RyLUeLggidM2Jvk1FZyyEauCRQ1";
const SOLANA: &str = "5QRoU8iyH7auNkmSfc9L8UAN441PiKZ2dRcXa1Lo1GkK";
const TIP: u32 = 1_000;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn rules() -> TakerRules {
    TakerRules {
        network: NET.into(),
        program_id: PROGRAM.into(),
        usdt_asset: hex(&[0xce; 32]),
        template_version: 1,
        l2s_min_blocks: 240,
        l2s_max_blocks: 360,
        s2l_min_blocks: 50,
        s2l_max_blocks: 150,
        max_spread_bps: 100,
        max_deadline_slots: 20_000,
    }
}

fn request() -> Request {
    Request { direction: Direction::L2S, amount_side: AmountSide::Send, amount: Units(10_000) }
}

fn side() -> TakerSide {
    TakerSide {
        payment_hash: [2; 32],
        taker_liquid_pk: [3; 32],
        taker_solana: SOLANA.into(),
        taker_liquid_script: None,
        taker_fee_script: None,
    }
}

fn good_quote() -> Quote {
    Quote { send: Units(10_000), receive: Units(9_970), quote_expiry: now() + 60, taker_lock_window: 30 }
}

fn good_terms(accept_event: String) -> MakerTerms {
    MakerTerms {
        accept_event,
        h_accept: TIP,
        lock_height: TIP + 300,
        maker_liquid_pk: hex(&[1; 32]),
        maker_liquid_script: Some(hex(&[0x51; 34])),
        maker_solana: SOLANA.into(),
        maker_initiator: Some(SOLANA.into()),
        usdt_asset: hex(&[0xce; 32]),
        program_id: PROGRAM.into(),
        template_version: 1,
        liquid_amount: Units(1_000_000),
        solana_amount: Units(9_970),
        min_deadline_slots: 8_400,
    }
}

/// How the scripted maker answers.
#[derive(Clone)]
struct Script {
    /// Sent to the taker first, signed by a stranger who somehow knows the negotiation id.
    forger_goes_first: bool,
    /// Re-sent later by the stranger, as an accept is on its way.
    change_quote: fn(Quote) -> Quote,
    change_terms: fn(MakerTerms) -> MakerTerms,
    reject: bool,
}

fn same_quote(q: Quote) -> Quote {
    q
}

fn same_terms(t: MakerTerms) -> MakerTerms {
    t
}

fn plain() -> Script {
    Script { forger_goes_first: false, change_quote: same_quote, change_terms: same_terms, reject: false }
}

fn maker_thread(url: String, maker: Keys, script: Script) {
    std::thread::spawn(move || {
        let forger = Keys::generate();
        let mut relay = Relay::connect(&url, Duration::from_millis(100)).unwrap();
        relay.subscribe("m", &[Kind::from(WRAP_KIND)], Some(&maker.public_key()), now() - 2 * 86_400).unwrap();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(15) {
            let Ok(Some(RelayEvent::Event(e))) = relay.next_event() else { continue };
            let Ok(got) = wrap::unwrap(&maker, &e) else { continue };
            let respond = |keys: &Keys, body: Body| {
                let env =
                    Envelope { v: VERSION, id: got.envelope.id.clone(), network: NET.into(), sent_at: now(), body };
                wrap::wrap(keys, &got.sender, &env, now()).unwrap().0
            };
            match &got.envelope.body {
                Body::RfqRequest(_) => {
                    if script.forger_goes_first {
                        // Better terms than the real maker's, for a taker who doesn't check who sent them.
                        let tempting = Quote { receive: Units(10_000), ..good_quote() };
                        relay.publish(&respond(&forger, Body::RfqQuote(tempting))).unwrap();
                    }
                    if script.reject {
                        relay.publish(&respond(&maker, Body::RfqReject(Reject { reason: "no".into() }))).unwrap();
                    } else {
                        relay.publish(&respond(&maker, Body::RfqQuote((script.change_quote)(good_quote())))).unwrap();
                    }
                }
                Body::RfqAccept(_) => {
                    if script.forger_goes_first {
                        let fake = MakerTerms {
                            maker_solana: "11111111111111111111111111111111".into(),
                            ..good_terms(got.rumor_id.clone())
                        };
                        relay.publish(&respond(&forger, Body::RfqTerms(fake))).unwrap();
                    }
                    let terms = (script.change_terms)(good_terms(got.rumor_id.clone()));
                    relay.publish(&respond(&maker, Body::RfqTerms(terms))).unwrap();
                }
                _ => {}
            }
        }
    });
}

fn run(script: Script) -> Result<Negotiated, String> {
    let relay = MockRelay::start();
    let maker = Keys::generate();
    maker_thread(relay.url(), maker.clone(), script);
    std::thread::sleep(Duration::from_millis(300));
    let me = Keys::generate();
    let mut conn = Relay::connect(&relay.url(), Duration::from_millis(100))?;
    negotiate(&mut conn, &me, &maker.public_key(), &rules(), request(), side(), || Ok(TIP), Duration::from_secs(5))
}

#[test]
fn an_honest_maker_gets_through() {
    let got = run(plain()).unwrap();
    assert_eq!(got.quote.receive, Units(9_970));
    assert_eq!(got.terms.solana_amount, Units(9_970));
}

/// A stranger who knows the negotiation id, with better numbers than the real maker, must be ignored at both steps.
#[test]
fn a_forger_is_ignored_at_every_step() {
    let got = run(Script { forger_goes_first: true, ..plain() }).unwrap();
    assert_eq!(got.quote.receive, Units(9_970), "the real maker's quote, not the tempting one");
    assert_eq!(got.terms.maker_solana, SOLANA, "the real maker's terms");
}

/// E8, through the real flow: a maker lying at the last step gets no funding, because `negotiate` returns an error.
#[test]
fn a_lying_maker_is_stopped_before_funding() {
    type Change = fn(MakerTerms) -> MakerTerms;
    let cases: Vec<(&str, Change)> = vec![
        ("short lock height", |t| MakerTerms { lock_height: TIP + 10, ..t }),
        ("wrong asset", |t| MakerTerms { usdt_asset: hex(&[1; 32]), ..t }),
        ("wrong program", |t| MakerTerms { program_id: SOLANA.into(), ..t }),
        ("wrong amount", |t| MakerTerms { solana_amount: Units(1), ..t }),
        ("answers another accept", |t| MakerTerms { accept_event: "other".into(), ..t }),
    ];
    for (name, change_terms) in cases {
        let got = run(Script { change_terms, ..plain() });
        let err = got.err().unwrap_or_else(|| panic!("{name} was accepted"));
        assert!(err.contains("bad terms"), "{name}: {err}");
    }
}

#[test]
fn a_bad_quote_is_refused_before_accepting() {
    let got = run(Script { change_quote: |q| Quote { receive: Units(1), ..q }, ..plain() });
    assert!(got.err().unwrap().contains("bad quote"));
}

#[test]
fn a_rejection_ends_the_negotiation() {
    assert!(run(Script { reject: true, ..plain() }).err().unwrap().contains("declined"));
}
