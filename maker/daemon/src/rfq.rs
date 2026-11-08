//! The maker's side of RFQ. Everything arriving here is attacker-reachable, so it is parsed strictly, rate limited, and
//! can only register a swap or take part in its signing session: no message moves funds, and the engine and signer still
//! check every lock on-chain and every signature share.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use liq2sol_chain::esplora::Esplora;
use liq2sol_chain::liquid::usdt;
use liq2sol_maker_core::engine::{self, Terms};
use liq2sol_maker_core::store::Store;
use liq2sol_maker_core::LockPoint;
use liq2sol_rfq::messages::*;
use liq2sol_rfq::relay::{Relay, RelayEvent};
use liq2sol_rfq::rules::{self, MakerRules, RateLimit};
use liq2sol_rfq::wrap;
use liq2sol_signer::{Call, Client, Reply, SwapTerms};
use nostr::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{accept, Details};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RfqConfig {
    /// The maker's identity. Not a funds key: losing it costs reputation, not money.
    pub nostr_key: PathBuf,
    pub relays: Vec<String>,
    pub network: String,
    pub min_sol_units: u64,
    pub max_sol_units: u64,
    /// The most to admit to in an offer, for a maker who doesn't want to show its full capacity. It also limits what is quoted.
    #[serde(default)]
    pub show_available_up_to: Option<u64>,
    /// What we charge in each direction, by size. `[{"up_to":"5000000","bps":30},{"up_to":"25000000","bps":40}]`
    /// charges 30 bps on swaps up to 5 USDT, and 40 above. The last step must reach `max_sol_units`.
    pub l2s_prices: Vec<PriceTier>,
    pub s2l_prices: Vec<PriceTier>,
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
    our_solana: String,
    our_initiator: String,
    fee_coin_sats: u64,
    engine: engine::Config,
    rate: RateLimit,
    negotiations: HashMap<String, Negotiation>,
    seen_events: HashSet<String>,
    recent: Vec<u64>,
    /// (Solana, Liquid) that we hold, in SOL-USDT base units.
    balances: Arc<Mutex<(u64, u64)>>,
    sol: liq2sol_chain::sol::Sol,
}

/// Often enough for an offer, and rarely enough for a public API's rate limit.
const BALANCES_EVERY: Duration = Duration::from_secs(120);

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
        if !rules::prices_ok(&cfg.l2s_prices, cfg.max_sol_units)
            || !rules::prices_ok(&cfg.s2l_prices, cfg.max_sol_units)
        {
            return Err("refusing to start: the prices must rise in steps and reach max_sol_units".into());
        }
        let keys = load_keys(&cfg.nostr_key)?;
        let store = Store::open(&maker.db).map_err(|e| e.to_string())?;
        let chain = crate::source(&maker.liquid);
        let mut signer = Client::connect(&maker.signer_socket)?;
        let Reply::Identity { solana, escrow_initiator, fee_coin_sats, .. } = signer.call(&Call::Identity)? else {
            return Err("unexpected reply from the signer".into());
        };
        Ok(Self {
            cfg,
            keys,
            store,
            chain,
            signer,
            our_solana: solana,
            our_initiator: escrow_initiator,
            fee_coin_sats,
            engine: maker.engine.clone(),
            rate: RateLimit::default(),
            negotiations: HashMap::new(),
            seen_events: HashSet::new(),
            recent: vec![],
            balances: Arc::default(),
            sol: liq2sol_chain::sol::Sol::new(&maker.solana_rpc),
        })
    }

    /// What we can pay in each direction, less what open swaps are counting on. The balances come from a background reader,
    /// because reading them takes many chain calls, and a taker asking for a quote shouldn't wait on that. Nothing read yet, or
    /// a failed read, shows nothing available.
    fn available(&mut self) -> (u64, u64) {
        let (solana, liquid) = self.balances.lock().map_or((0, 0), |b| *b);
        let committed = |direction| -> u64 {
            let open = self.store.open_swaps().unwrap_or_default();
            open.iter()
                .filter(|(t, s)| t.direction == direction && *s == engine::State::WatchLock)
                .map(|(t, _)| t.solana_amount)
                .sum()
        };
        let cap = self.cfg.show_available_up_to.unwrap_or(u64::MAX);
        let l2s = solana.saturating_sub(committed(engine::Direction::L2S)).min(cap);
        let s2l = liquid.saturating_sub(committed(engine::Direction::S2L)).min(cap);
        (l2s, s2l)
    }

    fn rules(&mut self) -> MakerRules {
        let (available_l2s, available_s2l) = self.available();
        MakerRules {
            available_l2s,
            available_s2l,
            network: self.cfg.network.clone(),
            min_sol_units: self.cfg.min_sol_units,
            max_sol_units: self.cfg.max_sol_units,
            l2s_prices: self.cfg.l2s_prices.clone(),
            s2l_prices: self.cfg.s2l_prices.clone(),
            quote_secs: self.cfg.quote_secs,
            taker_lock_window: self.engine.l2s_fund_window_blocks,
            l2s_lock_blocks: self.cfg.l2s_lock_blocks,
            s2l_lock_blocks: self.cfg.s2l_lock_blocks,
            min_deadline_slots: self.cfg.min_deadline_slots,
        }
    }

    pub fn offer(&mut self, now: u64) -> Result<Event, String> {
        let (available_l2s, available_s2l) = self.available();
        let offer = Offer {
            available_l2s: Units(available_l2s),
            available_s2l: Units(available_s2l),
            v: VERSION,
            network: self.cfg.network.clone(),
            program_id: solana_escrow::id().to_string(),
            template_version: TEMPLATE_VERSION,
            directions: vec![Direction::L2S, Direction::S2L],
            min_sol_units: Units(self.cfg.min_sol_units),
            max_sol_units: Units(self.cfg.max_sol_units),
            l2s_prices: self.cfg.l2s_prices.clone(),
            s2l_prices: self.cfg.s2l_prices.clone(),
            fee_coin_sats: self.fee_coin_sats,
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
            Body::ClaimAnswer(a) => self.on_claim_answer(&got.sender, &id, a),
            Body::EarlyRefundAsk(a) => self.on_early_refund(&got.sender, &id, a, now),
            Body::ReleaseAsk(a) => self.on_release(&got.sender, &id, a, now),
            Body::SessionOpen(o) => self.on_session_open(&got.sender, &id, o, now),
            Body::TakerShare(s) => self.on_taker_share(&got.sender, &id, s, now),
            // A taker has no business sending these, and a reject has nothing to free up in this version.
            Body::RfqQuote(_)
            | Body::RfqTerms(_)
            | Body::RfqReject(_)
            | Body::ClaimOffer(_)
            | Body::SessionNonce(_)
            | Body::MakerShare(_)
            | Body::EarlyRefundShare(_) => None,
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
        let rules = self.rules();
        // Say so, or the taker can only time out and can't tell a refusal from a maker that is gone.
        let Ok(quote) = rules::make_quote(&rules, &request, now) else {
            let body = Body::RfqReject(Reject { reason: "this maker can't quote that amount right now".into() });
            return self.reply(sender, id, body, now).ok().map(|(event, _)| event);
        };
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
        let checked = rules::check_accept(&acc, &n.quote, &n.quote_rumor_id, now).ok()?;
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

        let lock_point = LockPoint(c.lock_point);
        let Reply::SwapKey { key } = self.signer.call(&Call::SwapKey { swap_id: lock_point.id() })? else {
            return Err("unexpected reply from the signer".into());
        };
        let swap = SwapTerms {
            direction: match direction {
                Direction::L2S => engine::Direction::L2S,
                Direction::S2L => engine::Direction::S2L,
            },
            lock_point,
            taker_key: LockPoint(c.taker_key),
            blinding_key: c.blinding_key,
            lock_height,
            liquid_amount,
            solana_amount,
            taker_solana: c.taker_solana,
        };
        // A point off the curve, a bad key or a bad blinding key is refused here, and not when funds depend on it.
        liq2sol_swapkey::solana_point_of(&lock_point.0).map_err(|e| e.to_string())?;
        swap.address(&key)?;
        let details = Details { terms: swap.clone(), taker_nostr: sender.to_hex(), negotiation: id.to_string() };
        let terms =
            Terms { direction: swap.direction, lock_point, liquid_amount, solana_amount, h_accept: tip, lock_height };
        // The forever check: a point ever seen before is refused, because its secret may be public.
        if !accept(&self.store, &terms, &details)? {
            return Err("this lock point has been used before".into());
        }
        let is_l2s = direction == Direction::L2S;
        let body = Body::RfqTerms(MakerTerms {
            accept_event: accept_rumor_id.to_string(),
            h_accept: tip,
            lock_height,
            maker_key: key.to_hex(),
            maker_solana: self.our_solana.clone(),
            maker_initiator: is_l2s.then(|| self.our_initiator.clone()),
            usdt_asset: usdt().to_string(),
            program_id: solana_escrow::id().to_string(),
            template_version: TEMPLATE_VERSION,
            liquid_amount: Units(liquid_amount),
            solana_amount: Units(solana_amount),
            min_deadline_slots: self.cfg.min_deadline_slots,
        });
        self.reply(sender, id, body, now).map(|(event, _)| event)
    }

    /// The swap a session message belongs to. Only that swap's taker, in that swap's negotiation, gets anywhere, and
    /// nobody does while the rollback alarm is on: what we believe about the chain can't be trusted then.
    fn swap_for(&self, sender: &PublicKey, id: &str, lock_point: &str) -> Option<SwapTerms> {
        if self.store.rollback().unwrap_or(true) {
            return None;
        }
        let point = LockPoint::from_hex(lock_point)?;
        let body = self.store.details(&point.id()).ok()??;
        let details: Details = serde_json::from_str(&body).ok()?;
        (details.taker_nostr == sender.to_hex() && details.negotiation == id && details.terms.lock_point == point)
            .then_some(details.terms)
    }

    /// L→S. Nothing goes back: the engine sees the stored signature on its next tick, and locks.
    fn on_claim_answer(&mut self, sender: &PublicKey, id: &str, a: ClaimAnswer) -> Option<Event> {
        let terms = self.swap_for(sender, id, &a.lock_point)?;
        let call = Call::StoreAdaptor { terms, taker_nonce: a.nonce, taker_share: a.share };
        match self.signer.call(&call) {
            Ok(Reply::Held(true)) => println!("{} adapter signature stored", a.lock_point),
            other => eprintln!("rfq: claim answer refused: {other:?}"),
        }
        None
    }

    /// L→S. The signer decides, and if it agrees it has already promised never to lock.
    fn on_early_refund(&mut self, sender: &PublicKey, id: &str, a: EarlyRefundAsk, now: u64) -> Option<Event> {
        let terms = self.swap_for(sender, id, &a.lock_point)?;
        let swap = terms.lock_point.id();
        let call = Call::EarlyRefund { terms, tx: a.tx, taker_nonce: a.nonce };
        let body = match self.signer.call(&call) {
            Ok(Reply::EarlyRefundShare { nonce, share }) => {
                if let Err(e) = self.store.commit_step(&swap, &engine::State::Abandoned, &[]) {
                    eprintln!("rfq: couldn't mark {} abandoned: {e}", a.lock_point);
                }
                println!("{} refunded early, at the taker's request", a.lock_point);
                Body::EarlyRefundShare(EarlyRefundShare { lock_point: a.lock_point, nonce, share })
            }
            other => {
                eprintln!("rfq: early refund refused: {other:?}");
                Body::RfqReject(Reject { reason: "this maker can't refund early now".into() })
            }
        };
        self.reply(sender, id, body, now).ok().map(|(e, _)| e)
    }

    /// S→L. The signer refuses once we have locked on Liquid, and promises never to afterwards. The program sends the tokens
    /// back to the taker's own account, so nothing here can redirect them. Asking again sends the release again.
    fn on_release(&mut self, sender: &PublicKey, id: &str, a: ReleaseAsk, now: u64) -> Option<Event> {
        let terms = self.swap_for(sender, id, &a.lock_point)?;
        if terms.direction != engine::Direction::S2L {
            return None;
        }
        let swap = terms.lock_point.id();
        let call = Call::Release { taker_solana: terms.taker_solana.clone(), lock_point: terms.lock_point };
        let sent = match self.signer.call(&call) {
            Ok(Reply::Solana(hex)) => liq2sol_signer::unhex(&hex)
                .and_then(|raw| bincode::deserialize::<solana_sdk::transaction::Transaction>(&raw).map_err(|e| e.to_string()))
                .and_then(|tx| self.sol.send_signed(&tx).map(|_| ())),
            other => Err(format!("{other:?}")),
        };
        match sent {
            Ok(()) => {
                if let Err(e) = self.store.commit_step(&swap, &engine::State::Abandoned, &[]) {
                    eprintln!("rfq: couldn't mark {} abandoned: {e}", a.lock_point);
                }
                println!("{} released to the taker, at the taker's request", a.lock_point);
                None
            }
            Err(e) => {
                eprintln!("rfq: release refused: {}", e.chars().take(200).collect::<String>());
                let body = Body::RfqReject(Reject { reason: "this maker can't refund early now".into() });
                self.reply(sender, id, body, now).ok().map(|(e, _)| e)
            }
        }
    }

    fn on_session_open(&mut self, sender: &PublicKey, id: &str, o: SessionOpen, now: u64) -> Option<Event> {
        let terms = self.swap_for(sender, id, &o.lock_point)?;
        let call = Call::OpenSession { terms, claim_tx: o.tx, taker_nonce: o.nonce };
        match self.signer.call(&call) {
            Ok(Reply::Nonce(nonce)) => {
                let body = Body::SessionNonce(SessionNonce { lock_point: o.lock_point, nonce });
                self.reply(sender, id, body, now).ok().map(|(e, _)| e)
            }
            other => {
                eprintln!("rfq: signing session refused: {other:?}");
                None
            }
        }
    }

    fn on_taker_share(&mut self, sender: &PublicKey, id: &str, s: SessionShare, now: u64) -> Option<Event> {
        let terms = self.swap_for(sender, id, &s.lock_point)?;
        match self.signer.call(&Call::FinishSession { terms, taker_share: s.share }) {
            Ok(Reply::Share(share)) => {
                let body = Body::MakerShare(SessionShare { lock_point: s.lock_point, share });
                self.reply(sender, id, body, now).ok().map(|(e, _)| e)
            }
            other => {
                eprintln!("rfq: taker's share refused: {other:?}");
                None
            }
        }
    }
}

/// How the main loop reaches a taker. Everything goes out through every relay.
#[derive(Clone)]
pub struct Outbox {
    keys: Keys,
    network: String,
    relays: Vec<Sender<Event>>,
}

impl Outbox {
    pub fn send(&self, to_hex: &str, id: &str, body: Body) -> Result<(), String> {
        let to = PublicKey::from_hex(to_hex).map_err(|e| e.to_string())?;
        let now = now_secs();
        let envelope = Envelope { v: VERSION, id: id.to_string(), network: self.network.clone(), sent_at: now, body };
        let (event, _) = wrap::wrap(&self.keys, &to, &envelope, now).map_err(|e| format!("{e:?}"))?;
        for relay in &self.relays {
            // A relay thread only goes away with the process, so a failure here means nothing is listening anyway.
            let _ = relay.send(event.clone());
        }
        Ok(())
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
fn run_relay(
    url: String,
    handler: Arc<Mutex<Handler>>,
    pubkey: PublicKey,
    offer_every: Duration,
    outgoing: Receiver<Event>,
) {
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
        // Whatever piled up during an outage is stale, and the engine will ask again.
        while outgoing.try_recv().is_ok() {}
        'connected: loop {
            while let Ok(event) = outgoing.try_recv() {
                if let Err(e) = relay.publish(&event) {
                    eprintln!("rfq: {url}: {e}");
                    break 'connected;
                }
            }
            if last_offer.is_none_or(|t| t.elapsed() >= offer_every) {
                let offer = handler.lock().map_err(|_| "poisoned".to_string()).and_then(|mut h| h.offer(now_secs()));
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

/// One read of what we hold. A failure reads as nothing, so we don't quote on stale funds.
fn read_balances(signer: &mut Client, balances: &Mutex<(u64, u64)>) {
    let read = match signer.call(&Call::Capacity) {
        Ok(Reply::Capacity { solana_units, liquid_units }) => (solana_units, liquid_units / 100),
        other => {
            eprintln!("rfq: could not read our balances: {other:?}");
            (0, 0)
        }
    };
    if let Ok(mut b) = balances.lock() {
        *b = read;
    }
}

/// Start answering on every configured relay.
pub fn start(cfg: RfqConfig, maker: &crate::Config) -> Result<(PublicKey, Outbox), String> {
    let relays = cfg.relays.clone();
    let every = Duration::from_secs(cfg.offer_every_secs);
    let handler = Handler::new(cfg, maker)?;
    let pubkey = handler.keys.public_key();
    let mut outbox = Outbox { keys: handler.keys.clone(), network: handler.cfg.network.clone(), relays: vec![] };
    // Once before any relay starts, so the first offer is right, and then in the background, on a connection of its own.
    let mut reader = Client::connect(&maker.signer_socket)?;
    let balances = handler.balances.clone();
    read_balances(&mut reader, &balances);
    std::thread::spawn(move || loop {
        std::thread::sleep(BALANCES_EVERY);
        read_balances(&mut reader, &balances);
    });
    let handler = Arc::new(Mutex::new(handler));
    for url in relays {
        let h = handler.clone();
        let (send, receive) = channel();
        outbox.relays.push(send);
        std::thread::spawn(move || run_relay(url, h, pubkey, every, receive));
    }
    Ok((pubkey, outbox))
}
