//! The maker's main loop. See docs/maker.md.
//!
//! Each tick: read both chains into a `View`, ask the engine, durably record the new state and the intended actions,
//! then carry them out through the signer. It holds no keys. After a crash it simply starts ticking again, because the
//! chains and the store say everything that is needed, and every action is safe to repeat.

#![forbid(unsafe_code)]

pub mod rfq;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use anchor_lang::prelude::Pubkey;
use liq2sol_chain::esplora::{Esplora, Rpc};
use liq2sol_chain::liquid::{
    exact_secret_coin, is_key_path_spend, published_signature, usdt, use_network, NetworkConfig, LIQUID_MAINNET_GENESIS,
};
use liq2sol_chain::sol::{self, ata, escrow_pda, Settled, Sol, MAINNET_GENESIS};
use liq2sol_maker_core::engine::{
    self, Absence, Action, Closed, Direction, EscrowState, EscrowView, HtlcView, Outcome, Page, State, Terms, View,
};
use liq2sol_maker_core::store::Store;
use liq2sol_maker_core::LockPoint;
use liq2sol_rfq::messages::{Body, ClaimOffer};
use liq2sol_signer::{unhex, Call, Client, LiquidSource, Reply, SwapTerms};
use liq2sol_swapkey::solana_point_of;
use serde::{Deserialize, Serialize};
use simplicityhl::elements::encode::deserialize;
use simplicityhl::elements::{AddressParams, AssetId, OutPoint, Transaction as LiquidTx, Txid};
use solana_sdk::transaction::Transaction as SolanaTx;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub db: PathBuf,
    pub signer_socket: PathBuf,
    /// Pages are appended here. Something outside the daemon must watch this file.
    pub pages: PathBuf,
    pub engine: engine::Config,
    pub liquid: LiquidSource,
    /// An independent Liquid source. A lock isn't acted on unless both agree.
    pub second_liquid: Option<LiquidSource>,
    pub solana_rpc: String,
    pub tick_millis: u64,
    /// Don't repeat the same action for one swap sooner than this.
    pub retry_secs: u64,
    pub regtest_assets: Option<(String, String)>,
    /// Quotes over Nostr. Without this, swaps only enter through `accept`.
    pub rfq: Option<rfq::RfqConfig>,
    /// How often to look for settled escrows to close for their rent.
    #[serde(default = "default_close_every")]
    pub close_every_secs: u64,
}

/// The full terms of one swap, and how to reach its taker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Details {
    pub terms: SwapTerms,
    /// The taker's Nostr key. Only this key can take part in the swap's signing session.
    pub taker_nostr: String,
    /// The negotiation id, repeated on every message of the swap.
    pub negotiation: String,
}

/// Register a swap for the loop to pick up, refusing a point ever seen before.
pub fn accept(store: &Store, terms: &Terms, details: &Details) -> Result<bool, String> {
    let added = store.accept(terms, &State::WatchLock).map_err(|e| e.to_string())?;
    if added {
        let body = serde_json::to_string(details).map_err(|e| e.to_string())?;
        store.set_details(&terms.swap_id(), &body).map_err(|e| e.to_string())?;
    }
    Ok(added)
}

/// Solana's base fee. The priority fee is read from each transaction. Rent on a lock's accounts comes back, so it isn't a cost.
const LAMPORTS_PER_SIGNATURE: u64 = 5_000;

/// Where a top-up's cost is filed: under its transaction, since it belongs to no swap.
fn top_up_id(reply: &Reply) -> Result<[u8; 32], String> {
    let Reply::Liquid(text) = reply else {
        return Err(format!("signer: {reply:?}"));
    };
    let tx: LiquidTx = deserialize(&unhex(text)?).map_err(|e| e.to_string())?;
    unhex(&tx.txid().to_string())?.try_into().map_err(|_| "a txid of the wrong length".to_string())
}

struct Identity {
    solana: Pubkey,
    initiator: Pubkey,
    vault: bool,
    fee_coin_sats: u64,
}

pub struct Maker {
    cfg: Config,
    store: Store,
    chain: Esplora,
    second: Option<Esplora>,
    sol: Sol,
    signer: Client,
    identity: Identity,
    last_slot: (u64, Instant),
    last_sent: HashMap<([u8; 32], String), Instant>,
    last_paged: HashMap<([u8; 32], String), Instant>,
    last_top_up: Option<Instant>,
    last_close: Option<Instant>,
    /// The way to takers. Without it no L→S swap can get its adapter signature, so none will lock.
    pub outbox: Option<rfq::Outbox>,
}

fn default_close_every() -> u64 {
    600
}

/// How often to look at the fee coin pool. It is several chain reads, and a pool lasts many swaps.
const TOP_UP_EVERY: Duration = Duration::from_secs(60);

fn source(s: &LiquidSource) -> Esplora {
    match s {
        LiquidSource::Esplora(url) => Esplora::Http(url.clone()),
        LiquidSource::Node { cli, args } => Esplora::Node(Rpc { cli: cli.clone(), args: args.clone() }),
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn action_name(a: &Action) -> String {
    format!("{a:?}").split([' ', '(']).next().unwrap_or("").to_string()
}

impl Maker {
    pub fn new(cfg: Config) -> Result<Self, String> {
        let chain = source(&cfg.liquid);
        let second = cfg.second_liquid.as_ref().map(source);
        let sol = Sol::new(&cfg.solana_rpc);
        if chain.genesis()? == LIQUID_MAINNET_GENESIS || sol.genesis()? == MAINNET_GENESIS {
            return Err("refusing to start: this is mainnet".into());
        }
        if let Some((usdt_id, lbtc_id)) = &cfg.regtest_assets {
            use_network(NetworkConfig {
                params: &AddressParams::ELEMENTS,
                usdt: AssetId::from_str(usdt_id).map_err(|e| e.to_string())?,
                lbtc: AssetId::from_str(lbtc_id).map_err(|e| e.to_string())?,
            });
        }
        let store = Store::open(&cfg.db).map_err(|e| e.to_string())?;
        let mut signer = Client::connect(&cfg.signer_socket)?;
        let Reply::Identity { solana, escrow_initiator, fee_coin_sats, .. } = signer.call(&Call::Identity)? else {
            return Err("unexpected reply from the signer".into());
        };
        let parse = |s: &str| Pubkey::from_str(s).map_err(|e| e.to_string());
        let (solana, initiator) = (parse(&solana)?, parse(&escrow_initiator)?);
        let identity = Identity { solana, initiator, vault: initiator != solana, fee_coin_sats };
        // Whatever was in flight at a crash is settled by reading the chains, so old intents need nothing more.
        for (id, hash, action) in store.unfinished_intents().map_err(|e| e.to_string())? {
            println!("resuming: {} may have been in flight for {}", action_name(&action), hex(&hash));
            store.finish_intent(id).map_err(|e| e.to_string())?;
        }
        let slot = sol.slot()?;
        Ok(Self {
            cfg,
            store,
            chain,
            second,
            sol,
            signer,
            identity,
            last_slot: (slot, Instant::now()),
            last_sent: HashMap::new(),
            last_paged: HashMap::new(),
            last_top_up: None,
            last_close: None,
            outbox: None,
        })
    }

    fn page(&mut self, hash: &[u8; 32], page: &Page) {
        let key = (*hash, format!("{page:?}"));
        // Hold back repeats for ten minutes, but a new kind of page always goes out.
        if self.last_paged.get(&key).is_some_and(|t| t.elapsed() < Duration::from_secs(600)) {
            return;
        }
        self.last_paged.insert(key, Instant::now());
        let line = format!("{} PAGE {} {page:?}\n", now_secs(), hex(hash));
        eprint!("{line}");
        use std::io::Write;
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.cfg.pages)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = written {
            eprintln!("could not write the page file: {e}");
        }
    }

    /// Latch an alarm if Liquid's tip went down, or the block we last saw at a height has changed.
    fn check_rollback(&mut self, tip: u32) -> Result<bool, String> {
        let map = |e: liq2sol_maker_core::store::StoreError| e.to_string();
        if self.store.rollback().map_err(map)? {
            return Ok(true);
        }
        if let Some((height, hash)) = self.store.last_tip().map_err(map)? {
            let moved = tip < height || self.chain.block_hash(height)? != hash;
            if moved {
                self.store.set_rollback(true).map_err(map)?;
                self.page(&[0; 32], &Page::Rollback);
                return Ok(true);
            }
        }
        let hash = self.chain.block_hash(tip)?;
        self.store.set_last_tip(tip, &hash).map_err(map)?;
        Ok(false)
    }

    fn maker_key(&mut self, swap_id: [u8; 32]) -> Result<LockPoint, String> {
        let Reply::SwapKey { key } = self.signer.call(&Call::SwapKey { swap_id })? else {
            return Err("unexpected reply from the signer".into());
        };
        Ok(key)
    }

    /// Keep fee coins ready, so a swap never waits for one. Failing here only delays a swap, and only until the next try.
    fn top_up_fee_coins(&mut self) {
        if self.last_top_up.is_some_and(|t| t.elapsed() < TOP_UP_EVERY) {
            return;
        }
        self.last_top_up = Some(Instant::now());
        match self.signer.call(&Call::TopUpFeeCoins) {
            Ok(Reply::Nothing) => {}
            Ok(reply) => {
                let sent = top_up_id(&reply).and_then(|id| self.send_liquid(&id, "TopUp", reply));
                if let Err(e) = sent {
                    eprintln!("could not top up the fee coins: {}", e.chars().take(200).collect::<String>());
                }
            }
            Err(e) => eprintln!("could not top up the fee coins: {e}"),
        }
    }

    /// Everything open except this swap, in this direction: what a new lock would add to.
    fn open_exposure(&self, terms: &Terms) -> Result<u64, String> {
        let open = self.store.open_swaps().map_err(|e| e.to_string())?;
        Ok(open
            .iter()
            .filter(|(t, s)| {
                t.direction == terms.direction && t.lock_point != terms.lock_point && *s != State::WatchLock
            })
            .map(|(t, _)| t.solana_amount)
            .sum())
    }

    fn coin_view(&mut self, terms: &Terms, swap: &SwapTerms, tip: u32) -> Result<(HtlcView, bool), String> {
        let id = terms.swap_id();
        let key = self.maker_key(id)?;
        let address = swap.address(&key)?;
        let blinding = swap.blinding()?;
        let map = |e: liq2sol_maker_core::store::StoreError| e.to_string();
        let exact =
            exact_secret_coin(&self.chain.secret_coins(&address, &blinding)?, usdt(), swap.liquid_amount, tip, 0);
        if let Some(found) = exact {
            let outpoint = found.coin.outpoint;
            self.store.remember_coin(&id, &outpoint.txid.to_string(), outpoint.vout).map_err(map)?;
            let confirmations = found.coin.height.map_or(0, |h| (tip + 1).saturating_sub(h));
            let mut disagree = false;
            if let Some(second) = &self.second {
                // A second source we can't read counts as disagreeing. That stops new locks, and must never stop a claim.
                disagree = match (second.secret_coins(&address, &blinding), second.tip()) {
                    (Ok(coins), Ok(tip)) => exact_secret_coin(&coins, usdt(), swap.liquid_amount, tip, 0).is_none(),
                    _ => true,
                };
            }
            return Ok((HtlcView::Unspent { confirmations }, disagree));
        }
        // Our own lock's coin is known even before it confirms, in case it is spent before we ever see it unspent. The
        // swap is always the lock's first output.
        if terms.direction == Direction::S2L {
            if let Some(raw) = self.store.stored_lock_tx(&id).map_err(map)? {
                let tx: LiquidTx = deserialize(&raw).map_err(|e| e.to_string())?;
                self.store.remember_coin(&id, &tx.txid().to_string(), 0).map_err(map)?;
            }
        }
        let Some((txid, vout)) = self.store.seen_coin(&id).map_err(map)? else {
            return Ok((HtlcView::Absent, false));
        };
        let outpoint = OutPoint::new(Txid::from_str(&txid).map_err(|e| e.to_string())?, vout);
        let Some(spender) = self.chain.spender(&outpoint)? else {
            return Ok((HtlcView::Absent, false));
        };
        let confirmed = self.chain.is_confirmed(&spender.txid())?;
        if is_key_path_spend(&spender, &outpoint) != Some(true) {
            return Ok((HtlcView::SpentByRefund { confirmed }, false));
        }
        // Zero opens no point, so a signature we can't read pages a human instead of being trusted.
        let signature = published_signature(&spender, &outpoint).ok_or("a key-path spend with no signature")?;
        let call = Call::ExtractSecret { lock_point: terms.lock_point, signature: hex(&signature) };
        let secret = match self.signer.call(&call)? {
            Reply::Secret(secret) => secret,
            _ => [0; 32],
        };
        Ok((HtlcView::SpentByClaim { secret, confirmed }, false))
    }

    fn escrow_address(&self, swap: &SwapTerms) -> Result<Pubkey, String> {
        let point = solana_point_of(&swap.lock_point.0).map_err(|e| e.to_string())?;
        Ok(match swap.direction {
            Direction::L2S => escrow_pda(&self.identity.initiator, &point),
            Direction::S2L => escrow_pda(&Pubkey::from_str(&swap.taker_solana).map_err(|e| e.to_string())?, &point),
        })
    }

    /// The escrow, and while claiming, how it was settled if it is gone.
    fn escrow_view(&self, id: &[u8; 32], swap: &SwapTerms, claiming: bool) -> Result<(Option<EscrowView>, Option<Closed>), String> {
        let address = self.escrow_address(swap)?;
        let (read_at, escrow) = self.sol.escrow_at(&address)?;
        let Some(e) = escrow else {
            return Ok((None, if claiming { self.closed(id, &address, read_at)? } else { None }));
        };
        self.store.see_escrow(id, read_at, e.deadline_slot).map_err(|e| e.to_string())?;
        let recipient_ok = match swap.direction {
            Direction::L2S => Some(e.recipient) == Pubkey::from_str(&swap.taker_solana).ok(),
            Direction::S2L => e.recipient == self.identity.solana,
        };
        let point = solana_point_of(&swap.lock_point.0).map_err(|e| e.to_string())?;
        let restarted = e.close_on_restart && self.sol.last_restart_slot()? > e.created_slot;
        Ok((Some(EscrowView {
            state: match e.state {
                solana_escrow::state::EscrowState::Open => EscrowState::Open,
                solana_escrow::state::EscrowState::Claimed => EscrowState::Claimed,
                solana_escrow::state::EscrowState::Refunded => EscrowState::Refunded,
            },
            amount: e.amount,
            point_ok: e.lock_point == point,
            recipient_ok,
            deadline_slot: e.deadline_slot,
            close_on_restart: e.close_on_restart,
            extended: e.extended,
            restarted,
            revealed: e.revealed,
        }), None))
    }

    /// S→L: the taker pays the escrow's rent, so it may close the escrow as soon as it settles, before we read the claim.
    fn closed(&self, id: &[u8; 32], address: &Pubkey, read_at: u64) -> Result<Option<Closed>, String> {
        let seen = match engine::absence(self.store.escrow_seen(id).map_err(|e| e.to_string())?, read_at) {
            Absence::Behind => return Ok(None),
            Absence::BeforeDeadline => return Ok(Some(Closed::BeforeDeadline)),
            Absence::AskHistory { seen } => seen,
        };
        // A refund was possible, so only the escrow's history can tell.
        Ok(match self.sol.settled_by(address) {
            Ok(Some(Settled::Claimed)) => Some(Closed::Claimed),
            Ok(Some(Settled::Refunded)) => Some(Closed::Refunded),
            // We never release after locking, so this needs a human too.
            Ok(Some(Settled::Released)) => Some(Closed::Unknown),
            Ok(None) | Err(_) if seen => Some(Closed::Unknown),
            // Never seen here, perhaps from before this was recorded: it may not exist yet, so keep claiming.
            Ok(None) | Err(_) => None,
        })
    }

    fn view(
        &mut self,
        terms: &Terms,
        swap: &SwapTerms,
        tip: u32,
        tip_age: u64,
        rollback: bool,
        claiming: bool,
    ) -> Result<View, String> {
        let slot = self.sol.slot()?;
        if slot != self.last_slot.0 {
            self.last_slot = (slot, Instant::now());
        }
        let (htlc, coin_disagrees) = self.coin_view(terms, swap, tip)?;
        let (escrow, escrow_closed) = self.escrow_view(&terms.swap_id(), swap, claiming)?;
        let adaptor_held = match self.signer.call(&Call::AdaptorHeld { swap_id: terms.swap_id() })? {
            Reply::Held(held) => held,
            reply => return Err(format!("signer: {reply:?}")),
        };
        let tips_disagree = match &self.second {
            Some(second) => second.tip().map_or(true, |t| t.abs_diff(tip) > 2),
            None => false,
        };
        Ok(View {
            liquid_tip: tip,
            liquid_tip_age_secs: tip_age,
            solana_slot: slot,
            solana_slot_age_secs: self.last_slot.1.elapsed().as_secs(),
            rollback_detected: rollback,
            sources_disagree: coin_disagrees || tips_disagree,
            maker_token_ok: self.sol.token_usable(&ata(&self.identity.solana))?,
            adaptor_held,
            htlc,
            escrow,
            escrow_closed,
            open_exposure: self.open_exposure(terms)?,
        })
    }

    fn send_solana(&mut self, swap: &[u8; 32], kind: &str, reply: Reply) -> Result<(), String> {
        let Reply::Solana(hex) = reply else {
            return Err(format!("signer: {reply:?}"));
        };
        let tx: SolanaTx = bincode::deserialize(&unhex(&hex)?).map_err(|e| e.to_string())?;
        self.sol.send_signed(&tx).map(|_| ())?;
        self.note_cost(swap, kind, 0, (LAMPORTS_PER_SIGNATURE * tx.signatures.len() as u64 + sol::priority_lamports(&tx)) as i64);
        Ok(())
    }

    /// For the report, so a failure here must not stop a swap.
    /// Close settled escrows we paid for, and count the rent that comes back. The signer refuses any it shouldn't close yet.
    fn close_settled_escrows(&mut self) {
        if self.last_close.is_some_and(|t| t.elapsed() < Duration::from_secs(self.cfg.close_every_secs)) {
            return;
        }
        self.last_close = Some(Instant::now());
        let swaps = match self.store.escrows_to_close() {
            Ok(swaps) => swaps,
            Err(e) => return eprintln!("could not list escrows to close: {e}"),
        };
        for terms in swaps {
            let id = terms.swap_id();
            let address = match solana_point_of(&terms.lock_point.0) {
                Ok(point) => escrow_pda(&self.identity.initiator, &point),
                Err(_) => continue,
            };
            let rent = match self.sol.escrow(&address) {
                Ok(None) => {
                    let _ = self.store.mark_closed(&id);
                    continue;
                }
                Ok(Some(_)) => self.sol.lamports(&address).unwrap_or(0),
                Err(_) => continue,
            };
            match self.signer.call(&Call::CloseEscrow { lock_point: terms.lock_point }) {
                Ok(reply @ Reply::Solana(_)) => match self.send_solana(&id, "CloseEscrow", reply) {
                    Ok(()) => {
                        self.note_cost(&id, "RentBack", 0, -(rent as i64));
                        let _ = self.store.mark_closed(&id);
                        println!("{} escrow closed, {rent} lamports of rent back", hex(&id));
                    }
                    Err(e) => eprintln!("{} could not close its escrow: {}", hex(&id), e.chars().take(200).collect::<String>()),
                },
                // Not yet: settled too recently, which is most of the time.
                Ok(Reply::Failed(e)) if e.contains("too soon") => {}
                other => eprintln!("{} could not close its escrow: {other:?}", hex(&id)),
            }
        }
    }

    fn note_cost(&self, swap: &[u8; 32], kind: &str, lbtc_sats: u64, lamports: i64) {
        if let Err(e) = self.store.record_cost(swap, kind, lbtc_sats, lamports) {
            eprintln!("could not record a cost: {e}");
        }
    }

    fn send_liquid(&mut self, swap: &[u8; 32], kind: &str, reply: Reply) -> Result<Vec<u8>, String> {
        let Reply::Liquid(hex) = reply else {
            return Err(format!("signer: {reply:?}"));
        };
        let raw = unhex(&hex)?;
        let tx: LiquidTx = deserialize(&raw).map_err(|e| e.to_string())?;
        match self.chain.broadcast(&tx) {
            Ok(_) => {}
            // Already known or mined means it worked before, which is the expected case for a retry.
            Err(e) if e.contains("already") || e.contains("txn-mempool-conflict") => {}
            Err(e) => return Err(e),
        }
        let fee: u64 = tx.output.iter().filter(|o| o.is_fee()).filter_map(|o| o.value.explicit()).sum();
        self.note_cost(swap, kind, fee, 0);
        Ok(raw)
    }

    fn offer_claim(&mut self, details: &Details) -> Result<(), String> {
        let lock_point = details.terms.lock_point.to_hex();
        match self.signer.call(&Call::PrepareClaim { terms: details.terms.clone() })? {
            Reply::Nothing => Ok(()),
            Reply::ClaimTemplate { tx, nonce } => {
                let outbox = self.outbox.as_ref().ok_or("no relay is configured, so the taker can't be reached")?;
                let body = Body::ClaimOffer(ClaimOffer { lock_point, tx, nonce });
                outbox.send(&details.taker_nostr, &details.negotiation, body)
            }
            // The pool ran out, so look at it again now, not in a minute.
            Reply::Failed(e) if e.contains("fee coin") => {
                self.last_top_up = None;
                Err(e)
            }
            reply => Err(format!("signer: {reply:?}")),
        }
    }

    fn execute(&mut self, terms: &Terms, details: &Details, action: &Action) -> Result<(), String> {
        let id = terms.swap_id();
        let lock_point = terms.lock_point;
        let t = &details.terms;
        let kind = action_name(action);
        match (action, t.direction) {
            (Action::RequestAdaptor, Direction::L2S) => self.offer_claim(details),
            (Action::LockSolana { deadline_slot }, Direction::L2S) => {
                let reply = self.signer.call(&Call::LockSolana { terms: t.clone(), deadline_slot: *deadline_slot })?;
                self.send_solana(&id, &kind, reply)?;
                // Ours until the escrow is closed for it, which the report counts as money back.
                if let Ok(rent) = self.sol.escrow_rent() {
                    self.note_cost(&id, "EscrowRent", 0, rent as i64);
                }
                Ok(())
            }
            (Action::LockLiquid | Action::RebroadcastLock, Direction::S2L) => {
                // The signer returns the same bytes every time, so this can't pay twice. They are kept here too, so
                // the coin's outpoint is known at once.
                let reply = self.signer.call(&Call::LockLiquid { terms: t.clone() })?;
                let Reply::Liquid(ref text) = reply else {
                    return Err(format!("signer: {reply:?}"));
                };
                let raw = unhex(text)?;
                self.store.store_lock_tx(&id, &raw).map_err(|e| e.to_string())?;
                // A rebroadcast is the same transaction, so it shares the lock's name. The taker's fee coin goes with it.
                self.send_liquid(&id, "LockLiquid", reply)?;
                self.note_cost(&id, "FeeCoinForTaker", self.identity.fee_coin_sats, 0);
                Ok(())
            }
            (Action::ClaimLiquid { secret }, Direction::L2S) => {
                let reply = self.signer.call(&Call::ClaimLiquid { terms: t.clone(), secret: *secret })?;
                self.send_liquid(&id, &kind, reply).map(|_| ())
            }
            (Action::ClaimSolana { secret }, Direction::S2L) => {
                let call = Call::ClaimSolana { taker_solana: t.taker_solana.clone(), lock_point, secret: *secret };
                let reply = self.signer.call(&call)?;
                self.send_solana(&id, &kind, reply)
            }
            (Action::RefundSolana, Direction::L2S) => {
                let reply = self.signer.call(&Call::RefundSolana { lock_point })?;
                self.send_solana(&id, &kind, reply)
            }
            (Action::RefundLiquid, Direction::S2L) => {
                let reply = self.signer.call(&Call::RefundLiquid { terms: t.clone() })?;
                self.send_liquid(&id, &kind, reply).map(|_| ())
            }
            (Action::Extend { new_deadline }, Direction::S2L) => {
                let call =
                    Call::Extend { taker_solana: t.taker_solana.clone(), lock_point, new_deadline: *new_deadline };
                let reply = self.signer.call(&call)?;
                self.send_solana(&id, &kind, reply)
            }
            (Action::Release, Direction::S2L) => {
                let call = Call::Release { taker_solana: t.taker_solana.clone(), lock_point };
                let reply = self.signer.call(&call)?;
                self.send_solana(&id, &kind, reply)
            }
            (Action::Page(page), _) => {
                self.page(&id, page);
                Ok(())
            }
            (other, _) => Err(format!("{} does not fit this swap's direction", action_name(other))),
        }
    }

    fn tick_swap(
        &mut self,
        terms: &Terms,
        state: &State,
        tip: u32,
        tip_age: u64,
        rollback: bool,
    ) -> Result<(), String> {
        let hash = terms.swap_id();
        let body = self.store.details(&hash).map_err(|e| e.to_string())?.ok_or("a swap with no details")?;
        let details: Details = serde_json::from_str(&body).map_err(|e| e.to_string())?;
        let claiming = terms.direction == Direction::S2L && matches!(state, State::Claiming { .. });
        let view = self.view(terms, &details.terms, tip, tip_age, rollback, claiming)?;
        let (next, actions) = engine::step(&self.cfg.engine, terms, state, &view);

        // Hold back repeats of an action already sent recently. A first send always goes out.
        let retry = Duration::from_secs(self.cfg.retry_secs);
        let due: Vec<Action> = actions
            .into_iter()
            .filter(|a| {
                matches!(a, Action::Page(_))
                    || self.last_sent.get(&(hash, action_name(a))).is_none_or(|t| t.elapsed() >= retry)
            })
            .collect();

        // Commit first, so a crash can't lose track of what may be in flight.
        let ids = self.store.commit_step(&hash, &next, &due).map_err(|e| e.to_string())?;
        if next != *state {
            println!("{} {:?}: {state:?} -> {next:?}", hex(&hash), terms.direction);
        }
        for (id, action) in ids.into_iter().zip(due) {
            self.last_sent.insert((hash, action_name(&action)), Instant::now());
            match self.execute(terms, &details, &action) {
                Ok(()) => println!("{} did {}", hex(&hash), action_name(&action)),
                // The engine asks again on a later tick, if it is still needed.
                Err(e) => eprintln!(
                    "{} {} failed: {}",
                    hex(&hash),
                    action_name(&action),
                    e.chars().take(300).collect::<String>()
                ),
            }
            self.store.finish_intent(id).map_err(|e| e.to_string())?;
        }
        // S→L proceeds land in the hot wallet. Put them back under the vault's cap.
        if self.identity.vault
            && terms.direction == Direction::S2L
            && next == (State::Done { outcome: Outcome::Completed })
            && next != *state
        {
            let reply = self.signer.call(&Call::DepositToVault { amount: terms.solana_amount })?;
            if let Err(e) = self.send_solana(&hash, "DepositToVault", reply) {
                eprintln!("{} could not sweep to the vault: {e}", hex(&hash));
            }
        }
        Ok(())
    }

    /// One pass over every open swap. A failure on one swap doesn't hold up the others.
    pub fn tick(&mut self) -> Result<(), String> {
        let tip = self.chain.tip()?;
        let tip_age = now_secs().saturating_sub(self.chain.tip_time()?);
        let rollback = self.check_rollback(tip)?;
        if !rollback {
            self.top_up_fee_coins();
        }
        for (terms, state) in self.store.open_swaps().map_err(|e| e.to_string())? {
            if let Err(e) = self.tick_swap(&terms, &state, tip, tip_age, rollback) {
                eprintln!("{}: {}", hex(&terms.swap_id()), e.chars().take(300).collect::<String>());
            }
        }
        self.close_settled_escrows();
        Ok(())
    }
}

/// For an operator who has checked the chain. New locks and all other actions resume on the next tick.
/// Paths in a config are relative to the config file, so a maker's folder can be copied without pointing at the original.
fn load_config(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut cfg: Config = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let base = path.parent().unwrap_or(Path::new("."));
    // An absolute path is kept as it is, because joining onto one replaces what came before.
    cfg.db = base.join(&cfg.db);
    cfg.signer_socket = base.join(&cfg.signer_socket);
    cfg.pages = base.join(&cfg.pages);
    if let Some(rfq) = cfg.rfq.as_mut() {
        rfq.nostr_key = base.join(&rfq.nostr_key);
    }
    Ok(cfg)
}

/// What this maker has earned and spent, from its own records. Read-only in effect, and safe while the maker runs.
pub fn report(config_path: &Path) -> Result<(), String> {
    let cfg = load_config(config_path)?;
    let store = Store::open(&cfg.db).map_err(|e| e.to_string())?;
    let r = store.report().map_err(|e| e.to_string())?;
    let money = |units: i128| format!("{:.6}", units as f64 / 1e6);
    println!("{:<12} {:<4} {:<10} {:>6} {:>14} {:>12}", "day (UTC)", "dir", "outcome", "swaps", "volume", "spread");
    let mut totals = (0u64, 0i128, 0i128);
    for ((day, direction, outcome), line) in &r.swaps {
        let day = day.as_deref().unwrap_or("earlier");
        println!("{day:<12} {direction:<4} {outcome:<10} {:>6} {:>14} {:>12}", line.swaps, money(line.volume), money(line.spread));
        totals = (totals.0 + line.swaps, totals.1 + line.volume, totals.2 + line.spread);
    }
    println!("{:<12} {:<4} {:<10} {:>6} {:>14} {:>12}", "total", "", "", totals.0, money(totals.1), money(totals.2));
    println!();
    println!("{:<12} {:>12} {:>14}", "day (UTC)", "L-BTC sats", "SOL");
    let mut spent = (0u64, 0i64);
    for (day, (sats, lamports)) in &r.costs {
        println!("{day:<12} {sats:>12} {:>14.9}", *lamports as f64 / 1e9);
        spent = (spent.0 + sats, spent.1 + lamports);
    }
    println!("{:<12} {:>12} {:>14.9}", "total", spent.0, spent.1 as f64 / 1e9);
    println!();
    println!("{} open, {} never started.", r.open, r.abandoned);
    println!();
    // From the running signer, which holds the keys. The rest of the report needs only the store.
    let balances = Client::connect(&cfg.signer_socket).and_then(|mut c| c.call(&Call::Balances).map_err(|e| e.to_string()));
    match balances {
        Ok(Reply::Balances { liquid_usdt, liquid_lbtc_sats, free_fee_coins, solana_usdt, sol_lamports }) => {
            println!("Liquid  {:>14} USDt  {liquid_lbtc_sats:>12} L-BTC sats, {free_fee_coins} fee coins ready", format!("{:.8}", liquid_usdt as f64 / 1e8));
            println!("Solana  {:>14} USDT  {:>12.9} SOL", money(solana_usdt as i128), sol_lamports as f64 / 1e9);
        }
        other => println!("Balances: can't ask the signer ({other:?})."),
    }
    Ok(())
}

pub fn clear_rollback(config_path: &Path) -> Result<(), String> {
    let cfg = load_config(config_path)?;
    let store = Store::open(&cfg.db).map_err(|e| e.to_string())?;
    let was_set = store.rollback().map_err(|e| e.to_string())?;
    store.set_rollback(false).map_err(|e| e.to_string())?;
    println!("rollback alarm was {}, and is now clear", if was_set { "set" } else { "not set" });
    Ok(())
}

/// Run until stopped. A chain or signer outage just means trying again.
pub fn run(config_path: &Path) -> Result<(), String> {
    let cfg = load_config(config_path)?;
    let tick = Duration::from_millis(cfg.tick_millis);
    let mut maker = loop {
        match Maker::new(cfg.clone()) {
            Ok(m) => break m,
            Err(e) if e.starts_with("refusing") => return Err(e),
            Err(e) => {
                eprintln!("waiting to start: {e}");
                // Not faster than this: if a public API is rate limiting us, retrying only prolongs it.
                std::thread::sleep(Duration::from_secs(30));
            }
        }
    };
    if let Some(rfq_cfg) = cfg.rfq.clone() {
        let (pubkey, outbox) = rfq::start(rfq_cfg, &cfg)?;
        maker.outbox = Some(outbox);
        println!("maker answering quotes as {}", pubkey.to_hex());
    }
    println!("maker running");
    loop {
        if let Err(e) = maker.tick() {
            eprintln!("tick failed: {}", e.chars().take(300).collect::<String>());
            if e.contains("Broken pipe") || e.contains("EOF") {
                if let Ok(client) = Client::connect(&cfg.signer_socket) {
                    maker.signer = client;
                }
            }
        }
        std::thread::sleep(tick);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded(folder: &str, edit: impl Fn(&str) -> String) -> Config {
        let dir = std::env::temp_dir().join(format!("liq2sol-config-{}-{folder}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("maker.json");
        std::fs::write(&path, edit(include_str!("../../../deploy/maker.example.json"))).unwrap();
        load_config(&path).unwrap()
    }

    #[test]
    fn the_example_stays_inside_the_folder_it_is_copied_to() {
        for folder in ["first", "second"] {
            let cfg = loaded(folder, str::to_string);
            let root = std::env::temp_dir().join(format!("liq2sol-config-{}-{folder}", std::process::id()));
            assert_eq!(cfg.db, root.join("maker.db"));
            assert_eq!(cfg.signer_socket, root.join("run/signer.sock"));
            assert_eq!(cfg.pages, root.join("pages.log"));
            assert_eq!(cfg.rfq.unwrap().nostr_key, root.join("keys/nostr.key"));
        }
    }

    #[test]
    fn an_absolute_path_is_kept() {
        let cfg = loaded("absolute", |s| s.replace("\"maker.db\"", "\"/elsewhere/maker.db\""));
        assert_eq!(cfg.db, Path::new("/elsewhere/maker.db"));
    }
}
