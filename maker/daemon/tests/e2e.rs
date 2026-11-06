//! The real daemon and signer processes, against a regtest node and a real Solana cluster, with a simulated taker.
//! Scenarios E1, E3, E5, E9, E11, E15 and E17 from docs/test-plan.md: each checks where the money ended up. A swap's taker is
//! simulated by a bot answering over the relay, as a real wallet would.
//!
//!   ELEMENTSD=~/bin/elementsd ELEMENTS_CLI=~/bin/elements-cli SOLANA_RPC=https://api.devnet.solana.com \
//!     cargo test -p liq2sol-maker --test e2e -- --ignored --nocapture
//!
//! Run `cargo build` first, because this starts the signer binary from the same workspace. Needs `cli/.keys` from
//! `liq2sol-cli setup`, holding SOL and stand-in tokens. `SCENARIO` runs a subset.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anchor_lang::prelude::Pubkey;
use liq2sol_chain::esplora::{build_confidential_pay, Esplora, Rpc};
use liq2sol_chain::liquid::{
    build_swap_spend, exact_secret_coin, keypair_from, lbtc, random32, usdt, use_network, NetworkConfig, Payment,
    SecretCoin, Wallet, FEE,
};
use liq2sol_chain::sol::{self, ata, escrow_pda, Sol, MAINNET_GENESIS};
use liq2sol_chain::swap::{answer_claim, TakerClaim};
use liq2sol_maker::rfq::{load_keys, RfqConfig};
use liq2sol_maker::{accept, Config, Details};
use liq2sol_maker_core::engine::{Action, Config as EngineConfig, Direction, Outcome, State, Terms};
use liq2sol_maker_core::policy::Limits;
use liq2sol_maker_core::store::Store;
use liq2sol_maker_core::LockPoint;
use liq2sol_rfq::messages::{self as msg, Accept, AmountSide, Body, Envelope, MakerTerms, Request, Units};
use liq2sol_rfq::mock_relay::MockRelay;
use liq2sol_rfq::relay::{Relay, RelayEvent};
use liq2sol_rfq::rules::TakerRules;
use liq2sol_rfq::taker::{find_offers, negotiate, send_body, wait_from, Negotiated, TakerSide};
use liq2sol_rfq::wrap;
use liq2sol_signer::{unhex, Call, Client, Config as SignerConfig, LiquidSource, Reply, SwapTerms};
use liq2sol_swapkey::{nonce_bytes, nonce_from, point_of, share_bytes, share_from, solana_point_of, solana_value};
use nostr::prelude::{Keys, Kind};
use simplicityhl::elements::encode::{deserialize, serialize_hex};
use simplicityhl::elements::secp256k1_zkp::{Keypair as LiquidKey, SecretKey};
use simplicityhl::elements::{Address, AddressParams, AssetId, BlockHash, Script, Sequence, Transaction as LiquidTx};
use solana_escrow::state::EscrowState;
use solana_sdk::signature::{read_keypair_file, Keypair};
use solana_sdk::signer::Signer as _;

const SOL_AMOUNT: u64 = 10;
const LIQ_AMOUNT: u64 = SOL_AMOUNT * 100;
const NETWORK: &str = "regtest";
const QUOTE_SECS: u64 = 4;
const FLOOD_LIMIT: usize = 12;
/// What the RFQ taker asks to send, at the 1% spread above.
const RFQ_SEND: u64 = 100;
const RFQ_RECEIVE: u64 = 99;
/// Each maker's limit per swap. Above this, the page splits an order.
const RFQ_MAX: u64 = 120;
type Res<T> = Result<T, String>;

fn check(condition: bool, what: &str) -> Res<()> {
    if condition {
        println!("    ok: {what}");
        Ok(())
    } else {
        Err(format!("FAILED: {what}"))
    }
}

struct Node {
    child: Option<Child>,
    daemon: String,
    dir: PathBuf,
    rpc: Rpc,
    wallet_rpc: Rpc,
    port: u16,
}

impl Node {
    fn start(root: &Path) -> Self {
        let daemon = std::env::var("ELEMENTSD").unwrap_or_else(|_| "elementsd".into());
        let cli = std::env::var("ELEMENTS_CLI").unwrap_or_else(|_| "elements-cli".into());
        let port = 30_000 + (std::process::id() % 10_000) as u16;
        let dir = root.join("node");
        std::fs::create_dir_all(&dir).unwrap();
        let base: Vec<String> = ["-chain=liquidregtest", "-rpcuser=t", "-rpcpassword=t", &format!("-rpcport={port}")]
            .map(String::from)
            .to_vec();
        let mut with_wallet = base.clone();
        with_wallet.push("-rpcwallet=w".into());
        let rpc = Rpc { cli: cli.clone(), args: base };
        let wallet_rpc = Rpc { cli, args: with_wallet };
        let mut node = Node { child: None, daemon, dir, rpc, wallet_rpc, port };
        node.spawn();
        node.rpc.call(&["createwallet", "w"]).unwrap();
        node.rpc.call(&["rescanblockchain"]).unwrap();
        node
    }

    fn spawn(&mut self) {
        let child = Command::new(&self.daemon)
            .arg(format!("-datadir={}", self.dir.display()))
            .args(["-chain=liquidregtest", "-listen=0", "-txindex=1", "-validatepegin=0", "-fallbackfee=0.00001"])
            .args(["-initialfreecoins=2100000000", "-anyonecanspendaremine=1", "-evbparams=simplicity:-1:::"])
            .arg("-acceptdiscountct=1")
            .args([
                "-persistmempool=0",
                "-rpcuser=t",
                "-rpcpassword=t",
                &format!("-rpcport={}", self.port),
                "-printtoconsole=0",
            ])
            .stdout(Stdio::null())
            .spawn()
            .expect("start elementsd");
        self.child = Some(child);
        let start = Instant::now();
        while self.rpc.call(&["getblockcount"]).is_err() {
            assert!(start.elapsed() < Duration::from_secs(60), "elementsd did not start");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn stop(&mut self) {
        let _ = self.rpc.call(&["stop"]);
        if let Some(mut c) = self.child.take() {
            let _ = c.wait();
        }
    }

    fn mine(&self, blocks: u32) {
        mine_with(&self.wallet_rpc, blocks);
    }

    fn fund(&self, address: &str, sats: u64, label: &str) {
        let amount = format!("{}.{:08}", sats / 100_000_000, sats % 100_000_000);
        self.wallet_rpc
            .call(&["sendtoaddress", address, &amount, "", "", "false", "false", "1", "UNSET", "false", label])
            .unwrap();
    }

    /// As the federation would: mark the block invalid, and forget its transactions.
    fn roll_back_to_before(&mut self, height: u32) {
        let hash = self.rpc.call(&["getblockhash", &height.to_string()]).unwrap();
        self.rpc.call(&["invalidateblock", &hash]).unwrap();
        self.stop();
        self.spawn();
        self.rpc.call(&["loadwallet", "w"]).unwrap();
    }
}

fn mine_with(rpc: &Rpc, blocks: u32) {
    if let Ok(address) = rpc.call(&["getnewaddress"]) {
        let _ = rpc.call(&["generatetoaddress", &blocks.to_string(), &address]);
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Mines steadily, like a real chain, until told to halt.
struct Miner {
    on: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Miner {
    fn start(rpc: Rpc) -> Self {
        let on = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let (o, s) = (on.clone(), stop.clone());
        let handle = std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                if o.load(Ordering::SeqCst) {
                    mine_with(&rpc, 1);
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        });
        Self { on, stop, handle: Some(handle) }
    }

    fn halt(&self) {
        self.on.store(false, Ordering::SeqCst);
    }

    fn resume(&self) {
        self.on.store(true, Ordering::SeqCst);
    }
}

impl Drop for Miner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

struct Proc {
    child: Child,
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct World {
    node: Node,
    miner: Miner,
    chain: Esplora,
    sol: Sol,
    genesis: BlockHash,
    taker_sol: Keypair,
    taker_wallet: Wallet,
    maker_solana: Pubkey,
    /// Our escrows are seeded with the vault, and draw from its token account.
    vault: Pubkey,
    vault_token: Pubkey,
    vault_owner: Keypair,
    maker_address: String,
    maker_blinding: SecretKey,
    bot: Bot,
    client: Client,
    store: Store,
    daemon: Option<Proc>,
    daemon_config: PathBuf,
    pages: PathBuf,
    _signer: Proc,
    fake_asset: AssetId,
    relay: MockRelay,
    relay_url: String,
    maker_nostr: nostr::prelude::PublicKey,
}

impl World {
    fn start_daemon(&mut self) {
        // Kept next to the config, so a failure can be explained afterwards.
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("daemon.log"))
            .expect("open the daemon's log");
        let child = Command::new(env!("CARGO_BIN_EXE_liq2sol-maker"))
            .arg(&self.daemon_config)
            .stdout(log)
            .spawn()
            .expect("start daemon");
        self.daemon = Some(Proc { child });
    }

    /// As a crash: no warning, no cleanup.
    fn kill_daemon(&mut self) {
        self.daemon = None;
        println!("    (killed the daemon with no warning)");
    }

    fn state(&self, hash: &[u8; 32]) -> Res<State> {
        Ok(self.store.state(hash).map_err(|e| e.to_string())?.ok_or("unknown swap")?.1)
    }

    fn wait_state(&self, hash: &[u8; 32], want: &str, limit: u64, pred: impl Fn(&State) -> bool) -> Res<State> {
        let start = Instant::now();
        loop {
            let state = self.state(hash)?;
            if pred(&state) {
                println!("    ok: reached {want} after {}s", start.elapsed().as_secs());
                return Ok(state);
            }
            if start.elapsed() > Duration::from_secs(limit) {
                return Err(format!("FAILED: never reached {want}, stuck at {state:?}"));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn wait_for<T>(&self, what: &str, limit: u64, mut f: impl FnMut() -> Res<Option<T>>) -> Res<T> {
        let start = Instant::now();
        loop {
            if let Ok(Some(v)) = f() {
                println!("    ok: {what} after {}s", start.elapsed().as_secs());
                return Ok(v);
            }
            if start.elapsed() > Duration::from_secs(limit) {
                return Err(format!("FAILED: {what} never happened"));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn holdings(&self) -> Res<u64> {
        let address = simplicityhl::elements::Address::from_str(&self.maker_address).map_err(|e| format!("{e:?}"))?;
        Ok(self
            .chain
            .secret_coins(&address, &self.maker_blinding)?
            .iter()
            .filter(|c| c.secrets.asset == usdt())
            .map(|c| c.secrets.value)
            .sum())
    }

    fn maker_holdings(&self) -> Res<u64> {
        self.holdings()
    }

    fn tokens(&self, owner: &Pubkey) -> Res<u64> {
        self.sol.token_balance(&ata(owner))
    }

    /// The Solana funds the maker controls: its vault, plus anything collected and not yet swept.
    fn maker_tokens(&self) -> Res<u64> {
        Ok(self.sol.token_balance(&self.vault_token)? + self.tokens(&self.maker_solana)?)
    }

    fn maker_key(&mut self, id: [u8; 32]) -> LockPoint {
        let Reply::SwapKey { key } = self.client.call(&Call::SwapKey { swap_id: id }).unwrap() else { panic!() };
        key
    }
}

/// A taker's wallet at work: it watches for claim offers on its L→S swaps, checks each one, and signs it.
struct Bot {
    keys: Keys,
    swaps: Arc<Mutex<HashMap<String, BotSwap>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone)]
struct BotSwap {
    terms: SwapTerms,
    key: LiquidKey,
    maker_key: LockPoint,
}

impl Bot {
    fn start(url: &str, maker: nostr::prelude::PublicKey, chain: Esplora, genesis: BlockHash) -> Self {
        let keys = Keys::generate();
        let swaps: Arc<Mutex<HashMap<String, BotSwap>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (url, me, map, halt) = (url.to_string(), keys.clone(), swaps.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let mut relay = Relay::connect(&url, Duration::from_millis(200)).expect("bot relay");
            relay
                .subscribe("bot", &[Kind::from(msg::WRAP_KIND)], Some(&me.public_key()), now_secs() - 2 * 86_400)
                .expect("bot subscribe");
            let mut answered = HashSet::new();
            while !halt.load(Ordering::SeqCst) {
                let Ok(Some(RelayEvent::Event(e))) = relay.next_event() else { continue };
                let Ok(got) = wrap::unwrap(&me, &e) else { continue };
                if got.sender != maker {
                    continue;
                }
                let Body::ClaimOffer(offer) = got.envelope.body else { continue };
                // The maker repeats its offer until the lock appears. Answering once per nonce is enough.
                if !answered.insert(offer.nonce.clone()) {
                    continue;
                }
                let Some(swap) = map.lock().unwrap().get(&offer.lock_point).cloned() else { continue };
                match answer_offer(&chain, &swap, &offer, genesis) {
                    Ok(answer) => {
                        let sent =
                            send_body(&mut relay, &me, &maker, &got.envelope.id, NETWORK, Body::ClaimAnswer(answer));
                        if let Err(e) = sent {
                            eprintln!("bot: {e}");
                        }
                    }
                    Err(e) => eprintln!("bot: refusing an offer: {e}"),
                }
            }
        });
        Self { keys, swaps, stop, thread: Some(thread) }
    }

    fn watch(&self, swap: BotSwap) {
        self.swaps.lock().unwrap().insert(swap.terms.lock_point.to_hex(), swap);
    }
}

impl Drop for Bot {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Check a claim offer and answer it. The coin comes from the taker's own reading of the chain.
fn answer_offer(chain: &Esplora, s: &BotSwap, offer: &msg::ClaimOffer, genesis: BlockHash) -> Res<msg::ClaimAnswer> {
    let address = s.terms.address(&s.maker_key)?;
    let coins = chain.secret_coins(&address, &s.terms.blinding()?)?;
    let coin = exact_secret_coin(&coins, usdt(), s.terms.liquid_amount, chain.tip()?, 0).ok_or("no coin of ours")?;
    let tx: LiquidTx = deserialize(&unhex(&offer.tx)?).map_err(|e| e.to_string())?;
    let nonce = nonce_from(&unhex(&offer.nonce)?).map_err(|e| e.to_string())?;
    let output = s.terms.output(&s.maker_key)?;
    let (ours, share) =
        answer_claim(chain, &s.key, &output, &coin.coin.outpoint, &s.terms.lock_point.0, &nonce, &tx, genesis)?;
    Ok(msg::ClaimAnswer {
        lock_point: offer.lock_point.clone(),
        nonce: msg::hex(&nonce_bytes(&ours)),
        share: msg::hex(&share_bytes(&share)),
    })
}

/// One simulated taker's swap, in either direction.
struct Swap {
    secret: [u8; 32],
    id: [u8; 32],
    point: LockPoint,
    key: LiquidKey,
    terms: SwapTerms,
    maker_key: LockPoint,
    escrow: Pubkey,
    negotiation: String,
    to: Script,
}

fn fresh_swap_terms(
    w: &World,
    direction: Direction,
    lock_height: u32,
    liquid: u64,
    solana: u64,
) -> Res<(LiquidKey, SwapTerms, [u8; 32])> {
    let secret = random32()?;
    let point = LockPoint(point_of(&secret).map_err(|e| e.to_string())?);
    let key = keypair_from(&random32()?)?;
    let terms = SwapTerms {
        direction,
        lock_point: point,
        taker_key: LockPoint(key.public_key().serialize()),
        blinding_key: random32()?,
        lock_height,
        liquid_amount: liquid,
        solana_amount: solana,
        taker_solana: w.taker_sol.pubkey().to_string(),
    };
    Ok((key, terms, secret))
}

fn point_64(point: &LockPoint) -> [u8; 64] {
    solana_point_of(&point.0).unwrap()
}

fn new_swap(w: &mut World, direction: Direction, lock_in: u32) -> Res<Swap> {
    let tip = w.chain.tip()?;
    let (key, terms, secret) = fresh_swap_terms(w, direction, tip + lock_in, LIQ_AMOUNT, SOL_AMOUNT)?;
    let point = terms.lock_point;
    let id = point.id();
    let maker_key = w.maker_key(id);
    let negotiation = msg::hex(&random32()?);
    let details = Details {
        terms: terms.clone(),
        taker_nostr: w.bot.keys.public_key().to_hex(),
        negotiation: negotiation.clone(),
    };
    let engine_terms = Terms {
        direction,
        lock_point: point,
        liquid_amount: LIQ_AMOUNT,
        solana_amount: SOL_AMOUNT,
        h_accept: tip,
        lock_height: tip + lock_in,
    };
    check(accept(&w.store, &engine_terms, &details)?, "the maker accepted the swap")?;
    let initiator = match direction {
        Direction::L2S => w.vault,
        Direction::S2L => w.taker_sol.pubkey(),
    };
    let escrow = escrow_pda(&initiator, &point_64(&point));
    if direction == Direction::L2S {
        w.bot.watch(BotSwap { terms: terms.clone(), key, maker_key });
    }
    Ok(Swap {
        secret,
        id,
        point,
        key,
        terms,
        maker_key,
        escrow,
        negotiation,
        to: w.taker_wallet.address().script_pubkey(),
    })
}

fn new_l2s(w: &mut World, lock_in: u32) -> Res<Swap> {
    new_swap(w, Direction::L2S, lock_in)
}

fn new_s2l(w: &mut World, lock_in: u32) -> Res<Swap> {
    new_swap(w, Direction::S2L, lock_in)
}

fn swap_address(s: &Swap) -> Address {
    s.terms.address(&s.maker_key).unwrap()
}

/// Pay from the taker's wallet. A second payment in one block would reuse the first's coins, so that one waits its turn.
fn taker_pays_to(w: &World, address: &Address, asset: AssetId, amount: u64) -> Res<()> {
    let payment = || Payment { asset, amount, address: address.clone() };
    w.wait_for("the taker's payment to be accepted", 60, || {
        let tx = build_confidential_pay(&w.chain, &w.taker_wallet, vec![payment()], w.genesis)?;
        match w.chain.broadcast(&tx) {
            Ok(_) => Ok(Some(())),
            Err(e) if e.contains("conflict") || e.contains("missingorspent") => Ok(None),
            Err(e) => Err(e),
        }
    })
}

fn taker_pays(w: &World, s: &Swap) -> Res<()> {
    taker_pays_to(w, &swap_address(s), usdt(), LIQ_AMOUNT)
}

fn swap_coin(w: &World, s: &Swap, confs: u32) -> Res<Option<SecretCoin>> {
    let coins = w.chain.secret_coins(&swap_address(s), &s.terms.blinding()?)?;
    Ok(exact_secret_coin(&coins, usdt(), LIQ_AMOUNT, w.chain.tip()?, confs))
}

fn fee_coin(w: &World, s: &Swap) -> Res<SecretCoin> {
    let address = s.terms.taker_fee_address()?;
    w.chain
        .secret_coins(&address, &s.terms.blinding()?)?
        .into_iter()
        .next()
        .ok_or_else(|| "no fee coin from the maker".into())
}

/// The whole S→L claim, over a relay, from an unsigned transaction to one the network accepts.
#[allow(clippy::too_many_arguments)]
fn session_claim(
    w: &World,
    relay: &mut Relay,
    me: &Keys,
    id: &str,
    terms: &SwapTerms,
    key: &LiquidKey,
    maker_key: &LockPoint,
    secret: &[u8; 32],
    fee: &SecretCoin,
    coin: &SecretCoin,
    to: &Address,
) -> Res<LiquidTx> {
    let output = terms.output(maker_key)?;
    let point = terms.lock_point;
    let (tx, _) = build_swap_spend(coin, fee, to, 0, Sequence::MAX)?;
    let claim = TakerClaim::start(&w.chain, key, &output, tx, w.genesis)?;
    let maker = w.maker_nostr;
    let open = Body::SessionOpen(msg::SessionOpen {
        lock_point: point.to_hex(),
        tx: serialize_hex(&claim.tx),
        nonce: msg::hex(&nonce_bytes(&claim.nonce())),
    });
    send_body(relay, me, &maker, id, NETWORK, open)?;
    let got = wait_from(relay, me, &maker, id, NETWORK, Duration::from_secs(30))?;
    let Body::SessionNonce(answer) = got.envelope.body else {
        return Err(format!("expected a nonce, got {:?}", got.envelope.body));
    };
    let maker_nonce = nonce_from(&unhex(&answer.nonce)?).map_err(|e| e.to_string())?;
    let (share, signed) = claim.sign(key, &output, &point.0, &maker_nonce)?;
    let body =
        Body::TakerShare(msg::SessionShare { lock_point: point.to_hex(), share: msg::hex(&share_bytes(&share)) });
    send_body(relay, me, &maker, id, NETWORK, body)?;
    let got = wait_from(relay, me, &maker, id, NETWORK, Duration::from_secs(30))?;
    let Body::MakerShare(answer) = got.envelope.body else {
        return Err(format!("expected a share, got {:?}", got.envelope.body));
    };
    let maker_share = share_from(&unhex(&answer.share)?).map_err(|e| e.to_string())?;
    signed.finish(key, &output, &maker_key.0, maker_share, &point.0, secret, w.genesis)
}

/// A genuine request to open a session on a coin that really is locked, so the maker would answer it if the sender and swap
/// were accepted. Returns whether a nonce came back, on this key and this negotiation id.
#[allow(clippy::too_many_arguments)]
fn session_answered(
    w: &World,
    me: &Keys,
    id: &str,
    terms: &SwapTerms,
    key: &LiquidKey,
    maker_key: &LockPoint,
    fee: &SecretCoin,
    coin: &SecretCoin,
    wait: Duration,
) -> Res<bool> {
    let mut relay = Relay::connect(&w.relay_url, Duration::from_millis(200))?;
    relay.subscribe("probe", &[Kind::from(msg::WRAP_KIND)], Some(&me.public_key()), now_secs() - 60)?;
    let (tx, _) = build_swap_spend(coin, fee, &w.taker_wallet.address(), 0, Sequence::MAX)?;
    let claim = TakerClaim::start(&w.chain, key, &terms.output(maker_key)?, tx, w.genesis)?;
    let open = Body::SessionOpen(msg::SessionOpen {
        lock_point: terms.lock_point.to_hex(),
        tx: serialize_hex(&claim.tx),
        nonce: msg::hex(&nonce_bytes(&claim.nonce())),
    });
    send_body(&mut relay, me, &w.maker_nostr, id, NETWORK, open)?;
    match wait_from(&mut relay, me, &w.maker_nostr, id, NETWORK, wait) {
        Ok(got) => Ok(matches!(got.envelope.body, Body::SessionNonce(_))),
        Err(e) if e.contains("in time") => Ok(false),
        Err(e) => Err(e),
    }
}

/// The taker's S→L claim, with the swap's own taker key. Publishing it is what reveals the secret to the maker.
fn taker_claims(w: &World, s: &Swap, coin: &SecretCoin) -> Res<LiquidTx> {
    let fee = fee_coin(w, s)?;
    check(fee.secrets.value == FEE && fee.secrets.asset == lbtc(), "the maker sent the taker a fee coin")?;
    let mut relay = Relay::connect(&w.relay_url, Duration::from_millis(200))?;
    let me = w.bot.keys.clone();
    relay.subscribe("claim", &[Kind::from(msg::WRAP_KIND)], Some(&me.public_key()), now_secs() - 2 * 86_400)?;
    let tx = session_claim(
        w,
        &mut relay,
        &me,
        &s.negotiation,
        &s.terms,
        &s.key,
        &s.maker_key,
        &s.secret,
        &fee,
        coin,
        &w.taker_wallet.address(),
    )?;
    w.chain.broadcast(&tx)?;
    Ok(tx)
}

/// What a crash leaves behind if it happens right after the adapter signature is stored: done through the signer directly,
/// with the daemon down.
fn store_adaptor_directly(w: &mut World, s: &Swap) -> Res<()> {
    let swap = BotSwap { terms: s.terms.clone(), key: s.key, maker_key: s.maker_key };
    let Reply::ClaimTemplate { tx, nonce } = w.client.call(&Call::PrepareClaim { terms: s.terms.clone() })? else {
        return Err("no claim template".into());
    };
    let offer = msg::ClaimOffer { lock_point: s.point.to_hex(), tx, nonce };
    let answer = answer_offer(&w.chain, &swap, &offer, w.genesis)?;
    let call = Call::StoreAdaptor { terms: s.terms.clone(), taker_nonce: answer.nonce, taker_share: answer.share };
    match w.client.call(&call)? {
        Reply::Held(true) => Ok(()),
        other => Err(format!("not stored: {other:?}")),
    }
}

fn locked(s: &State) -> bool {
    matches!(s, State::LockedSolana { .. } | State::LockedLiquid)
}

fn done(outcome: Outcome) -> impl Fn(&State) -> bool {
    move |s| *s == State::Done { outcome }
}

fn revealed(s: &Swap) -> [u8; 32] {
    solana_value(&s.secret).unwrap()
}

/// E1 and E17: a whole L→S swap, with the daemon killed at the two points that matter most.
fn l2s_with_crashes(w: &mut World) -> Res<()> {
    let (maker_before, taker_before) = (w.maker_holdings()?, w.tokens(&w.taker_sol.pubkey())?);
    let s = new_l2s(w, 300)?;
    taker_pays(w, &s)?;
    w.wait_state(&s.id, "the maker's Solana lock", 180, locked)?;
    w.kill_daemon();
    let e = w.wait_for("the lock to land anyway, or be retried", 30, || w.sol.escrow(&s.escrow)).or_else(|_| {
        w.start_daemon();
        w.wait_for("the lock after the restart", 120, || w.sol.escrow(&s.escrow))
    })?;
    if w.daemon.is_none() {
        w.start_daemon();
    }
    check(e.close_on_restart && e.amount == SOL_AMOUNT, "the escrow has the right terms")?;

    w.sol.claim(&w.taker_sol, &s.escrow, &e, revealed(&s))?;
    check(w.tokens(&w.taker_sol.pubkey())? == taker_before + SOL_AMOUNT, "the taker was paid on Solana")?;
    w.kill_daemon();
    w.start_daemon();
    w.wait_state(&s.id, "completion after the restart", 180, done(Outcome::Completed))?;
    check(w.maker_holdings()? == maker_before + LIQ_AMOUNT, "the maker was paid on Liquid, exactly once")?;
    Ok(())
}

/// E1 and E17: a whole S→L swap, with crashes. The key check is that the swap address is only ever paid once.
fn s2l_with_crashes(w: &mut World) -> Res<()> {
    let (maker_before, vault_before) = (w.maker_holdings()?, w.sol.token_balance(&w.vault_token)?);
    let s = new_s2l(w, 80)?;
    let deadline = w.sol.slot()? + 5_000;
    w.sol.lock(&w.taker_sol, point_64(&s.point), w.maker_solana, SOL_AMOUNT, deadline, false)?;
    w.wait_state(&s.id, "the maker's Liquid lock", 120, locked)?;
    w.kill_daemon();
    w.start_daemon();
    let coin = w.wait_for("the lock to confirm", 120, || swap_coin(w, &s, 2))?;
    std::thread::sleep(Duration::from_secs(8)); // Long enough for a buggy retry to pay a second time.
    check(w.chain.outputs(&swap_address(&s))?.len() == 1, "the address was paid exactly once, despite the crash")?;
    check(w.maker_holdings()? == maker_before - LIQ_AMOUNT, "the maker paid exactly the agreed amount")?;

    taker_claims(w, &s, &coin)?;
    w.kill_daemon();
    std::thread::sleep(Duration::from_secs(4));
    w.start_daemon();
    w.wait_state(&s.id, "completion after the restart", 180, done(Outcome::Completed))?;
    let e = w.sol.escrow(&s.escrow)?.ok_or("no escrow")?;
    check(e.state == EscrowState::Claimed, "the escrow is claimed")?;
    w.wait_for("the proceeds to be swept into the vault, back under the cap", 90, || {
        Ok((w.sol.token_balance(&w.vault_token)? == vault_before + SOL_AMOUNT).then_some(()))
    })?;
    Ok(())
}

/// E17, the dangerous window: a crash after the state is saved, but before the action is sent. The state is written with
/// the daemon down, which is exactly what such a crash leaves behind.
fn crash_between_commit_and_send(w: &mut World) -> Res<()> {
    // L→S: the daemon "decided" to lock on Solana, then died before sending anything. The adapter signature was already
    // stored by the signer, because the engine doesn't decide to lock without it.
    let s = new_l2s(w, 300)?;
    w.kill_daemon();
    taker_pays(w, &s)?;
    let coin_seen = w.wait_for("the taker's coin to confirm", 60, || swap_coin(w, &s, 2))?;
    let start = Instant::now();
    while !matches!(w.client.call(&Call::PrepareClaim { terms: s.terms.clone() })?, Reply::ClaimTemplate { .. }) {
        check(start.elapsed() < Duration::from_secs(90), "a fee coin became ready")?;
        if let Reply::Liquid(hex) = w.client.call(&Call::TopUpFeeCoins)? {
            let tx: LiquidTx = deserialize(&unhex(&hex)?).map_err(|e| e.to_string())?;
            let _ = w.chain.broadcast(&tx);
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    store_adaptor_directly(w, &s)?;
    let deadline_slot = w.sol.slot()? + 150;
    let action = Action::LockSolana { deadline_slot };
    w.store.commit_step(&s.id, &State::LockedSolana { deadline_slot }, &[action]).map_err(|e| e.to_string())?;
    check(w.sol.escrow(&s.escrow)?.is_none(), "nothing was sent before the crash")?;
    w.start_daemon();
    let e = w.wait_for("the lock to be sent after the restart", 90, || w.sol.escrow(&s.escrow))?;
    check(e.deadline_slot == deadline_slot, "with the deadline decided before the crash")?;
    w.sol.claim(&w.taker_sol, &s.escrow, &e, revealed(&s))?;
    w.wait_state(&s.id, "completion", 120, done(Outcome::Completed))?;
    check(w.chain.spender(&coin_seen.coin.outpoint)?.is_some(), "the maker collected its coin")?;

    // S→L: the same, on the side that could pay twice.
    let s = new_s2l(w, 80)?;
    let maker_before = w.maker_holdings()?;
    let deadline = w.sol.slot()? + 5_000;
    w.kill_daemon();
    w.sol.lock(&w.taker_sol, point_64(&s.point), w.maker_solana, SOL_AMOUNT, deadline, false)?;
    w.store.commit_step(&s.id, &State::LockedLiquid, &[Action::LockLiquid]).map_err(|e| e.to_string())?;
    check(swap_coin(w, &s, 0)?.is_none(), "nothing was paid before the crash")?;
    w.start_daemon();
    let coin = w.wait_for("the lock to be paid after the restart", 90, || swap_coin(w, &s, 2))?;
    std::thread::sleep(Duration::from_secs(12)); // Longer than the retry interval, so a second payment would show.
    check(w.chain.outputs(&swap_address(&s))?.len() == 1, "the address was paid exactly once")?;
    check(w.maker_holdings()? == maker_before - LIQ_AMOUNT, "for exactly the agreed amount")?;
    taker_claims(w, &s, &coin)?;
    w.wait_state(&s.id, "completion", 120, done(Outcome::Completed))?;
    Ok(())
}

/// E3: the taker pays and signs, then vanishes. The maker gets its Solana funds back at the deadline by itself.
fn l2s_taker_vanishes(w: &mut World) -> Res<()> {
    let before = w.maker_tokens()?;
    let s = new_l2s(w, 300)?;
    taker_pays(w, &s)?;
    w.wait_state(&s.id, "the maker's Solana lock", 180, locked)?;
    w.wait_for("the escrow to appear", 60, || w.sol.escrow(&s.escrow))?;
    check(w.maker_tokens()? == before - SOL_AMOUNT, "the funds are locked, drawn from the vault")?;
    w.wait_state(&s.id, "the maker's automatic refund", 180, done(Outcome::Refunded))?;
    check(w.maker_tokens()? == before, "the refund went back to the vault")?;
    Ok(())
}

/// A taker who pays but never signs: without an adapter signature the maker risks nothing, so it never locks.
fn l2s_taker_never_signs(w: &mut World) -> Res<()> {
    let tokens = w.maker_tokens()?;
    let s = new_l2s(w, 300)?;
    w.bot.swaps.lock().unwrap().remove(&s.point.to_hex());
    taker_pays(w, &s)?;
    w.wait_state(&s.id, "giving up on a taker who never signs", 240, |st| *st == State::Abandoned)?;
    check(w.sol.escrow(&s.escrow)?.is_none(), "no escrow was ever created")?;
    check(w.maker_tokens()? == tokens, "no maker funds moved")?;
    Ok(())
}

/// E3: the taker locks, then vanishes. The maker refunds on Liquid at the lock height, and the taker on Solana later.
fn s2l_taker_vanishes(w: &mut World) -> Res<()> {
    let maker_before = w.maker_holdings()?;
    let taker_before = w.tokens(&w.taker_sol.pubkey())?;
    let s = new_s2l(w, 35)?;
    let deadline = w.sol.slot()? + 400;
    w.sol.lock(&w.taker_sol, point_64(&s.point), w.maker_solana, SOL_AMOUNT, deadline, false)?;
    w.wait_state(&s.id, "the maker's Liquid lock", 60, locked)?;
    w.wait_state(&s.id, "the maker's automatic refund", 300, done(Outcome::Refunded))?;
    check(w.maker_holdings()? == maker_before, "the maker has its Liquid funds back")?;
    while w.sol.slot()? < deadline {
        std::thread::sleep(Duration::from_secs(2));
    }
    let e = w.sol.escrow(&s.escrow)?.ok_or("no escrow")?;
    w.sol.refund(&w.taker_sol, &s.escrow, &e)?;
    check(w.tokens(&w.taker_sol.pubkey())? == taker_before, "the taker has its Solana funds back")?;
    Ok(())
}

/// E9: bad coins never get a lock. A bad escrow never gets one either, and is handed back.
fn bad_locks(w: &mut World) -> Res<()> {
    let tokens = w.maker_tokens()?;
    let s = new_l2s(w, 300)?;
    let address = swap_address(&s);
    taker_pays_to(w, &address, usdt(), LIQ_AMOUNT - 100)?;
    // Blinded, so nothing on-chain says what it is. Only reading it does.
    w.node.fund(&address.to_string(), LIQ_AMOUNT, &w.fake_asset.to_string());
    w.wait_state(&s.id, "giving up on a short coin and a fake asset", 240, |st| *st == State::Abandoned)?;
    check(w.sol.escrow(&s.escrow)?.is_none(), "no escrow was ever created")?;
    check(w.maker_tokens()? == tokens, "no maker funds moved")?;

    let maker_before = w.maker_holdings()?;
    let taker_before = w.tokens(&w.taker_sol.pubkey())?;
    let s = new_s2l(w, 80)?;
    let deadline = w.sol.slot()? + 5_000;
    w.sol.lock(&w.taker_sol, point_64(&s.point), w.maker_solana, SOL_AMOUNT - 1, deadline, false)?;
    w.wait_state(&s.id, "giving up on a short escrow", 240, |st| *st == State::Abandoned)?;
    check(w.maker_holdings()? == maker_before, "no maker funds moved")?;
    w.wait_for("the taker's escrow to be handed back", 60, || {
        Ok((w.tokens(&w.taker_sol.pubkey())? == taker_before).then_some(()))
    })?;
    Ok(())
}

/// E11: Liquid stalls with S→L open. With no help, the daemon extends the deadline, and the swap finishes safely.
fn liquid_halt_extends(w: &mut World) -> Res<()> {
    let s = new_s2l(w, 80)?;
    let deadline = w.sol.slot()? + 1_000;
    w.sol.lock(&w.taker_sol, point_64(&s.point), w.maker_solana, SOL_AMOUNT, deadline, false)?;
    w.wait_state(&s.id, "the maker's Liquid lock", 60, locked)?;
    let coin = w.wait_for("the lock to confirm", 90, || swap_coin(w, &s, 2))?;

    w.miner.halt();
    println!("    Liquid halts now");
    let e = w.wait_for("the daemon to extend the deadline by itself", 150, || {
        Ok(w.sol.escrow(&s.escrow)?.filter(|e| e.extended))
    })?;
    check(e.deadline_slot > deadline, "the taker's refund moved later")?;
    while w.sol.slot()? < deadline {
        std::thread::sleep(Duration::from_secs(2));
    }
    let refund = w.sol.refund(&w.taker_sol, &s.escrow, &e);
    check(refund.is_err(), "the taker can't refund after the original deadline: the theft is closed")?;

    println!("    Liquid resumes");
    w.miner.resume();
    taker_claims(w, &s, &coin)?;
    w.wait_state(&s.id, "completion after the halt", 180, done(Outcome::Completed))?;
    Ok(())
}

/// E15: a rollback past the taker's claim. The daemon stops and pages, and doesn't take the coin back.
fn rollback_stops_everything(w: &mut World) -> Res<()> {
    // A second swap, locked first so it survives the rollback, on which a signing session would otherwise go ahead.
    let open = new_s2l(w, 80)?;
    let far_deadline = w.sol.slot()? + 5_000;
    w.sol.lock(&w.taker_sol, point_64(&open.point), w.maker_solana, SOL_AMOUNT, far_deadline, false)?;
    w.wait_state(&open.id, "the second swap's Liquid lock", 60, locked)?;
    let open_coin = w.wait_for("the second lock to confirm", 90, || swap_coin(w, &open, 2))?;
    let open_fee = fee_coin(w, &open)?;

    let s = new_s2l(w, 45)?;
    let deadline = w.sol.slot()? + 5_000;
    w.sol.lock(&w.taker_sol, point_64(&s.point), w.maker_solana, SOL_AMOUNT, deadline, false)?;
    w.wait_state(&s.id, "the maker's Liquid lock", 60, locked)?;
    let coin = w.wait_for("the lock to confirm", 90, || swap_coin(w, &s, 2))?;
    let claim_tx = taker_claims(w, &s, &coin)?;
    let claim_id = claim_tx.txid();
    w.wait_state(&s.id, "the maker collecting on Solana", 120, done(Outcome::Completed))?;
    let claim_height = w.wait_for("the taker's claim to confirm", 60, || {
        let rpc = &w.node.rpc;
        let info: serde_json::Value =
            serde_json::from_str(&rpc.call(&["getrawtransaction", &claim_id.to_string(), "1"])?)
                .map_err(|e| e.to_string())?;
        let Some(hash) = info["blockhash"].as_str() else { return Ok(None) };
        let header: serde_json::Value =
            serde_json::from_str(&rpc.call(&["getblockheader", hash])?).map_err(|e| e.to_string())?;
        Ok(header["height"].as_u64().map(|h| h as u32))
    })?;

    w.miner.halt();
    let tokens = w.maker_tokens()?;
    println!("    The federation rolls Liquid back past the claim");
    w.kill_daemon();
    let lock_height = s.terms.lock_height;
    w.node.roll_back_to_before(claim_height);
    w.start_daemon();
    w.wait_for("the daemon to raise the rollback alarm", 60, || {
        Ok(std::fs::read_to_string(&w.pages).ok().filter(|t| t.contains("Rollback")))
    })?;
    check(w.store.rollback().map_err(|e| e.to_string())?, "the alarm is latched")?;
    check(w.chain.spender(&coin.coin.outpoint)?.is_none(), "the claim is gone, so the coin looks unspent")?;
    let bot = w.bot.keys.clone();
    let ask = |w: &World, wait| {
        session_answered(
            w,
            &bot,
            &open.negotiation,
            &open.terms,
            &open.key,
            &open.maker_key,
            &open_fee,
            &open_coin,
            wait,
        )
    };
    check(!ask(w, SESSION_WAIT)?, "no signing session runs while the alarm is on")?;

    // A coin that really is there, confirmed after the rollback: everything but the alarm says to lock.
    let waiting = new_l2s(w, 300)?;
    taker_pays(w, &waiting)?;
    w.node.mine(2);
    check(swap_coin(w, &waiting, 2)?.is_some(), "a confirmed coin is waiting")?;
    std::thread::sleep(Duration::from_secs(10));
    check(w.state(&waiting.id)? == State::WatchLock, "the alarm stops a lock that would otherwise go ahead")?;
    check(w.sol.escrow(&waiting.escrow)?.is_none(), "no escrow was created")?;
    check(w.maker_tokens()? == tokens, "no Solana funds moved")?;

    // An operator clears the alarm, with the real command. It must stay clear, and the waiting swap goes ahead.
    let status = Command::new(env!("CARGO_BIN_EXE_liq2sol-maker"))
        .args(["clear-rollback", w.daemon_config.to_str().ok_or("path")?])
        .stdout(Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    check(status.success(), "the operator cleared the alarm")?;
    w.wait_state(&waiting.id, "the waiting swap going ahead once the alarm is cleared", 120, locked)?;
    check(!w.store.rollback().map_err(|e| e.to_string())?, "the alarm stayed clear")?;
    check(ask(w, Duration::from_secs(20))?, "and the same request is answered once it is cleared")?;
    w.wait_for("its lock to land on Solana", 60, || w.sol.escrow(&waiting.escrow))?;

    // Past the lock height, with the coin unspent and no alarm, an honest maker still leaves it alone.
    while w.chain.tip()? < lock_height {
        w.node.mine(5);
    }
    std::thread::sleep(Duration::from_secs(10));
    check(w.chain.spender(&coin.coin.outpoint)?.is_none(), "the maker didn't refund a coin it already collected for")?;
    w.chain.broadcast(&claim_tx)?;
    w.node.mine(1);
    let spend = w.chain.spender(&coin.coin.outpoint)?.ok_or("not spent")?;
    check(spend.output[0].script_pubkey == s.to, "the taker's rebroadcast claim still collects, however late")?;
    w.miner.resume();
    Ok(())
}

/// The signer is built in the same workspace, so it sits next to the daemon's binary.
fn signer_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_liq2sol-maker")).with_file_name("liq2sol-signer")
}

fn build_world(root: &Path) -> World {
    let node = Node::start(root);
    node.mine(1);
    let parse = |t: String| -> serde_json::Value { serde_json::from_str(&t).unwrap() };
    let usdt_id = parse(node.wallet_rpc.call(&["issueasset", "1000", "0", "false"]).unwrap())["asset"]
        .as_str()
        .unwrap()
        .to_string();
    let fake = parse(node.wallet_rpc.call(&["issueasset", "1000", "0", "false"]).unwrap())["asset"]
        .as_str()
        .unwrap()
        .to_string();
    node.mine(1);
    let lbtc_id = parse(node.rpc.call(&["getsidechaininfo"]).unwrap())["pegged_asset"].as_str().unwrap().to_string();
    use_network(NetworkConfig {
        params: &AddressParams::ELEMENTS,
        usdt: AssetId::from_str(&usdt_id).unwrap(),
        lbtc: AssetId::from_str(&lbtc_id).unwrap(),
    });
    let chain = Esplora::Node(node.rpc.clone());
    let genesis = BlockHash::from_str(&chain.genesis().unwrap()).unwrap();

    let solana_rpc = std::env::var("SOLANA_RPC").unwrap_or_else(|_| "https://api.devnet.solana.com".into());
    let sol = Sol::new(&solana_rpc);
    assert_ne!(sol.genesis().unwrap(), MAINNET_GENESIS, "never on mainnet");

    let keys_src = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../cli/.keys"));
    let keys = root.join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::copy(keys_src.join("maker-solana.json"), keys.join("solana.json")).unwrap();

    // The vault's cold owner never goes near the signer or the daemon.
    let operator = read_keypair_file(keys.join("solana.json")).unwrap();
    let vault_owner = Keypair::new();
    let mut transfer = vec![2, 0, 0, 0];
    transfer.extend_from_slice(&20_000_000u64.to_le_bytes());
    let fund = solana_sdk::instruction::Instruction {
        program_id: anchor_lang::system_program::ID,
        accounts: vec![
            solana_sdk::instruction::AccountMeta::new(operator.pubkey(), true),
            solana_sdk::instruction::AccountMeta::new(vault_owner.pubkey(), true),
        ],
        data: transfer,
    };
    sol.send(&[fund], &operator, &[&vault_owner]).unwrap();
    let init = sol::vault_init_ix(&vault_owner.pubkey(), operator.pubkey(), 100_000, 1_000);
    sol.send(&[init], &vault_owner, &[]).unwrap();
    let vault = sol::maker_vault_pda(&vault_owner.pubkey());
    let vault_token = sol::maker_vault_token_pda(&vault);
    let deposit = sol::vault_deposit_ix(&operator.pubkey(), &vault_owner.pubkey(), 40 * SOL_AMOUNT);
    sol.send(&[deposit], &operator, &[]).unwrap();
    // Short and outside the build directory: a Unix socket path is limited to about 100 bytes.
    let private = std::env::temp_dir().join(format!("l2s-{}", std::process::id()));
    std::fs::create_dir_all(&private).unwrap();
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = private.join("signer.sock");

    // Short timeouts, so failures play out in minutes. The rules are unchanged.
    let engine = EngineConfig {
        l2s_deadline_slots: 150,
        s2l_gate_slots: 100,
        extend_after_stall_secs: 20,
        extend_within_slots: 2_000,
        extend_by_slots: 100_000,
        ..EngineConfig::default()
    };
    let liquid = LiquidSource::Node { cli: node.rpc.cli.clone(), args: node.rpc.args.clone() };
    let assets = Some((usdt_id.clone(), lbtc_id.clone()));
    let signer_cfg = SignerConfig {
        socket: socket.clone(),
        ledger: root.join("signer.db"),
        keys_dir: keys.clone(),
        liquid: liquid.clone(),
        solana_rpc: solana_rpc.clone(),
        limits: Limits::from(&engine),
        regtest_assets: assets.clone(),
        vault_owner: Some(vault_owner.pubkey().to_string()),
        fee_coin_sats: FEE,
    };
    let signer_path = root.join("signer.json");
    std::fs::write(&signer_path, serde_json::to_string(&signer_cfg).unwrap()).unwrap();
    let signer = Command::new(signer_binary()).arg(&signer_path).spawn().expect("start signer");
    let signer = Proc { child: signer };
    let start = Instant::now();
    while Client::connect(&socket).is_err() {
        assert!(start.elapsed() < Duration::from_secs(60), "signer did not start");
        std::thread::sleep(Duration::from_millis(200));
    }
    let mut client = Client::connect(&socket).unwrap();
    let Reply::Identity { liquid_address, solana, escrow_initiator, .. } = client.call(&Call::Identity).unwrap() else {
        panic!()
    };
    let maker_blinding = SecretKey::from_slice(&std::fs::read(keys.join("liquid-blinding.key")).unwrap()).unwrap();
    assert_eq!(escrow_initiator, vault.to_string(), "the signer runs on the vault");

    let pages = root.join("pages.log");
    let db = root.join("maker.db");
    let relay = MockRelay::start();
    // `RFQ_RELAY` runs the swaps through a real relay. The attack scenario always stays on the mock.
    let public_relay = std::env::var("RFQ_RELAY").ok();
    let relay_url = public_relay.clone().unwrap_or_else(|| relay.url());
    let cfg = Config {
        db: db.clone(),
        signer_socket: socket,
        pages: pages.clone(),
        engine,
        liquid,
        second_liquid: None,
        solana_rpc,
        tick_millis: 1_000,
        retry_secs: 8,
        regtest_assets: assets,
        rfq: Some(RfqConfig {
            nostr_key: root.join("nostr.key"),
            relays: vec![relay_url.clone()],
            network: NETWORK.into(),
            min_sol_units: 1,
            max_sol_units: RFQ_MAX,
            show_available_up_to: None,
            l2s_bps: 100,
            s2l_bps: 100,
            quote_secs: QUOTE_SECS,
            l2s_lock_blocks: 300,
            s2l_lock_blocks: 80,
            min_deadline_slots: 5_000,
            // Don't keep writing to a public relay. Offers expire soon after the run.
            offer_every_secs: if public_relay.is_some() { 300 } else { 2 },
            offer_ttl_secs: if public_relay.is_some() { 600 } else { 60 },
            max_requests_per_sender: FLOOD_LIMIT,
            max_requests_per_window: 10_000,
            window_secs: 300,
            max_negotiations: 1_000,
        }),
    };
    let maker_nostr = load_keys(&root.join("nostr.key")).unwrap().public_key();
    let daemon_config = root.join("maker.json");
    std::fs::write(&daemon_config, serde_json::to_string(&cfg).unwrap()).unwrap();

    let taker_wallet = Wallet {
        key: keypair_from(&random32().unwrap()).unwrap(),
        blinding: SecretKey::from_slice(&random32().unwrap()).unwrap(),
    };
    for address in [liquid_address.clone(), taker_wallet.address().to_string()] {
        for _ in 0..8 {
            node.fund(&address, 200_000, "bitcoin");
        }
        node.fund(&address, 40 * LIQ_AMOUNT, &usdt_id);
    }
    node.mine(1);
    let miner = Miner::start(node.wallet_rpc.clone());

    let store = Store::open(&db).unwrap();
    let bot = Bot::start(&relay_url, maker_nostr, chain.clone(), genesis);
    let mut world = World {
        node,
        miner,
        chain,
        sol,
        genesis,
        taker_sol: read_keypair_file(keys_src.join("taker-solana.json")).unwrap(),
        taker_wallet,
        maker_solana: Pubkey::from_str(&solana).unwrap(),
        vault,
        vault_token,
        vault_owner,
        maker_address: liquid_address,
        maker_blinding,
        bot,
        client,
        store,
        daemon: None,
        daemon_config,
        pages,
        _signer: signer,
        fake_asset: AssetId::from_str(&fake).unwrap(),
        relay,
        relay_url,
        maker_nostr,
    };
    world.start_daemon();
    world
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn taker_rules() -> TakerRules {
    TakerRules {
        network: NETWORK.into(),
        program_id: solana_escrow::id().to_string(),
        usdt_asset: usdt().to_string(),
        template_version: msg::TEMPLATE_VERSION,
        l2s_min_blocks: 240,
        l2s_max_blocks: 360,
        s2l_min_blocks: 50,
        s2l_max_blocks: 150,
        max_spread_bps: 100,
        max_deadline_slots: 20_000,
    }
}

fn rfq_request(direction: msg::Direction) -> Request {
    Request { direction, amount_side: AmountSide::Send, amount: Units(RFQ_SEND) }
}

/// The terms the taker will hold the maker to, built from what the taker chose and what it just checked.
fn agreed_terms(s: &SwapTerms, terms: &MakerTerms) -> Res<(SwapTerms, LockPoint)> {
    let maker_key = LockPoint(msg::hex33(&terms.maker_key).ok_or("bad maker key")?);
    let agreed = SwapTerms {
        lock_height: terms.lock_height,
        liquid_amount: terms.liquid_amount.0,
        solana_amount: terms.solana_amount.0,
        ..s.clone()
    };
    Ok((agreed, maker_key))
}

/// E1 over Nostr: L→S, where the taker derives the address itself from the agreed terms, and only then pays it.
fn rfq_l2s_swap(w: &mut World) -> Res<()> {
    let me = Keys::generate();
    let mut relay = Relay::connect(&w.relay_url, Duration::from_millis(200))?;
    let offers = find_offers(&mut relay, NETWORK, Duration::from_secs(5))?;
    check(offers.iter().any(|(k, _)| *k == w.maker_nostr), "the taker found the maker's offer")?;

    let (key, chosen, secret) = fresh_swap_terms(w, Direction::L2S, 0, 0, 0)?;
    let id = chosen.swap_id();
    let side = TakerSide {
        lock_point: chosen.lock_point.0,
        taker_key: chosen.taker_key.0,
        blinding_key: chosen.blinding_key,
        taker_solana: w.taker_sol.pubkey().to_string(),
    };
    let chain = w.chain.clone();
    let tip = move || chain.tip();
    let maker = w.maker_nostr;
    let Negotiated { id: negotiation, quote, terms } = negotiate(
        &mut relay,
        &me,
        &maker,
        &taker_rules(),
        rfq_request(msg::Direction::L2S),
        side,
        tip,
        Duration::from_secs(15),
    )?;
    check(quote.send.0 == RFQ_SEND && quote.receive.0 == RFQ_RECEIVE, "the quote carries the agreed spread")?;
    check(matches!(w.state(&id)?, State::WatchLock | State::LockedSolana { .. }), "the maker registered the swap")?;
    check(terms.liquid_amount.0 == RFQ_SEND * 100, "the taker pays exactly what was quoted")?;
    let (agreed, maker_key) = agreed_terms(&chosen, &terms)?;
    taker_pays_to(w, &agreed.address(&maker_key)?, usdt(), terms.liquid_amount.0)?;

    // The maker offers its claim once the coin confirms. The maker can't use the signature until we reveal the secret.
    let swap = BotSwap { terms: agreed, key, maker_key };
    let got = wait_from(&mut relay, &me, &maker, &negotiation, NETWORK, Duration::from_secs(120))?;
    let Body::ClaimOffer(offer) = got.envelope.body else {
        return Err(format!("expected a claim offer, got {:?}", got.envelope.body));
    };
    let answer = answer_offer(&w.chain, &swap, &offer, w.genesis)?;
    send_body(&mut relay, &me, &maker, &negotiation, NETWORK, Body::ClaimAnswer(answer))?;

    let initiator =
        Pubkey::from_str(terms.maker_initiator.as_deref().ok_or("no initiator")?).map_err(|e| e.to_string())?;
    let escrow_addr = escrow_pda(&initiator, &point_64(&chosen.lock_point));
    let e = w.wait_for("the maker to lock on Solana", 120, || w.sol.escrow(&escrow_addr))?;
    check(
        e.amount == RFQ_RECEIVE && e.recipient == w.taker_sol.pubkey(),
        "the escrow pays the taker the quoted amount",
    )?;
    let before = w.tokens(&w.taker_sol.pubkey())?;
    w.sol.claim(&w.taker_sol, &escrow_addr, &e, solana_value(&secret).map_err(|e| e.to_string())?)?;
    check(w.tokens(&w.taker_sol.pubkey())? == before + RFQ_RECEIVE, "the taker was paid")?;
    w.wait_state(&id, "the maker completing the swap", 120, done(Outcome::Completed))?;
    Ok(())
}

/// E1 over Nostr: S→L.
fn rfq_s2l_swap(w: &mut World) -> Res<()> {
    let me = Keys::generate();
    let mut relay = Relay::connect(&w.relay_url, Duration::from_millis(200))?;
    let (key, chosen, secret) = fresh_swap_terms(w, Direction::S2L, 0, 0, 0)?;
    let id = chosen.swap_id();
    let side = TakerSide {
        lock_point: chosen.lock_point.0,
        taker_key: chosen.taker_key.0,
        blinding_key: chosen.blinding_key,
        taker_solana: w.taker_sol.pubkey().to_string(),
    };
    let chain = w.chain.clone();
    let maker = w.maker_nostr;
    let Negotiated { id: negotiation, terms, .. } = negotiate(
        &mut relay,
        &me,
        &maker,
        &taker_rules(),
        rfq_request(msg::Direction::S2L),
        side,
        move || chain.tip(),
        Duration::from_secs(15),
    )?;
    check(
        terms.liquid_amount.0 == RFQ_RECEIVE * 100 && terms.solana_amount.0 == RFQ_SEND,
        "the amounts match the quote",
    )?;
    let recipient = Pubkey::from_str(&terms.maker_solana).map_err(|e| e.to_string())?;
    let deadline = w.sol.slot()? + terms.min_deadline_slots;
    w.sol.lock(&w.taker_sol, point_64(&chosen.lock_point), recipient, RFQ_SEND, deadline, false)?;

    let (agreed, maker_key) = agreed_terms(&chosen, &terms)?;
    let address = agreed.address(&maker_key)?;
    let blinding = agreed.blinding()?;
    let amount = terms.liquid_amount.0;
    let coin = w.wait_for("the maker to lock on Liquid", 120, || {
        Ok(exact_secret_coin(&w.chain.secret_coins(&address, &blinding)?, usdt(), amount, w.chain.tip()?, 2))
    })?;
    let fee_address = agreed.taker_fee_address()?;
    let fee = w.chain.secret_coins(&fee_address, &blinding)?.into_iter().next().ok_or("no fee coin")?;
    check(fee.secrets.value == FEE && fee.secrets.asset == lbtc(), "the fee coin is what was promised")?;
    let to = w.taker_wallet.address();

    // Real requests on a real coin, so only the sender and id checks can refuse them. The real session below is the control.
    let stranger = Keys::generate();
    check(
        !session_answered(w, &stranger, &negotiation, &agreed, &key, &maker_key, &fee, &coin, SESSION_WAIT)?,
        "a stranger gets nothing from a swap's signing session, even with the right id",
    )?;
    let other_id = msg::hex(&random32()?);
    check(
        !session_answered(w, &me, &other_id, &agreed, &key, &maker_key, &fee, &coin, SESSION_WAIT)?,
        "the swap's own taker gets nothing under another negotiation id",
    )?;

    let claim = session_claim(w, &mut relay, &me, &negotiation, &agreed, &key, &maker_key, &secret, &fee, &coin, &to)?;
    println!("    the claim went through the relay in pieces, and is {} bytes", serialize_hex(&claim).len() / 2);
    w.chain.broadcast(&claim)?;
    w.wait_state(&id, "the maker completing the swap", 120, done(Outcome::Completed))?;
    Ok(())
}

/// The real page in a real browser, against this daemon and node. See client/test/drive.mjs. Only runs with `CLIENT_TEST`,
/// because it needs `npm install` in client/ and a built client (client/build.sh).
fn browser_client(w: &mut World) -> Res<()> {
    if std::env::var("CLIENT_TEST").is_err() {
        println!("    skipped: set CLIENT_TEST=1");
        return Ok(());
    }
    let dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../client"));
    let keys = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../cli/.keys/taker-solana.json"));
    let wallet_args = serde_json::to_string(&w.node.rpc.args).map_err(|e| e.to_string())?;
    let amount = RFQ_SEND as f64 / 1e6;
    let output = Command::new("node")
        .arg(dir.join("test/drive.mjs"))
        .env("ELEMENTS_CLI", &w.node.rpc.cli)
        .env("ELEMENTS_ARGS", wallet_args)
        .env("USDT", usdt().to_string())
        .env("LBTC", lbtc().to_string())
        .env("NETWORK_NAME", NETWORK)
        .env("RELAY_URL", &w.relay_url)
        .env("TAKER_SOLANA_KEY", keys)
        .env("AMOUNT", format!("{amount}"))
        .env("SPLIT_AMOUNT", format!("{}", (RFQ_MAX + RFQ_SEND / 2) as f64 / 1e6))
        .env("SHOTS_DIR", PathBuf::from(env!("CARGO_TARGET_TMPDIR")))
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    print!("{text}");
    if !output.status.success() {
        for (terms, state) in w.store.open_swaps().map_err(|e| e.to_string())? {
            println!("    the maker has {} at {state:?}", terms.lock_point.to_hex());
        }
    }
    check(output.status.success(), "the page ran every scenario")?;
    // The page says it finished. The maker's own store must agree it was paid.
    for line in text.lines().filter_map(|l| l.strip_prefix("SWAP ")) {
        let point = LockPoint::from_hex(line).ok_or("bad swap line")?;
        w.wait_state(&point.id(), "the maker completing the page's swap", 120, done(Outcome::Completed))?;
    }
    println!("    ok: the maker completed each of the page's swaps");
    Ok(())
}

/// A taker speaking the wire protocol by hand, so it can break the rules.
struct RawTaker {
    keys: Keys,
    relay: Relay,
    maker: nostr::prelude::PublicKey,
    id: String,
}

impl RawTaker {
    fn new(w: &World) -> Res<Self> {
        let keys = Keys::generate();
        let mut relay = Relay::connect(&w.relay_url, Duration::from_millis(200))?;
        relay.subscribe("raw", &[Kind::from(msg::WRAP_KIND)], Some(&keys.public_key()), now_secs() - 2 * 86_400)?;
        Ok(Self { keys, relay, maker: w.maker_nostr, id: msg::hex(&random32()?) })
    }

    fn send_at(&mut self, id: &str, network: &str, sent_at: u64, body: Body) -> Res<String> {
        let envelope = Envelope { v: msg::VERSION, id: id.to_string(), network: network.into(), sent_at, body };
        let (event, rumor_id) =
            wrap::wrap(&self.keys, &self.maker, &envelope, now_secs()).map_err(|e| format!("{e:?}"))?;
        self.relay.publish(&event)?;
        Ok(rumor_id)
    }

    fn send(&mut self, body: Body) -> Res<String> {
        let id = self.id.clone();
        self.send_at(&id, NETWORK, now_secs(), body)
    }

    fn answer(&mut self, wait: Duration) -> Res<Option<wrap::Received>> {
        let start = Instant::now();
        while start.elapsed() < wait {
            if let Some(RelayEvent::Event(e)) = self.relay.next_event()? {
                if let Ok(got) = wrap::unwrap(&self.keys, &e) {
                    return Ok(Some(got));
                }
            }
        }
        Ok(None)
    }

    fn quote(&mut self, wait: Duration) -> Res<(msg::Quote, String)> {
        let got = self.answer(wait)?.ok_or("no quote")?;
        match got.envelope.body {
            Body::RfqQuote(q) => Ok((q, got.rumor_id)),
            other => Err(format!("expected a quote, got {other:?}")),
        }
    }
}

fn fresh_id() -> Res<(LockPoint, [u8; 32])> {
    let point = LockPoint(point_of(&random32()?).map_err(|e| e.to_string())?);
    Ok((point, point.id()))
}

fn accept_body(w: &World, quote_event: String, point: LockPoint) -> Res<Body> {
    let key = keypair_from(&random32()?)?;
    Ok(Body::RfqAccept(Accept {
        quote_event,
        lock_point: point.to_hex(),
        taker_key: msg::hex(&key.public_key().serialize()),
        blinding_key: msg::hex(&random32()?),
        taker_solana: w.taker_sol.pubkey().to_string(),
    }))
}

const QUIET: Duration = Duration::from_secs(3);
/// Longer than a session request takes when it is answered, so silence means refused and not slow.
const SESSION_WAIT: Duration = Duration::from_secs(10);

/// E6 and E7 against the real daemon: each of these is a message the maker must not act on.
fn rfq_attacks(w: &mut World) -> Res<()> {
    let request = || Body::RfqRequest(rfq_request(msg::Direction::L2S));
    let known = |w: &World, hash: &[u8; 32]| w.store.state(hash).map(|s| s.is_some()).map_err(|e| e.to_string());

    // Stale and wrong-network messages get no answer at all.
    let mut t = RawTaker::new(w)?;
    let id = t.id.clone();
    t.send_at(&id, NETWORK, now_secs() - 600, request())?;
    check(t.answer(QUIET)?.is_none(), "a stale request gets no answer")?;
    t.send_at(&id, "testnet", now_secs(), request())?;
    check(t.answer(QUIET)?.is_none(), "a request for another network gets no answer")?;

    // A stranger can't accept someone else's quote.
    let mut victim = RawTaker::new(w)?;
    victim.send(request())?;
    let (_, quote_id) = victim.quote(Duration::from_secs(10))?;
    let mut mallory = RawTaker::new(w)?;
    let (point, hash) = fresh_id()?;
    let stolen = accept_body(w, quote_id.clone(), point)?;
    let vid = victim.id.clone();
    mallory.send_at(&vid, NETWORK, now_secs(), stolen)?;
    check(mallory.answer(QUIET)?.is_none(), "a stranger accepting someone's quote gets no answer")?;
    check(!known(w, &hash)?, "and no swap was registered")?;

    // An accept naming the wrong quote is refused.
    let (point, hash) = fresh_id()?;
    victim.send(accept_body(w, "not-the-quote".into(), point)?)?;
    check(victim.answer(QUIET)?.is_none(), "an accept naming the wrong quote gets no answer")?;
    check(!known(w, &hash)?, "and no swap was registered")?;

    // A quote can't be accepted after it expires.
    let mut slow = RawTaker::new(w)?;
    slow.send(request())?;
    let (_, quote_id) = slow.quote(Duration::from_secs(10))?;
    std::thread::sleep(Duration::from_secs(QUOTE_SECS + 2));
    let (point, hash) = fresh_id()?;
    slow.send(accept_body(w, quote_id, point)?)?;
    check(slow.answer(QUIET)?.is_none(), "an expired quote can't be accepted")?;
    check(!known(w, &hash)?, "and no swap was registered")?;

    // The same accept delivered twice gets the same answer, and only one swap.
    let mut twice = RawTaker::new(w)?;
    twice.send(request())?;
    let (_, quote_id) = twice.quote(Duration::from_secs(10))?;
    let (point, hash) = fresh_id()?;
    let body = accept_body(w, quote_id, point)?;
    let rumor = twice.send(body)?;
    let first = twice.answer(Duration::from_secs(10))?.ok_or("no terms")?;
    let Body::RfqTerms(MakerTerms { accept_event, .. }) = &first.envelope.body else {
        return Err("expected terms".into());
    };
    check(*accept_event == rumor, "the terms name this accept")?;
    w.relay.replay_everything();
    std::thread::sleep(QUIET);
    let swaps = w.store.open_swaps().map_err(|e| e.to_string())?.iter().filter(|(t, _)| t.swap_id() == hash).count();
    check(swaps == 1, "a relay replaying everything creates no second swap")?;

    // A point that isn't on the curve can't be locked to or claimed, so no swap is made of it.
    let mut off_curve = RawTaker::new(w)?;
    off_curve.send(request())?;
    let (_, quote_id) = off_curve.quote(Duration::from_secs(10))?;
    let Body::RfqAccept(good) = accept_body(w, quote_id, fresh_id()?.0)? else { unreachable!() };
    let not_a_point = format!("02{}", "ff".repeat(32));
    off_curve.send(Body::RfqAccept(Accept { lock_point: not_a_point.clone(), ..good }))?;
    let answer = off_curve.answer(Duration::from_secs(10))?.ok_or("no answer")?;
    check(matches!(answer.envelope.body, Body::RfqReject(_)), "a lock point off the curve is declined")?;
    check(!known(w, &LockPoint::from_hex(&not_a_point).ok_or("hex")?.id())?, "and no swap was registered")?;

    // A point can never be reused: a new negotiation offering the same point is declined.
    let mut again = RawTaker::new(w)?;
    again.send(request())?;
    let (_, quote_id) = again.quote(Duration::from_secs(10))?;
    again.send(accept_body(w, quote_id, point)?)?;
    let answer = again.answer(Duration::from_secs(10))?.ok_or("no answer")?;
    check(matches!(answer.envelope.body, Body::RfqReject(_)), "a reused lock point is declined")?;

    // A taker's message types from a taker are ignored.
    let mut odd = RawTaker::new(w)?;
    odd.send(Body::RfqQuote(msg::Quote { send: Units(1), receive: Units(1), quote_expiry: 0, taker_lock_window: 0 }))?;
    check(odd.answer(QUIET)?.is_none(), "a quote sent to the maker gets no answer")?;

    // A flood from one key is cut off, and others are still served.
    let mut flood = RawTaker::new(w)?;
    let mut answered = 0;
    for _ in 0..FLOOD_LIMIT + 10 {
        flood.id = msg::hex(&random32()?);
        flood.send(request())?;
    }
    while flood.answer(Duration::from_secs(2))?.is_some() {
        answered += 1;
    }
    check(answered <= FLOOD_LIMIT, "a flood from one key is rate limited")?;
    let mut other = RawTaker::new(w)?;
    other.send(request())?;
    check(other.quote(Duration::from_secs(10)).is_ok(), "and other takers are still served")?;

    // A relay dropping everything costs time, and moves no funds.
    w.relay.drop_all(true);
    let me = Keys::generate();
    let mut relay = Relay::connect(&w.relay.url(), Duration::from_millis(200))?;
    let (point, hash) = fresh_id()?;
    let side = TakerSide {
        lock_point: point.0,
        taker_key: keypair_from(&random32()?)?.public_key().serialize(),
        blinding_key: random32()?,
        taker_solana: w.taker_sol.pubkey().to_string(),
    };
    let chain = w.chain.clone();
    let maker = w.maker_nostr;
    let dropped = negotiate(
        &mut relay,
        &me,
        &maker,
        &taker_rules(),
        rfq_request(msg::Direction::L2S),
        side,
        move || chain.tip(),
        QUIET,
    );
    w.relay.drop_all(false);
    check(dropped.is_err(), "the taker gives up cleanly when a relay drops everything")?;
    check(!known(w, &hash)?, "and no swap exists")?;

    // Forgery of the maker: a taker ignores answers from anyone but the maker it chose.
    let me = Keys::generate();
    let mut relay = Relay::connect(&w.relay.url(), Duration::from_millis(200))?;
    relay.subscribe("forge", &[Kind::from(msg::WRAP_KIND)], Some(&me.public_key()), now_secs() - 2 * 86_400)?;
    let forger = Keys::generate();
    let fake = Envelope {
        v: msg::VERSION,
        id: msg::hex(&[1; 32]),
        network: NETWORK.into(),
        sent_at: now_secs(),
        body: Body::RfqQuote(msg::Quote {
            send: Units(RFQ_SEND),
            receive: Units(RFQ_SEND),
            quote_expiry: now_secs() + 60,
            taker_lock_window: 30,
        }),
    };
    let (event, _) = wrap::wrap(&forger, &me.public_key(), &fake, now_secs()).map_err(|e| format!("{e:?}"))?;
    w.relay.inject(event);
    let start = Instant::now();
    let mut seen = None;
    while start.elapsed() < QUIET && seen.is_none() {
        if let Some(RelayEvent::Event(e)) = relay.next_event()? {
            seen = wrap::unwrap(&me, &e).ok();
        }
    }
    let got = seen.ok_or("the forged message never arrived")?;
    check(
        got.sender == forger.public_key() && got.sender != w.maker_nostr,
        "a forged quote is attributed to its real author, so a taker can't mistake it",
    )?;
    Ok(())
}

#[test]
#[ignore = "needs elementsd and funded wallets; see the module docs"]
fn the_daemon_runs_real_swaps() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut world = build_world(&root);
    type Scenario = (&'static str, fn(&mut World) -> Res<()>);
    let scenarios: Vec<Scenario> = vec![
        ("L2S with crashes (E1, E17)", l2s_with_crashes),
        ("S2L with crashes (E1, E17)", s2l_with_crashes),
        ("crash between commit and send (E17)", crash_between_commit_and_send),
        ("L2S taker vanishes (E3)", l2s_taker_vanishes),
        ("L2S taker never signs", l2s_taker_never_signs),
        ("S2L taker vanishes (E3)", s2l_taker_vanishes),
        ("bad locks (E9)", bad_locks),
        ("Liquid halt extends (E11)", liquid_halt_extends),
        ("rollback (E15)", rollback_stops_everything),
        ("RFQ L2S swap (E1)", rfq_l2s_swap),
        ("RFQ S2L swap (E1)", rfq_s2l_swap),
        ("RFQ attacks (E6, E7)", rfq_attacks),
        ("browser client", browser_client),
    ];
    let filter = std::env::var("SCENARIO").unwrap_or_default();
    let mut failures = vec![];
    for (name, run) in scenarios {
        if !name.contains(&filter) {
            continue;
        }
        println!("\n==> {name}");
        let start = Instant::now();
        match run(&mut world) {
            Ok(()) => println!("PASS {name} ({}s)", start.elapsed().as_secs()),
            Err(e) => {
                println!("FAIL {name}: {e}");
                failures.push(name);
            }
        }
        if world.daemon.is_none() {
            world.start_daemon();
        }
    }
    // Give back what's left in the vault, so repeated runs don't drain the test wallet.
    let left = world.sol.token_balance(&world.vault_token).unwrap_or(0);
    if left > 0 {
        let owner = world.vault_owner.pubkey();
        let accounts = anchor_lang::ToAccountMetas::to_account_metas(
            &solana_escrow::accounts::VaultWithdraw {
                owner,
                maker_vault: world.vault,
                vault_token: world.vault_token,
                to: ata(&world.maker_solana),
                token_program: anchor_lang::prelude::Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
                    .unwrap(),
            },
            None,
        );
        let data = anchor_lang::InstructionData::data(&solana_escrow::instruction::VaultWithdraw { amount: left });
        let ix = solana_sdk::instruction::Instruction { program_id: solana_escrow::id(), accounts, data };
        match world.sol.send(&[ix], &world.vault_owner, &[]) {
            Ok(_) => println!("\nreturned {left} units from the vault"),
            Err(e) => println!("\ncould not empty the vault: {}", e.chars().take(120).collect::<String>()),
        }
    }
    // And the owner's SOL, because this throwaway key is gone after the run.
    let owner = world.vault_owner.pubkey();
    if let Ok(balance) = world.sol.lamports(&owner) {
        let mut transfer = vec![2, 0, 0, 0];
        transfer.extend_from_slice(&balance.to_le_bytes());
        let ix = solana_sdk::instruction::Instruction {
            program_id: anchor_lang::system_program::ID,
            accounts: vec![
                solana_sdk::instruction::AccountMeta::new(owner, true),
                solana_sdk::instruction::AccountMeta::new(world.maker_solana, true),
            ],
            data: transfer,
        };
        let payer = read_keypair_file(root.join("keys/solana.json")).unwrap();
        let _ = world.sol.send(&[ix], &payer, &[&world.vault_owner]);
    }
    drop(world);
    let _ = std::fs::remove_dir_all(&root);
    assert!(failures.is_empty(), "failed: {failures:?}");
}
