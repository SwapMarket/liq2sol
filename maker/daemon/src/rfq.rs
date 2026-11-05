//! The maker's side of RFQ. Everything arriving here is attacker-reachable, so it is parsed strictly, rate limited, and
//! can only ever register a swap: no message moves funds, and the engine and signer still check every lock on-chain.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use liq2sol_chain::liquid::{usdt, Esplora};
use liq2sol_maker_core::engine::{self, Terms};
use liq2sol_maker_core::store::Store;
use liq2sol_rfq::messages::{self as msg, *};
use liq2sol_rfq::relay::{Relay, RelayEvent};
use liq2sol_rfq::rules::{self, MakerRules, RateLimit};
use liq2sol_rfq::wrap;
use liq2sol_signer::{Call, Client, L2sTerms, Reply, S2lTerms};
use nostr::prelude::*;
use serde::{Deserialize, Serialize};
use simplicityhl::elements::Script;

use crate::{accept, Details};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RfqConfig {
    /// The maker's identity. Not a funds key: losing it costs reputation, not money.
    pub nostr_key: PathBuf,
    pub relays: Vec<String>,
    pub network: String,
    pub min_sol_units: u64,
    pub max_sol_units: u64,
    pub l2s_bps: u32,
    pub s2l_bps: u32,
    pub quote_secs: u64,
    pub l2s_lock_blocks: u32,
    pub s2l_lock_blocks: u32,
    pub min_deadline_slots: u64,
    pub offer_every_secs: u64,
    pub offer_ttl_secs: u64,
    pub max_requests_per_sender: usize,
    pub max_requests_per_window: usize,
    pub window_secs: u64,
    pub max_negotiations: usize,
}

struct Negotiation {
    sender: PublicKey,
    direction: Direction,
    quote: Quote,
    quote_rumor_id: String,
    answered: Option<(String, Event)>,
}

pub struct Handler {
    cfg: RfqConfig,
    keys: Keys,
    store: Store,
    chain: Esplora,
    signer: Client,
    our_script: Script,
    our_solana: String,
    our_initiator: String,
    engine: engine::Config,
    rate: RateLimit,
    negotiations: HashMap<String, Negotiation>,
    seen_events: HashSet<String>,
    recent: Vec<u64>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Load the identity key, creating it on first use with private permissions.
pub fn load_keys(path: &std::path::Path) -> Result<Keys, String> {
    use std::os::unix::fs::PermissionsExt;
    if !path.exists() {
        let keys = Keys::generate();
        std::fs::write(path, keys.secret_key().to_secret_hex()).map_err(|e| e.to_string())?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    Keys::parse(text.trim()).map_err(|e| e.to_string())
}

impl Handler {
    pub fn new(cfg: RfqConfig, maker: &crate::Config) -> Result<Self, String> {
        let keys = load_keys(&cfg.nostr_key)?;
        let store = Store::open(&maker.db).map_err(|e| e.to_string())?;
        let chain = crate::source(&maker.liquid);
        let mut signer = Client::connect(&maker.signer_socket)?;
        let Reply::Identity { liquid_script, solana, escrow_initiator, .. } = signer.call(&Call::Identity)? else {
            return Err("unexpected reply from the signer".into());
        };
        Ok(Self {
            cfg,
            keys,
            store,
            chain,
            signer,
            our_script: Script::from(liquid_script),
            our_solana: solana,
            our_initiator: escrow_initiator,
            engine: maker.engine.clone(),
            rate: RateLimit::default(),
            negotiations: HashMap::new(),
            seen_events: HashSet::new(),
            recent: vec![],
        })
    }

    fn rules(&self) -> MakerRules {
        MakerRules {
            network: self.cfg.network.clone(),
            min_sol_units: self.cfg.min_sol_units,
            max_sol_units: self.cfg.max_sol_units,
            l2s_bps: self.cfg.l2s_bps,
            s2l_bps: self.cfg.s2l_bps,
            quote_secs: self.cfg.quote_secs,
            taker_lock_window: self.engine.l2s_fund_window_blocks,
            l2s_lock_blocks: self.cfg.l2s_lock_blocks,
            s2l_lock_blocks: self.cfg.s2l_lock_blocks,
            min_deadline_slots: self.cfg.min_deadline_slots,
        }
    }

    pub fn offer(&self, now: u64) -> Result<Event, String> {
        let offer = Offer {
            v: VERSION,
            network: self.cfg.network.clone(),
            program_id: solana_escrow::id().to_string(),
            template_version: 1,
            directions: vec![Direction::L2S, Direction::S2L],
            min_sol_units: Units(self.cfg.min_sol_units),
            max_sol_units: Units(self.cfg.max_sol_units),
            l2s_bps: self.cfg.l2s_bps,
            s2l_bps: self.cfg.s2l_bps,
            fee_coin_sats: liq2sol_chain::liquid::FEE,
            confirmations: self.engine.confirmations,
            relays: self.cfg.relays.clone(),
        };
        wrap::offer_event(&self.keys, &offer, now, self.cfg.offer_ttl_secs).map_err(|e| format!("{e:?}"))
    }

    fn reply(&self, to: &PublicKey, id: &str, body: Body, now: u64) -> Result<(Event, String), String> {
        let envelope =
            Envelope { v: VERSION, id: id.to_string(), network: self.cfg.network.clone(), sent_at: now, body };
        wrap::wrap(&self.keys, to, &envelope, now).map_err(|e| format!("{e:?}"))
    }

    /// Everything else in this file serves this: turn one event into at most one answer.
    pub fn on_event(&mut self, event: &Event, now: u64) -> Option<Event> {
        // The cheapest check first, so a flood of copies costs almost nothing.
        if !self.seen_events.insert(event.id.to_hex()) {
            return None;
        }
        if self.seen_events.len() > 100_000 {
            self.seen_events.clear();
        }
        let got = wrap::unwrap(&self.keys, event).ok()?;
        rules::check_envelope(&got.envelope, &self.cfg.network, now).ok()?;
        let sender = got.sender.to_hex();
        if !self.rate.allow(&sender, now, self.cfg.window_secs, self.cfg.max_requests_per_sender) {
            return None;
        }
        self.recent.retain(|t| now.saturating_sub(*t) < self.cfg.window_secs);
        if self.recent.len() >= self.cfg.max_requests_per_window {
            return None;
        }
        self.recent.push(now);
        let id = got.envelope.id.clone();
        match got.envelope.body {
            Body::RfqRequest(request) => self.on_request(&got.sender, &id, request, now),
            Body::RfqAccept(acc) => self.on_accept(&got.sender, &id, &got.rumor_id, acc, now),
            // A taker has no business sending these, and a reject has nothing to free up in this version.
            Body::RfqQuote(_) | Body::RfqTerms(_) | Body::RfqReject(_) => None,
        }
    }

    fn on_request(&mut self, sender: &PublicKey, id: &str, request: Request, now: u64) -> Option<Event> {
        if self.negotiations.contains_key(id) {
            return None;
        }
        self.negotiations.retain(|_, n| now <= n.quote.quote_expiry + 600);
        if self.negotiations.len() >= self.cfg.max_negotiations {
            return None;
        }
        let quote = rules::make_quote(&self.rules(), &request, now).ok()?;
        let (event, quote_rumor_id) = self.reply(sender, id, Body::RfqQuote(quote.clone()), now).ok()?;
        self.negotiations.insert(
            id.to_string(),
            Negotiation { sender: *sender, direction: request.direction, quote, quote_rumor_id, answered: None },
        );
        Some(event)
    }

    fn on_accept(&mut self, sender: &PublicKey, id: &str, rumor_id: &str, acc: Accept, now: u64) -> Option<Event> {
        let n = self.negotiations.get(id)?;
        // Only the key that asked may accept, so nobody can hijack a quote.
        if n.sender != *sender {
            return None;
        }
        // The same accept delivered again gets the same answer, in case the first was lost.
        if let Some((answered_id, event)) = &n.answered {
            return (answered_id == rumor_id).then(|| event.clone());
        }
        let checked = rules::check_accept(&acc, n.direction, &n.quote, &n.quote_rumor_id, now).ok()?;
        let (direction, quote) = (n.direction, n.quote.clone());
        match self.terms_for(sender, id, rumor_id, direction, &quote, checked, now) {
            Ok(event) => {
                if let Some(n) = self.negotiations.get_mut(id) {
                    n.answered = Some((rumor_id.to_string(), event.clone()));
                }
                Some(event)
            }
            Err(reason) => {
                eprintln!("rfq: accept refused: {}", reason.chars().take(200).collect::<String>());
                let body = Body::RfqReject(Reject { reason: "this swap can't be accepted".into() });
                self.reply(sender, id, body, now).ok().map(|(e, _)| e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn terms_for(
        &mut self,
        sender: &PublicKey,
        id: &str,
        accept_rumor_id: &str,
        direction: Direction,
        quote: &Quote,
        c: rules::CheckedAccept,
        now: u64,
    ) -> Result<Event, String> {
        let tip = self.chain.tip()?;
        let (send, receive) = (quote.send.0, quote.receive.0);
        let (liquid_amount, solana_amount, lock_blocks) = match direction {
            Direction::L2S => (send.checked_mul(100), receive, self.cfg.l2s_lock_blocks),
            Direction::S2L => (receive.checked_mul(100), send, self.cfg.s2l_lock_blocks),
        };
        let liquid_amount = liquid_amount.ok_or("amount overflow")?;
        let lock_height = tip.checked_add(lock_blocks).ok_or("height overflow")?;
        // A bad address is refused here, and not when it would later fail on-chain.
        Pubkey58::check(&c.taker_solana)?;

        let Reply::SwapKey { x_only } = self.signer.call(&Call::SwapKey { payment_hash: c.payment_hash })? else {
            return Err("unexpected reply from the signer".into());
        };
        let details = match direction {
            Direction::L2S => Details::L2S(L2sTerms {
                payment_hash: c.payment_hash,
                taker_pk: c.taker_liquid_pk,
                lock_height,
                liquid_amount,
                solana_amount,
                taker_solana: c.taker_solana,
            }),
            Direction::S2L => Details::S2L(S2lTerms {
                payment_hash: c.payment_hash,
                taker_pk: c.taker_liquid_pk,
                taker_script: c.taker_liquid_script.ok_or("no taker script")?,
                taker_fee_script: c.taker_fee_script.ok_or("no fee script")?,
                lock_height,
                liquid_amount,
                taker_solana: c.taker_solana,
            }),
        };
        let terms = Terms {
            direction: match direction {
                Direction::L2S => engine::Direction::L2S,
                Direction::S2L => engine::Direction::S2L,
            },
            payment_hash: c.payment_hash,
            liquid_amount,
            solana_amount,
            h_accept: tip,
            lock_height,
        };
        // The forever check: a payment hash ever seen before is refused, because its preimage may be public.
        if !accept(&self.store, &terms, &details)? {
            return Err("this payment hash has been used before".into());
        }
        let is_l2s = direction == Direction::L2S;
        let body = Body::RfqTerms(MakerTerms {
            accept_event: accept_rumor_id.to_string(),
            h_accept: tip,
            lock_height,
            maker_liquid_pk: msg::hex(&x_only),
            maker_liquid_script: is_l2s.then(|| msg::hex(self.our_script.as_bytes())),
            maker_solana: self.our_solana.clone(),
            maker_initiator: is_l2s.then(|| self.our_initiator.clone()),
            usdt_asset: usdt().to_string(),
            program_id: solana_escrow::id().to_string(),
            template_version: 1,
            liquid_amount: Units(liquid_amount),
            solana_amount: Units(solana_amount),
            min_deadline_slots: self.cfg.min_deadline_slots,
        });
        self.reply(sender, id, body, now).map(|(event, _)| event)
    }
}

struct Pubkey58;
impl Pubkey58 {
    fn check(s: &str) -> Result<(), String> {
        anchor_lang::prelude::Pubkey::from_str(s).map(|_| ()).map_err(|e| format!("bad Solana address: {e}"))
    }
}

/// One relay, forever: reconnect on failure, publish offers, and answer what arrives. A relay that goes away costs
/// quotes on that relay, and nothing else.
fn run_relay(url: String, handler: Arc<Mutex<Handler>>, pubkey: PublicKey, offer_every: Duration) {
    loop {
        let mut relay = match Relay::connect(&url, Duration::from_secs(1)) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("rfq: {e}");
                std::thread::sleep(Duration::from_secs(10));
                continue;
            }
        };
        // Wrappers carry randomized timestamps up to two days old, so a shorter window would lose messages.
        let since = now_secs().saturating_sub(2 * 86_400);
        if let Err(e) = relay.subscribe("liq2sol-maker", &[Kind::from(WRAP_KIND)], Some(&pubkey), since) {
            eprintln!("rfq: {url}: {e}");
            continue;
        }
        let mut last_offer: Option<Instant> = None;
        loop {
            if last_offer.is_none_or(|t| t.elapsed() >= offer_every) {
                let offer = handler.lock().map_err(|_| "poisoned".to_string()).and_then(|h| h.offer(now_secs()));
                match offer.and_then(|o| relay.publish(&o)) {
                    Ok(()) => last_offer = Some(Instant::now()),
                    Err(e) => {
                        eprintln!("rfq: {url}: {e}");
                        break;
                    }
                }
            }
            match relay.next_event() {
                Ok(Some(RelayEvent::Event(event))) => {
                    let answer = handler.lock().ok().and_then(|mut h| h.on_event(&event, now_secs()));
                    if let Some(answer) = answer {
                        if let Err(e) = relay.publish(&answer) {
                            eprintln!("rfq: {url}: {e}");
                            break;
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("rfq: {url}: {e}");
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// Start answering on every configured relay.
pub fn start(cfg: RfqConfig, maker: &crate::Config) -> Result<PublicKey, String> {
    let relays = cfg.relays.clone();
    let every = Duration::from_secs(cfg.offer_every_secs);
    let handler = Handler::new(cfg, maker)?;
    let pubkey = handler.keys.public_key();
    let handler = Arc::new(Mutex::new(handler));
    for url in relays {
        let h = handler.clone();
        std::thread::spawn(move || run_relay(url, h, pubkey, every));
    }
    Ok(pubkey)
}
