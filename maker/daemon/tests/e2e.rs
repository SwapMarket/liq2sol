//! The real daemon and signer processes, against a regtest node and a real Solana cluster, with a simulated taker.
//! Scenarios E1, E3, E5, E9, E11, E15 and E17 from docs/test-plan.md: each checks where the money ended up.
//!
//!   ELEMENTSD=~/bin/elementsd ELEMENTS_CLI=~/bin/elements-cli SOLANA_RPC=https://api.devnet.solana.com \
//!     cargo test -p liq2sol-maker --test e2e -- --ignored --nocapture
//!
//! Needs `cli/.keys` from `liq2sol-cli setup`, holding SOL and stand-in tokens. `SCENARIO` runs a subset.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anchor_lang::prelude::Pubkey;
use liq2sol_chain::liquid::{
    address_of, exact_coin, keypair_from, lbtc, pay, random32, spend_htlc, txout, usdt, use_network, Coin, Esplora,
    HtlcSpend, NetworkConfig, Rpc, FEE,
};
use liq2sol_chain::sol::{self, ata, escrow_pda, Sol, MAINNET_GENESIS};
use liq2sol_htlc::{Htlc, HtlcParams};
use liq2sol_maker::{accept, Config, Details};
use liq2sol_maker_core::engine::{Action, Config as EngineConfig, Direction, Outcome, State, Terms};
use liq2sol_maker_core::hash_preimage;
use liq2sol_maker_core::policy::Limits;
use liq2sol_maker_core::store::Store;
use liq2sol_signer::{
    l2s_params, s2l_params, Call, Client, Config as SignerConfig, L2sTerms, LiquidSource, Reply, S2lTerms,
};
use simplicityhl::elements::secp256k1_zkp::Keypair as LiquidKey;
use simplicityhl::elements::{AddressParams, AssetId, BlockHash, Script, Transaction as LiquidTx};
use solana_escrow::state::EscrowState;
use solana_sdk::signature::{read_keypair_file, Keypair};
use solana_sdk::signer::Signer as _;

const SOL_AMOUNT: u64 = 10;
const LIQ_AMOUNT: u64 = SOL_AMOUNT * 100;
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
    taker_wallet: LiquidKey,
    maker_solana: Pubkey,
    /// Our escrows are seeded with the vault, and draw from its token account.
    vault: Pubkey,
    vault_token: Pubkey,
    vault_owner: Keypair,
    maker_script: Script,
    maker_address: String,
    client: Client,
    store: Store,
    daemon: Option<Proc>,
    daemon_config: PathBuf,
    pages: PathBuf,
    _signer: Proc,
    fake_asset: AssetId,
}

impl World {
    fn start_daemon(&mut self) {
        let child = Command::new(env!("CARGO_BIN_EXE_liq2sol-maker"))
            .arg(&self.daemon_config)
            .stdout(Stdio::null())
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

    fn holdings(&self, key_address: &simplicityhl::elements::Address) -> Res<u64> {
        Ok(self
            .chain
            .coins(key_address)?
            .iter()
            .filter(|c| c.out.asset.explicit() == Some(usdt()))
            .filter_map(|c| c.out.value.explicit())
            .sum())
    }

    fn maker_holdings(&self) -> Res<u64> {
        let address = simplicityhl::elements::Address::from_str(&self.maker_address).map_err(|e| format!("{e:?}"))?;
        self.holdings(&address)
    }

    fn tokens(&self, owner: &Pubkey) -> Res<u64> {
        self.sol.token_balance(&ata(owner))
    }

    /// The Solana funds the maker controls: its vault, plus anything collected and not yet swept.
    fn maker_tokens(&self) -> Res<u64> {
        Ok(self.sol.token_balance(&self.vault_token)? + self.tokens(&self.maker_solana)?)
    }

    fn swap_key(&mut self, hash: [u8; 32]) -> [u8; 32] {
        let Reply::SwapKey { x_only } = self.client.call(&Call::SwapKey { payment_hash: hash }).unwrap() else {
            panic!()
        };
        x_only
    }
}

/// One simulated taker's side of an L→S swap.
struct L2s {
    preimage: [u8; 32],
    hash: [u8; 32],
    params: HtlcParams,
    escrow: Pubkey,
}

fn new_l2s(w: &mut World, lock_in: u32) -> Res<L2s> {
    let preimage = random32()?;
    let hash = hash_preimage(&preimage);
    let refund_key = keypair_from(&random32()?)?;
    let tip = w.chain.tip()?;
    let details = L2sTerms {
        payment_hash: hash,
        taker_pk: refund_key.x_only_public_key().0.serialize(),
        lock_height: tip + lock_in,
        liquid_amount: LIQ_AMOUNT,
        solana_amount: SOL_AMOUNT,
        taker_solana: w.taker_sol.pubkey().to_string(),
    };
    let key = w.swap_key(hash);
    let params = l2s_params(&details, key, &w.maker_script);
    let terms = Terms {
        direction: Direction::L2S,
        payment_hash: hash,
        liquid_amount: LIQ_AMOUNT,
        solana_amount: SOL_AMOUNT,
        h_accept: tip,
        lock_height: tip + lock_in,
    };
    check(accept(&w.store, &terms, &Details::L2S(details))?, "the maker accepted the swap")?;
    Ok(L2s { preimage, hash, params, escrow: escrow_pda(&w.vault, &hash) })
}

/// One simulated taker's side of an S→L swap.
struct S2l {
    preimage: [u8; 32],
    hash: [u8; 32],
    claim_key: LiquidKey,
    fee_key: LiquidKey,
    params: HtlcParams,
    escrow: Pubkey,
    to: Script,
}

fn new_s2l(w: &mut World, lock_in: u32) -> Res<S2l> {
    let preimage = random32()?;
    let hash = hash_preimage(&preimage);
    let (claim_key, fee_key) = (keypair_from(&random32()?)?, keypair_from(&random32()?)?);
    let to = address_of(&w.taker_wallet).script_pubkey();
    let tip = w.chain.tip()?;
    let details = S2lTerms {
        payment_hash: hash,
        taker_pk: claim_key.x_only_public_key().0.serialize(),
        taker_script: to.to_bytes(),
        taker_fee_script: address_of(&fee_key).script_pubkey().to_bytes(),
        lock_height: tip + lock_in,
        liquid_amount: LIQ_AMOUNT,
        taker_solana: w.taker_sol.pubkey().to_string(),
    };
    let key = w.swap_key(hash);
    let params = s2l_params(&details, key);
    let terms = Terms {
        direction: Direction::S2L,
        payment_hash: hash,
        liquid_amount: LIQ_AMOUNT,
        solana_amount: SOL_AMOUNT,
        h_accept: tip,
        lock_height: tip + lock_in,
    };
    check(accept(&w.store, &terms, &Details::S2L(details))?, "the maker accepted the swap")?;
    Ok(S2l { preimage, hash, claim_key, fee_key, params, escrow: escrow_pda(&w.taker_sol.pubkey(), &hash), to })
}

fn htlc_address(params: &HtlcParams) -> simplicityhl::elements::Address {
    Htlc::new(params).unwrap().address(&AddressParams::ELEMENTS)
}

fn taker_pays(w: &World, s: &L2s) -> Res<()> {
    pay(&w.chain, &w.taker_wallet, vec![txout(usdt(), LIQ_AMOUNT, htlc_address(&s.params).script_pubkey())], w.genesis)
        .map(|_| ())
}

fn htlc_coin(w: &World, params: &HtlcParams, confs: u32) -> Res<Option<Coin>> {
    Ok(exact_coin(&w.chain.coins(&htlc_address(params))?, usdt(), LIQ_AMOUNT, w.chain.tip()?, confs))
}

fn locked(s: &State) -> bool {
    matches!(s, State::LockedSolana { .. } | State::LockedLiquid)
}

fn done(outcome: Outcome) -> impl Fn(&State) -> bool {
    move |s| *s == State::Done { outcome }
}

/// E1 and E17: a whole L→S swap, with the daemon killed at the two points that matter most.
fn l2s_with_crashes(w: &mut World) -> Res<()> {
    let (maker_before, taker_before) = (w.maker_holdings()?, w.tokens(&w.taker_sol.pubkey())?);
    let s = new_l2s(w, 300)?;
    taker_pays(w, &s)?;
    w.wait_state(&s.hash, "the maker's Solana lock", 120, locked)?;
    w.kill_daemon();
    let e = w.wait_for("the lock to land anyway, or be retried", 30, || w.sol.escrow(&s.escrow)).or_else(|_| {
        w.start_daemon();
        w.wait_for("the lock after the restart", 120, || w.sol.escrow(&s.escrow))
    })?;
    if w.daemon.is_none() {
        w.start_daemon();
    }
    check(e.close_on_restart && e.amount == SOL_AMOUNT, "the escrow has the right terms")?;

    w.sol.claim(&w.taker_sol, &s.escrow, &e, s.preimage)?;
    check(w.tokens(&w.taker_sol.pubkey())? == taker_before + SOL_AMOUNT, "the taker was paid on Solana")?;
    w.kill_daemon();
    w.start_daemon();
    w.wait_state(&s.hash, "completion after the restart", 180, done(Outcome::Completed))?;
    check(w.maker_holdings()? == maker_before + LIQ_AMOUNT, "the maker was paid on Liquid, exactly once")?;
    Ok(())
}

/// E1 and E17: a whole S→L swap, with crashes. The key check is that the HTLC is only ever paid once.
fn s2l_with_crashes(w: &mut World) -> Res<()> {
    let (maker_before, vault_before) = (w.maker_holdings()?, w.sol.token_balance(&w.vault_token)?);
    let s = new_s2l(w, 80)?;
    let deadline = w.sol.slot()? + 5_000;
    w.sol.lock(&w.taker_sol, s.hash, w.maker_solana, SOL_AMOUNT, deadline, false)?;
    w.wait_state(&s.hash, "the maker's Liquid lock", 120, locked)?;
    w.kill_daemon();
    w.start_daemon();
    let coin = w.wait_for("the lock to confirm", 120, || htlc_coin(w, &s.params, 2))?;
    std::thread::sleep(Duration::from_secs(8)); // Long enough for a buggy retry to pay a second time.
    check(w.chain.coins(&htlc_address(&s.params))?.len() == 1, "the HTLC was paid exactly once, despite the crash")?;
    check(w.maker_holdings()? == maker_before - LIQ_AMOUNT, "the maker paid exactly the agreed amount")?;

    let fee = w.chain.coins(&address_of(&s.fee_key))?.into_iter().next().ok_or("no fee coin from the maker")?;
    check(fee.out.value.explicit() == Some(FEE), "the maker sent the taker a fee coin")?;
    let how = HtlcSpend::Claim { preimage: s.preimage, recipient: s.claim_key, to: s.to.clone() };
    spend_htlc(&w.chain, &s.params, &coin, &fee, &s.fee_key, how, w.genesis)?;
    w.kill_daemon();
    std::thread::sleep(Duration::from_secs(4));
    w.start_daemon();
    w.wait_state(&s.hash, "completion after the restart", 180, done(Outcome::Completed))?;
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
    // L→S: the daemon "decided" to lock on Solana, then died before sending anything.
    let s = new_l2s(w, 300)?;
    w.kill_daemon();
    taker_pays(w, &s)?;
    let coin_seen = w.wait_for("the taker's coin to confirm", 60, || htlc_coin(w, &s.params, 2))?;
    let deadline_slot = w.sol.slot()? + 150;
    let action = Action::LockSolana { deadline_slot };
    w.store.commit_step(&s.hash, &State::LockedSolana { deadline_slot }, &[action]).map_err(|e| e.to_string())?;
    check(w.sol.escrow(&s.escrow)?.is_none(), "nothing was sent before the crash")?;
    w.start_daemon();
    let e = w.wait_for("the lock to be sent after the restart", 90, || w.sol.escrow(&s.escrow))?;
    check(e.deadline_slot == deadline_slot, "with the deadline decided before the crash")?;
    w.sol.claim(&w.taker_sol, &s.escrow, &e, s.preimage)?;
    w.wait_state(&s.hash, "completion", 120, done(Outcome::Completed))?;
    check(w.chain.spender(&coin_seen.outpoint)?.is_some(), "the maker collected its coin")?;

    // S→L: the same, on the side that could pay twice.
    let s = new_s2l(w, 80)?;
    let maker_before = w.maker_holdings()?;
    let deadline = w.sol.slot()? + 5_000;
    w.kill_daemon();
    w.sol.lock(&w.taker_sol, s.hash, w.maker_solana, SOL_AMOUNT, deadline, false)?;
    w.store.commit_step(&s.hash, &State::LockedLiquid, &[Action::LockLiquid]).map_err(|e| e.to_string())?;
    check(htlc_coin(w, &s.params, 0)?.is_none(), "nothing was paid before the crash")?;
    w.start_daemon();
    let coin = w.wait_for("the lock to be paid after the restart", 90, || htlc_coin(w, &s.params, 2))?;
    std::thread::sleep(Duration::from_secs(12)); // Longer than the retry interval, so a second payment would show.
    check(w.chain.coins(&htlc_address(&s.params))?.len() == 1, "the HTLC was paid exactly once")?;
    check(w.maker_holdings()? == maker_before - LIQ_AMOUNT, "for exactly the agreed amount")?;
    let fee = w.chain.coins(&address_of(&s.fee_key))?.into_iter().next().ok_or("no fee coin")?;
    let how = HtlcSpend::Claim { preimage: s.preimage, recipient: s.claim_key, to: s.to.clone() };
    spend_htlc(&w.chain, &s.params, &coin, &fee, &s.fee_key, how, w.genesis)?;
    w.wait_state(&s.hash, "completion", 120, done(Outcome::Completed))?;
    Ok(())
}

/// E3: the taker pays, then vanishes. The maker gets its Solana funds back at the deadline by itself.
fn l2s_taker_vanishes(w: &mut World) -> Res<()> {
    let before = w.maker_tokens()?;
    let s = new_l2s(w, 300)?;
    taker_pays(w, &s)?;
    w.wait_state(&s.hash, "the maker's Solana lock", 120, locked)?;
    w.wait_for("the escrow to appear", 60, || w.sol.escrow(&s.escrow))?;
    check(w.maker_tokens()? == before - SOL_AMOUNT, "the funds are locked, drawn from the vault")?;
    w.wait_state(&s.hash, "the maker's automatic refund", 180, done(Outcome::Refunded))?;
    check(w.maker_tokens()? == before, "the refund went back to the vault")?;
    Ok(())
}

/// E3: the taker locks, then vanishes. The maker refunds on Liquid at the lock height, and the taker on Solana later.
fn s2l_taker_vanishes(w: &mut World) -> Res<()> {
    let maker_before = w.maker_holdings()?;
    let taker_before = w.tokens(&w.taker_sol.pubkey())?;
    let s = new_s2l(w, 35)?;
    let deadline = w.sol.slot()? + 400;
    w.sol.lock(&w.taker_sol, s.hash, w.maker_solana, SOL_AMOUNT, deadline, false)?;
    w.wait_state(&s.hash, "the maker's Liquid lock", 60, locked)?;
    w.wait_state(&s.hash, "the maker's automatic refund", 300, done(Outcome::Refunded))?;
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
    let address = htlc_address(&s.params);
    pay(&w.chain, &w.taker_wallet, vec![txout(usdt(), LIQ_AMOUNT - 100, address.script_pubkey())], w.genesis)?;
    w.node.fund(&address.to_string(), LIQ_AMOUNT, &w.fake_asset.to_string());
    w.wait_state(&s.hash, "giving up on a short coin and a fake asset", 240, |st| *st == State::Abandoned)?;
    check(w.sol.escrow(&s.escrow)?.is_none(), "no escrow was ever created")?;
    check(w.maker_tokens()? == tokens, "no maker funds moved")?;

    let maker_before = w.maker_holdings()?;
    let taker_before = w.tokens(&w.taker_sol.pubkey())?;
    let s = new_s2l(w, 80)?;
    let deadline = w.sol.slot()? + 5_000;
    w.sol.lock(&w.taker_sol, s.hash, w.maker_solana, SOL_AMOUNT - 1, deadline, false)?;
    w.wait_state(&s.hash, "giving up on a short escrow", 240, |st| *st == State::Abandoned)?;
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
    w.sol.lock(&w.taker_sol, s.hash, w.maker_solana, SOL_AMOUNT, deadline, false)?;
    w.wait_state(&s.hash, "the maker's Liquid lock", 60, locked)?;
    let coin = w.wait_for("the lock to confirm", 90, || htlc_coin(w, &s.params, 2))?;

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
    let fee = w.chain.coins(&address_of(&s.fee_key))?.into_iter().next().ok_or("no fee coin")?;
    let how = HtlcSpend::Claim { preimage: s.preimage, recipient: s.claim_key, to: s.to.clone() };
    spend_htlc(&w.chain, &s.params, &coin, &fee, &s.fee_key, how, w.genesis)?;
    w.wait_state(&s.hash, "completion after the halt", 180, done(Outcome::Completed))?;
    Ok(())
}

/// E15: a rollback past the taker's claim. The daemon stops and pages, and doesn't take the coin back.
fn rollback_stops_everything(w: &mut World) -> Res<()> {
    let s = new_s2l(w, 45)?;
    let deadline = w.sol.slot()? + 5_000;
    w.sol.lock(&w.taker_sol, s.hash, w.maker_solana, SOL_AMOUNT, deadline, false)?;
    w.wait_state(&s.hash, "the maker's Liquid lock", 60, locked)?;
    let coin = w.wait_for("the lock to confirm", 90, || htlc_coin(w, &s.params, 2))?;
    let fee = w.chain.coins(&address_of(&s.fee_key))?.into_iter().next().ok_or("no fee coin")?;
    let how = HtlcSpend::Claim { preimage: s.preimage, recipient: s.claim_key, to: s.to.clone() };
    let claim_id = spend_htlc(&w.chain, &s.params, &coin, &fee, &s.fee_key, how, w.genesis)?;
    let claim_tx: LiquidTx = w.chain.tx(&claim_id)?;
    w.wait_state(&s.hash, "the maker collecting on Solana", 120, done(Outcome::Completed))?;
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
    let lock_height = s.params.lock_height;
    w.node.roll_back_to_before(claim_height);
    w.start_daemon();
    w.wait_for("the daemon to raise the rollback alarm", 60, || {
        Ok(std::fs::read_to_string(&w.pages).ok().filter(|t| t.contains("Rollback")))
    })?;
    check(w.store.rollback().map_err(|e| e.to_string())?, "the alarm is latched")?;
    check(w.chain.spender(&coin.outpoint)?.is_none(), "the claim is gone, so the coin looks unspent")?;

    // A coin that really is there, confirmed after the rollback: everything but the alarm says to lock.
    let waiting = new_l2s(w, 300)?;
    taker_pays(w, &waiting)?;
    w.node.mine(2);
    check(htlc_coin(w, &waiting.params, 2)?.is_some(), "a confirmed coin is waiting")?;
    std::thread::sleep(Duration::from_secs(10));
    check(w.state(&waiting.hash)? == State::WatchLock, "the alarm stops a lock that would otherwise go ahead")?;
    check(w.sol.escrow(&waiting.escrow)?.is_none(), "no escrow was created")?;
    check(w.maker_tokens()? == tokens, "no Solana funds moved")?;

    // An operator clears the alarm, with the real command. It must stay clear, and the waiting swap goes ahead.
    let status = Command::new(env!("CARGO_BIN_EXE_liq2sol-maker"))
        .args(["clear-rollback", w.daemon_config.to_str().ok_or("path")?])
        .stdout(Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    check(status.success(), "the operator cleared the alarm")?;
    w.wait_state(&waiting.hash, "the waiting swap going ahead once the alarm is cleared", 60, locked)?;
    check(!w.store.rollback().map_err(|e| e.to_string())?, "the alarm stayed clear")?;
    w.wait_for("its lock to land on Solana", 60, || w.sol.escrow(&waiting.escrow))?;

    // Past the lock height, with the coin unspent and no alarm, an honest maker still leaves it alone.
    while w.chain.tip()? < lock_height {
        w.node.mine(5);
    }
    std::thread::sleep(Duration::from_secs(10));
    check(w.chain.spender(&coin.outpoint)?.is_none(), "the maker didn't refund a coin it already collected for")?;
    w.chain.broadcast(&claim_tx)?;
    w.node.mine(1);
    let spend = w.chain.spender(&coin.outpoint)?.ok_or("not spent")?;
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
    transfer.extend_from_slice(&50_000_000u64.to_le_bytes());
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
    let deposit = sol::vault_deposit_ix(&operator.pubkey(), &vault_owner.pubkey(), 20 * SOL_AMOUNT);
    sol.send(&[deposit], &operator, &[]).unwrap();
    let private = root.join("private");
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
    let Reply::Identity { liquid_address, liquid_script, solana, escrow_initiator } =
        client.call(&Call::Identity).unwrap()
    else {
        panic!()
    };
    assert_eq!(escrow_initiator, vault.to_string(), "the signer runs on the vault");

    let pages = root.join("pages.log");
    let db = root.join("maker.db");
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
    };
    let daemon_config = root.join("maker.json");
    std::fs::write(&daemon_config, serde_json::to_string(&cfg).unwrap()).unwrap();

    let taker_wallet = keypair_from(&random32().unwrap()).unwrap();
    for address in [liquid_address.clone(), address_of(&taker_wallet).to_string()] {
        for _ in 0..8 {
            node.fund(&address, 200_000, "bitcoin");
        }
        node.fund(&address, 40 * LIQ_AMOUNT, &usdt_id);
    }
    node.mine(1);
    let miner = Miner::start(node.wallet_rpc.clone());

    let store = Store::open(&db).unwrap();
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
        maker_script: Script::from(liquid_script),
        maker_address: liquid_address,
        client,
        store,
        daemon: None,
        daemon_config,
        pages,
        _signer: signer,
        fake_asset: AssetId::from_str(&fake).unwrap(),
    };
    world.start_daemon();
    let _ = lbtc();
    world
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
        ("S2L taker vanishes (E3)", s2l_taker_vanishes),
        ("bad locks (E9)", bad_locks),
        ("Liquid halt extends (E11)", liquid_halt_extends),
        ("rollback (E15)", rollback_stops_everything),
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
    drop(world);
    let _ = std::fs::remove_dir_all(&root);
    assert!(failures.is_empty(), "failed: {failures:?}");
}
