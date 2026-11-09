//! Failure scenarios from docs/test-plan.md section 5, on a real Elements regtest node and a real Solana cluster.
//!
//! Regtest gives full control of Liquid: not mining is a halt, and `invalidateblock` plus a restart with an empty
//! mempool is a rollback. Solana's clock keeps running throughout, which is exactly the condition these exercise.
//! Some scenarios end with someone losing funds. That is the point: they demonstrate the residual risks that
//! docs/protocol.md section 7 claims, and they fail if the behaviour ever changes.
//!
//!   ELEMENTSD=~/bin/elementsd ELEMENTS_CLI=~/bin/elements-cli SOLANA_RPC=https://api.devnet.solana.com \
//!     cargo test --test failures -- --ignored --nocapture
//!
//! Needs the test wallets from `liq2sol-cli setup` to hold devnet SOL and stand-in tokens.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use std::cell::RefCell;

use anchor_lang::prelude::Pubkey;
use liq2sol_chain::liquid::{
    build_swap_spend, exact_secret_coin, keypair_from, lbtc, published_signature, random32, read_output, sign_keypath,
    usdt, use_network, NetworkConfig, Payment, Res, SecretCoin, Wallet, FEE,
};
use liq2sol_chain::esplora::{build_confidential_pay, Esplora, Rpc};
use liq2sol_chain::sol::{ata, vault_pda, Sol, MAINNET_GENESIS};
use liq2sol_swapkey::{
    combine, complete, extract_secret, key_spend_sighash, point_of, secret_from_solana, set_key_spend_witness,
    solana_point, solana_value, verify_share, Adaptor, Session, SwapOutput, SwapParams,
};
use simplicityhl::elements::secp256k1_zkp::{Keypair as LiquidKey, Message, PublicKey, Secp256k1, SecretKey};
use simplicityhl::elements::{Address, AddressParams, AssetId, BlockHash, OutPoint, Sequence, Transaction, Txid};
use solana_escrow::state::{Escrow, EscrowState};
use solana_sdk::signature::{read_keypair_file, Keypair};
use solana_sdk::signer::Signer;

const SOL_AMOUNT: u64 = 10;
const LIQ_AMOUNT: u64 = SOL_AMOUNT * 100;
/// Short on purpose. About 30 seconds at devnet's 240 ms per slot.
const WINDOW: u64 = 120;

struct Node {
    child: Option<Child>,
    daemon: String,
    dir: PathBuf,
    rpc: Rpc,
    port: u16,
}

impl Node {
    fn start() -> Res<Self> {
        let daemon = std::env::var("ELEMENTSD").unwrap_or_else(|_| "elementsd".into());
        let cli = std::env::var("ELEMENTS_CLI").unwrap_or_else(|_| "elements-cli".into());
        let port = 20_000 + (std::process::id() % 20_000) as u16;
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("failures-{port}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let args = ["-chain=liquidregtest", "-rpcuser=t", "-rpcpassword=t", &format!("-rpcport={port}"), "-rpcwallet=w"]
            .map(String::from)
            .to_vec();
        let mut node = Node { child: None, daemon, dir, rpc: Rpc { cli, args }, port };
        node.spawn()?;
        node.rpc.call(&["createwallet", "w"])?;
        node.rpc.call(&["rescanblockchain"])?;
        Ok(node)
    }

    fn spawn(&mut self) -> Res<()> {
        let child = Command::new(&self.daemon)
            .arg(format!("-datadir={}", self.dir.display()))
            .args(["-chain=liquidregtest", "-listen=0", "-txindex=1", "-validatepegin=0", "-fallbackfee=0.00001"])
            .args(["-initialfreecoins=2100000000", "-anyonecanspendaremine=1", "-evbparams=simplicity:-1:::"])
            .arg("-acceptdiscountct=1")
            .args(["-persistmempool=0", "-rpcuser=t", "-rpcpassword=t", &format!("-rpcport={}", self.port), "-printtoconsole=0"])
            .stdout(Stdio::null())
            .spawn()
            .map_err(|e| format!("start elementsd: {e}"))?;
        self.child = Some(child);
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(60) {
            let probe = Rpc { cli: self.rpc.cli.clone(), args: self.rpc.args[..4].to_vec() };
            if probe.call(&["getblockcount"]).is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        Err("elementsd did not start".into())
    }

    fn stop(&mut self) {
        let probe = Rpc { cli: self.rpc.cli.clone(), args: self.rpc.args[..4].to_vec() };
        let _ = probe.call(&["stop"]);
        if let Some(mut c) = self.child.take() {
            let _ = c.wait();
        }
    }

    /// A rollback as the federation would do it: mark the block invalid, and forget its transactions.
    fn roll_back_to_before(&mut self, height: u32) -> Res<()> {
        let hash = self.rpc.call(&["getblockhash", &height.to_string()])?;
        self.rpc.call(&["invalidateblock", &hash])?;
        self.stop();
        self.spawn()?;
        let probe = Rpc { cli: self.rpc.cli.clone(), args: self.rpc.args[..4].to_vec() };
        probe.call(&["loadwallet", "w"])?;
        Ok(())
    }

    fn mine(&self, blocks: u32) -> Res<()> {
        let address = self.rpc.call(&["getnewaddress"])?;
        self.rpc.call(&["generatetoaddress", &blocks.to_string(), &address]).map(|_| ())
    }

    /// A node wallet payment to a confidential address, so the coin is blinded like every other here.
    fn fund(&self, address: &Address, sats: u64, label: &str) -> Res<()> {
        let amount = format!("{}.{:08}", sats / 100_000_000, sats % 100_000_000);
        self.rpc
            .call(&["sendtoaddress", &address.to_string(), &amount, "", "", "false", "false", "1", "UNSET", "false", label])
            .map(|_| ())
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Env {
    node: Node,
    chain: Esplora,
    sol: Sol,
    genesis: BlockHash,
    taker_sol: Keypair,
    maker_sol: Keypair,
    mint_authority: Keypair,
    fake_asset: AssetId,
}

struct Terms {
    output: SwapOutput,
    address: Address,
    blinding: SecretKey,
    lock_height: u32,
    secret: [u8; 32],
    /// What a claim reveals on Solana.
    revealed: [u8; 32],
    point: [u8; 33],
    /// The point as the program stores it, and the seed of the escrow's address.
    hash: [u8; 64],
    maker_key: LiquidKey,
    taker_key: LiquidKey,
    /// Who may refund: the party that funded the coin.
    refund_key: LiquidKey,
    fee_wallet: Wallet,
    to: Address,
    /// Each side's Liquid wallet, funded for this scenario only.
    taker: Wallet,
    maker: Wallet,
    /// The adapter signature from the last claim, as the party who needs to read the secret would hold it.
    adaptor: RefCell<Option<Adaptor>>,
}

fn wallet() -> Res<Wallet> {
    Ok(Wallet { key: keypair_from(&random32()?)?, blinding: SecretKey::from_slice(&random32()?).map_err(|e| e.to_string())? })
}

impl Env {
    fn start() -> Res<Self> {
        let node = Node::start()?;
        let chain = Esplora::Node(node.rpc.clone());
        node.mine(1)?;
        let issued: serde_json::Value = serde_json::from_str(&node.rpc.call(&["issueasset", "1000", "0", "false"])?).map_err(|e| e.to_string())?;
        let usdt_id = AssetId::from_str(issued["asset"].as_str().ok_or("asset")?).map_err(|e| e.to_string())?;
        let fake: serde_json::Value = serde_json::from_str(&node.rpc.call(&["issueasset", "1000", "0", "false"])?).map_err(|e| e.to_string())?;
        let fake_asset = AssetId::from_str(fake["asset"].as_str().ok_or("asset")?).map_err(|e| e.to_string())?;
        node.mine(1)?;
        let info: serde_json::Value = serde_json::from_str(&node.rpc.call(&["getsidechaininfo"])?).map_err(|e| e.to_string())?;
        let lbtc_id = AssetId::from_str(info["pegged_asset"].as_str().ok_or("pegged asset")?).map_err(|e| e.to_string())?;
        use_network(NetworkConfig { params: &AddressParams::ELEMENTS, usdt: usdt_id, lbtc: lbtc_id });

        let sol = Sol::new(&std::env::var("SOLANA_RPC").unwrap_or_else(|_| "https://api.devnet.solana.com".into()));
        if sol.genesis()? == MAINNET_GENESIS {
            return Err("refusing to run on Solana mainnet".into());
        }
        let keys = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/.keys"));
        let read = |name: &str| read_keypair_file(keys.join(name)).map_err(|e| format!("{name}: {e}"));
        let genesis = BlockHash::from_str(&chain.genesis()?).map_err(|e| e.to_string())?;
        Ok(Env { node, chain, sol, genesis, taker_sol: read("taker-solana.json")?, maker_sol: read("maker-solana.json")?, mint_authority: read("mint-authority.json")?, fake_asset })
    }

    /// `taker_claims` is true for S→L, where the maker funded the coin and so is the one who can refund.
    fn terms(&self, lock_height: u32, taker_claims: bool) -> Res<Terms> {
        let (taker, maker) = (wallet()?, wallet()?);
        for w in [&taker, &maker] {
            self.node.fund(&w.address(), 100_000, "bitcoin")?;
            self.node.fund(&w.address(), 10 * LIQ_AMOUNT, &usdt().to_string())?;
        }
        self.node.mine(1)?;
        let secret = random32()?;
        let (maker_key, taker_key) = (keypair_from(&random32()?)?, keypair_from(&random32()?)?);
        let refund_key = if taker_claims { maker_key } else { taker_key };
        let output = SwapOutput::new(&SwapParams {
            maker_key: maker_key.public_key().serialize(),
            taker_key: taker_key.public_key().serialize(),
            refund_pk: refund_key.x_only_public_key().0.serialize(),
            lock_height,
        })
        .map_err(|e| e.to_string())?;
        let blinding = SecretKey::from_slice(&random32()?).map_err(|e| e.to_string())?;
        let blinder = PublicKey::from_secret_key(&Secp256k1::new(), &blinding);
        let address = output.address(blinder, &AddressParams::ELEMENTS);
        let to = if taker_claims { taker.address() } else { maker.address() };
        Ok(Terms {
            output,
            address,
            blinding,
            lock_height,
            secret,
            revealed: solana_value(&secret).map_err(|e| e.to_string())?,
            point: point_of(&secret).map_err(|e| e.to_string())?,
            hash: solana_point(&secret).map_err(|e| e.to_string())?,
            maker_key,
            taker_key,
            refund_key,
            fee_wallet: wallet()?,
            to,
            taker,
            maker,
            adaptor: RefCell::new(None),
        })
    }

    fn tip(&self) -> Res<u32> {
        self.chain.tip()
    }

    fn pay(&self, from: &Wallet, payments: Vec<Payment>) -> Res<Transaction> {
        let tx = build_confidential_pay(&self.chain, from, payments, self.genesis)?;
        self.chain.broadcast(&tx)?;
        Ok(tx)
    }

    fn lock_liquid(&self, t: &Terms, payer: &Wallet, with_fee_coin: bool) -> Res<Transaction> {
        let mut payments = vec![Payment { asset: usdt(), amount: LIQ_AMOUNT, address: t.address.clone() }];
        if with_fee_coin {
            payments.push(Payment { asset: lbtc(), amount: FEE, address: t.fee_wallet.address() });
        }
        self.pay(payer, payments)
    }

    fn fee_coin(&self, t: &Terms) -> Res<SecretCoin> {
        self.chain
            .secret_coins(&t.fee_wallet.address(), &t.fee_wallet.blinding)?
            .into_iter()
            .find(|c| c.secrets.value == FEE)
            .ok_or_else(|| "no confirmed fee coin".to_string())
    }

    /// A new fee coin, found by its own transaction, because a key can hold several of them.
    fn fund_fee_coin(&self, t: &Terms, payer: &Wallet) -> Res<SecretCoin> {
        let holder = t.fee_wallet.address();
        let tx = self.pay(payer, vec![Payment { asset: lbtc(), amount: FEE, address: holder.clone() }])?;
        self.node.mine(1)?;
        let (vout, out) = tx
            .output
            .iter()
            .enumerate()
            .find(|(_, o)| o.script_pubkey == holder.script_pubkey() && read_output(o, &t.fee_wallet.blinding).is_some())
            .ok_or("the fee coin isn't in its transaction")?;
        let secrets = read_output(out, &t.fee_wallet.blinding).ok_or("unreadable fee coin")?;
        let outpoint = OutPoint::new(tx.txid(), vout as u32);
        let coin = liq2sol_chain::liquid::Coin { outpoint, out: out.clone(), height: Some(self.tip()?) };
        Ok(SecretCoin { coin, secrets })
    }

    fn coins_at(&self, t: &Terms) -> Res<Vec<SecretCoin>> {
        self.chain.secret_coins(&t.address, &t.blinding)
    }

    fn coin(&self, t: &Terms, confs: u32) -> Res<SecretCoin> {
        exact_secret_coin(&self.coins_at(t)?, usdt(), LIQ_AMOUNT, self.tip()?, confs)
            .ok_or_else(|| "the exact coin is not there".to_string())
    }

    /// A whole signing session, then publish. Both parties live in this process, each using only its own key, and the
    /// signature only comes out right with the true secret.
    fn claim_liquid(&self, t: &Terms, coin: &SecretCoin, fee: &SecretCoin, secret: [u8; 32]) -> Res<Txid> {
        let (mut tx, utxos) = build_swap_spend(coin, fee, &t.to, 0, Sequence::MAX)?;
        let message = key_spend_sighash(&tx, &utxos, 0, self.genesis).map_err(|e| e.to_string())?;
        let ctx = t.output.key_ctx();
        let (maker_sk, taker_sk) = (t.maker_key.secret_bytes(), t.taker_key.secret_bytes());
        let maker = Session::start(&maker_sk, ctx, &message).map_err(|e| e.to_string())?;
        let taker = Session::start(&taker_sk, ctx, &message).map_err(|e| e.to_string())?;
        let (maker_nonce, taker_nonce) = (maker.public.clone(), taker.public.clone());
        let (maker_share, aggregate) =
            maker.sign(&maker_sk, ctx, &taker_nonce, &t.point, &message).map_err(|e| e.to_string())?;
        let (taker_share, _) = taker.sign(&taker_sk, ctx, &maker_nonce, &t.point, &message).map_err(|e| e.to_string())?;
        let taker_public = t.taker_key.public_key().serialize();
        verify_share(ctx, taker_share, &aggregate, &taker_public, &taker_nonce, &t.point, &message)
            .map_err(|e| e.to_string())?;
        let adaptor =
            combine(ctx, &aggregate, [maker_share, taker_share], &t.point, &message).map_err(|e| e.to_string())?;
        let signature = complete(&adaptor, &secret).map_err(|e| e.to_string())?;
        *t.adaptor.borrow_mut() = Some(adaptor);
        set_key_spend_witness(&mut tx, 0, &signature);
        sign_keypath(&mut tx, &utxos, 1, &t.fee_wallet.key, self.genesis)?;
        self.chain.broadcast(&tx)
    }

    fn refund_liquid(&self, t: &Terms, coin: &SecretCoin, fee: &SecretCoin, to: Address) -> Res<Txid> {
        let (tx, utxos) = build_swap_spend(coin, fee, &to, t.lock_height, Sequence::ZERO)?;
        let digest = t.output.refund_sighash(&tx, &utxos, 0, self.genesis).map_err(|e| e.to_string())?;
        let signature =
            *Secp256k1::new().sign_schnorr_no_aux_rand(&Message::from_digest(digest), &t.refund_key).as_ref();
        let mut tx = t.output.finalize_refund(tx, &utxos, 0, self.genesis, signature).map_err(|e| e.to_string())?;
        sign_keypath(&mut tx, &utxos, 1, &t.fee_wallet.key, self.genesis)?;
        self.chain.broadcast(&tx)
    }

    fn holds(&self, _t: &Terms, w: &Wallet) -> Res<u64> {
        let coins = self.chain.secret_coins(&w.address(), &w.blinding)?;
        Ok(coins.iter().filter(|c| c.secrets.asset == usdt()).map(|c| c.secrets.value).sum())
    }

    fn tokens(&self, owner: &Pubkey) -> Res<u64> {
        self.sol.token_balance(&ata(owner))
    }

    fn escrow(&self, address: &Pubkey) -> Res<Escrow> {
        self.sol.escrow(address)?.ok_or_else(|| "escrow missing".to_string())
    }

    fn wait_slot(&self, slot: u64) -> Res<()> {
        let start = Instant::now();
        while self.sol.slot()? < slot {
            if start.elapsed() > Duration::from_secs(600) {
                return Err(format!("slot {slot} never arrived"));
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        Ok(())
    }
}

fn check(condition: bool, what: &str) -> Res<()> {
    if condition {
        println!("    ok: {what}");
        Ok(())
    } else {
        Err(format!("FAILED: {what}"))
    }
}

fn rejected<T>(result: Res<T>, must_contain: &str, what: &str) -> Res<()> {
    match result {
        Ok(_) => Err(format!("FAILED: {what}: it was accepted")),
        Err(e) if e.contains(must_contain) => {
            println!("    ok: {what}");
            Ok(())
        }
        Err(e) => Err(format!("FAILED: {what}: wrong reason: {}", e.chars().take(400).collect::<String>())),
    }
}

/// What the claim on Solana revealed, as the secret the maker needs.
fn learned(e: &Escrow) -> Res<[u8; 32]> {
    secret_from_solana(&e.revealed).map_err(|e| e.to_string())
}

/// The secret, from the signature on-chain, for the party holding the adapter signature.
fn secret_from_chain(env: &Env, t: &Terms, outpoint: &OutPoint) -> Res<[u8; 32]> {
    let tx: Transaction = env.chain.spender(outpoint)?.ok_or("nothing spent the coin")?;
    let signature = published_signature(&tx, outpoint).ok_or("no key-path signature")?;
    let adaptor = t.adaptor.borrow().clone().ok_or("no adapter signature held")?;
    extract_secret(&adaptor, &signature, &t.point).ok_or_else(|| "the signature does not give the secret".to_string())
}

fn revealing(secret: &[u8; 32]) -> Res<[u8; 32]> {
    solana_value(secret).map_err(|e| e.to_string())
}

/// E4: the taker reveals just before the deadline, and the maker still gets paid. One slot too late, no reveal.
fn e4_last_moment_reveal(env: &Env) -> Res<()> {
    let t = env.terms(env.tip()? + 300, false)?;
    env.lock_liquid(&t, &t.taker, false)?;
    env.node.mine(2)?;
    let coin = env.coin(&t, 2)?;
    let deadline = env.sol.slot()? + WINDOW;
    let escrow = env.sol.lock(&env.maker_sol, t.hash, env.taker_sol.pubkey(), SOL_AMOUNT, deadline, true)?;
    env.wait_slot(deadline - 40)?;
    let e = env.escrow(&escrow)?;
    let before = env.tokens(&env.taker_sol.pubkey())?;
    env.sol.claim(&env.taker_sol, &escrow, &e, t.revealed)?;
    check(env.tokens(&env.taker_sol.pubkey())? == before + SOL_AMOUNT, "the taker's late claim paid out")?;
    let settled = env.escrow(&escrow)?;
    check(settled.revealed == t.revealed, "the revealed value is readable after the deadline window")?;
    let fee = env.fund_fee_coin(&t, &t.maker)?;
    env.claim_liquid(&t, &coin, &fee, learned(&settled)?)?;
    env.node.mine(1)?;
    check(env.chain.spender(&coin.coin.outpoint)?.is_some(), "the maker claimed on Liquid with hours to spare")?;

    // Past the deadline the claim is refused in simulation, so the secret is never published.
    let t = env.terms(env.tip()? + 300, false)?;
    let deadline = env.sol.slot()? + 40;
    let escrow = env.sol.lock(&env.maker_sol, t.hash, env.taker_sol.pubkey(), SOL_AMOUNT, deadline, true)?;
    env.wait_slot(deadline + 1)?;
    let e = env.escrow(&escrow)?;
    rejected(env.sol.claim(&env.taker_sol, &escrow, &e, t.revealed), "simulation failed", "a claim past the deadline is stopped before sending")?;
    check(env.escrow(&escrow)?.revealed == [0; 32], "the secret was not published")?;
    env.sol.refund(&env.maker_sol, &escrow, &e)?;
    Ok(())
}

/// E5: the maker sleeps through the reveal. Back before the lock height it is paid; back after, the taker can take both.
fn e5_maker_offline(env: &Env) -> Res<()> {
    let lock_height = env.tip()? + 10;
    let t = env.terms(lock_height, false)?;
    env.lock_liquid(&t, &t.taker, false)?;
    env.node.mine(2)?;
    let coin = env.coin(&t, 2)?;
    let deadline = env.sol.slot()? + WINDOW;
    let escrow = env.sol.lock(&env.maker_sol, t.hash, env.taker_sol.pubkey(), SOL_AMOUNT, deadline, true)?;
    let e = env.escrow(&escrow)?;
    env.sol.claim(&env.taker_sol, &escrow, &e, t.revealed)?;
    let fee = env.fund_fee_coin(&t, &t.maker)?;
    while env.tip()? < lock_height - 1 {
        env.node.mine(1)?;
    }
    env.claim_liquid(&t, &coin, &fee, learned(&env.escrow(&escrow)?)?)?;
    env.node.mine(1)?;
    check(env.chain.spender(&coin.coin.outpoint)?.is_some(), "a maker back one block before the lock height is paid")?;

    // Back too late: the taker refunds first, and now holds both sides.
    let lock_height = env.tip()? + 8;
    let t = env.terms(lock_height, false)?;
    env.lock_liquid(&t, &t.taker, false)?;
    env.node.mine(2)?;
    let coin = env.coin(&t, 2)?;
    let deadline = env.sol.slot()? + WINDOW;
    let escrow = env.sol.lock(&env.maker_sol, t.hash, env.taker_sol.pubkey(), SOL_AMOUNT, deadline, true)?;
    let e = env.escrow(&escrow)?;
    env.sol.claim(&env.taker_sol, &escrow, &e, t.revealed)?;
    let fee = env.fund_fee_coin(&t, &t.taker)?;
    while env.tip()? < lock_height {
        env.node.mine(1)?;
    }
    env.refund_liquid(&t, &coin, &fee, t.taker.address())?;
    env.node.mine(1)?;
    let maker_fee = env.fund_fee_coin(&t, &t.maker)?;
    rejected(env.claim_liquid(&t, &coin, &maker_fee, learned(&env.escrow(&escrow)?)?), "error code: -25", "the node refuses the late maker's claim, because the coin is spent")?;
    let spender = env.chain.spender(&coin.coin.outpoint)?.ok_or("coin not spent")?;
    check(spender.output[0].script_pubkey == t.taker.address().script_pubkey(), "the taker's refund took the coin, so the taker holds both sides")?;
    println!("    RISK CONFIRMED: a maker offline past its margin loses the swap. This is what the margin and monitoring are for.");
    Ok(())
}

/// E9: wrong coins never get a Solana lock, nobody signs a claim of one, and they always come back through the refund path.
/// Nothing on-chain looks at the asset or amount any more, so this protection is entirely each party reading the coin.
fn e9_wrong_coins(env: &Env) -> Res<()> {
    let lock_height = env.tip()? + 5;
    let t = env.terms(lock_height, false)?;

    // Too little, and a fake asset of the right amount, both blinded, so they look like anything else on-chain.
    env.pay(&t.taker, vec![Payment { asset: usdt(), amount: LIQ_AMOUNT - 100, address: t.address.clone() }])?;
    env.node.mine(1)?;
    env.node.fund(&t.address, LIQ_AMOUNT, &env.fake_asset.to_string())?;
    env.node.mine(2)?;
    let coins = env.coins_at(&t)?;
    check(coins.len() == 2, "both wrong coins are at the address, and both can be read")?;
    check(
        exact_secret_coin(&coins, usdt(), LIQ_AMOUNT, env.tip()?, 2).is_none(),
        "the maker reads no acceptable coin, so it never locks or signs",
    )?;
    check(
        coins.iter().all(|c| !c.coin.out.asset.is_explicit() && !c.coin.out.value.is_explicit()),
        "and nobody without the blinding key could tell",
    )?;

    while env.tip()? < lock_height {
        env.node.mine(1)?;
    }
    let to = t.taker.address();
    let fee = env.fund_fee_coin(&t, &t.taker)?;
    env.refund_liquid(&t, &coins[0], &fee, to.clone())?;
    let fee2 = env.fund_fee_coin(&t, &t.taker)?;
    env.refund_liquid(&t, &coins[1], &fee2, to)?;
    env.node.mine(1)?;
    check(env.coins_at(&t)?.is_empty(), "both wrong coins were refunded, whatever their asset")?;

    // Paid twice: the maker acts on one exact coin, and the other is refundable.
    let lock_height = env.tip()? + 6;
    let t = env.terms(lock_height, false)?;
    env.lock_liquid(&t, &t.taker, false)?;
    env.node.mine(1)?;
    env.lock_liquid(&t, &t.taker, false)?;
    env.node.mine(2)?;
    let coins = env.coins_at(&t)?;
    check(coins.len() == 2, "two exact coins at one address")?;
    let fee = env.fund_fee_coin(&t, &t.maker)?;
    env.claim_liquid(&t, &coins[0], &fee, t.secret)?;
    env.node.mine(1)?;
    let fee2 = env.fund_fee_coin(&t, &t.taker)?;
    while env.tip()? < lock_height {
        env.node.mine(1)?;
    }
    env.refund_liquid(&t, &coins[1], &fee2, t.taker.address())?;
    env.node.mine(1)?;
    check(env.coins_at(&t)?.is_empty(), "the second payment came back")?;
    Ok(())
}

/// E11: Liquid halts during S→L, and the maker's `extend` keeps the taker's refund closed until blocks resume.
fn e11_liquid_halt_with_extend(env: &Env) -> Res<()> {
    let t = env.terms(env.tip()? + 20, true)?;
    let deadline = env.sol.slot()? + WINDOW;
    let escrow = env.sol.lock(&env.taker_sol, t.hash, env.maker_sol.pubkey(), SOL_AMOUNT, deadline, false)?;
    env.lock_liquid(&t, &t.maker, true)?;
    env.node.mine(2)?;
    let coin = env.coin(&t, 2)?;
    let fee = env.fee_coin(&t)?;

    println!("    Liquid halts now: no more blocks");
    let halted_at = env.tip()?;
    env.sol.extend(&env.maker_sol, &escrow, deadline + 100_000)?;
    env.wait_slot(deadline + 1)?;
    let e = env.escrow(&escrow)?;
    check(e.extended && e.deadline_slot == deadline + 100_000, "the deadline moved")?;
    check(env.tip()? == halted_at, "Liquid really did not advance")?;
    rejected(env.sol.refund(&env.taker_sol, &escrow, &e), "simulation failed", "the taker can't refund after the original deadline")?;

    println!("    Liquid resumes");
    env.node.mine(1)?;
    env.claim_liquid(&t, &coin, &fee, t.secret)?;
    env.node.mine(1)?;
    let secret = secret_from_chain(env, &t, &coin.coin.outpoint)?;
    let before = env.tokens(&env.maker_sol.pubkey())?;
    env.sol.claim(&env.maker_sol, &escrow, &e, revealing(&secret)?)?;
    check(env.tokens(&env.maker_sol.pubkey())? == before + SOL_AMOUNT, "the maker was paid on Solana")?;
    check(env.holds(&t, &t.taker)? >= 10 * LIQ_AMOUNT + LIQ_AMOUNT, "the taker was paid on Liquid")?;
    Ok(())
}

/// E12: the same halt with no `extend`. This is the theft from protocol.md section 7.1, and why decision D1 exists.
fn e12_liquid_halt_without_extend(env: &Env) -> Res<()> {
    let t = env.terms(env.tip()? + 20, true)?;
    let deadline = env.sol.slot()? + WINDOW;
    let before = env.tokens(&env.taker_sol.pubkey())?;
    let escrow = env.sol.lock(&env.taker_sol, t.hash, env.maker_sol.pubkey(), SOL_AMOUNT, deadline, false)?;
    env.lock_liquid(&t, &t.maker, true)?;
    env.node.mine(2)?;
    let coin = env.coin(&t, 2)?;
    let fee = env.fee_coin(&t)?;

    println!("    Liquid halts now, and the maker does nothing");
    env.wait_slot(deadline)?;
    let e = env.escrow(&escrow)?;
    env.sol.refund(&env.taker_sol, &escrow, &e)?;
    check(env.tokens(&env.taker_sol.pubkey())? == before, "the taker took its Solana funds back during the halt")?;

    println!("    Liquid resumes, below the lock height, and the taker claims there too");
    env.node.mine(1)?;
    check(env.tip()? < t.lock_height, "the maker's refund is still not open")?;
    env.claim_liquid(&t, &coin, &fee, t.secret)?;
    env.node.mine(1)?;
    check(env.holds(&t, &t.taker)? >= 11 * LIQ_AMOUNT, "the taker also holds the Liquid funds")?;

    let secret = secret_from_chain(env, &t, &coin.coin.outpoint)?;
    rejected(env.sol.claim(&env.maker_sol, &escrow, &e, revealing(&secret)?), "", "the maker, now knowing the secret, has nothing left to claim")?;
    check(env.escrow(&escrow)?.state == EscrowState::Refunded, "the escrow is refunded")?;
    println!("    THEFT CONFIRMED: without extend, a Liquid halt longer than the margin costs the maker the full amount.");
    Ok(())
}

/// E13: Liquid halts during L→S. Its clock stops with it, so the maker is safe.
fn e13_liquid_halt_l2s(env: &Env) -> Res<()> {
    let lock_height = env.tip()? + 8;
    let t = env.terms(lock_height, false)?;
    env.lock_liquid(&t, &t.taker, false)?;
    env.node.mine(2)?;
    let coin = env.coin(&t, 2)?;
    let fee = env.fund_fee_coin(&t, &t.maker)?;
    let deadline = env.sol.slot()? + WINDOW;
    let escrow = env.sol.lock(&env.maker_sol, t.hash, env.taker_sol.pubkey(), SOL_AMOUNT, deadline, true)?;

    println!("    Liquid halts now");
    let halted_at = env.tip()?;
    let e = env.escrow(&escrow)?;
    env.sol.claim(&env.taker_sol, &escrow, &e, t.revealed)?;
    env.wait_slot(deadline + 1)?;
    check(env.tip()? == halted_at && halted_at < lock_height, "the lock height can't pass during the halt")?;
    let settled = env.escrow(&escrow)?;
    env.claim_liquid(&t, &coin, &fee, learned(&settled)?)?;
    println!("    Liquid resumes");
    env.node.mine(1)?;
    check(env.holds(&t, &t.maker)? >= 11 * LIQ_AMOUNT - 1, "the maker's claim confirmed after the halt")?;
    Ok(())
}

/// E15a: a rollback drops the taker's Liquid lock after the maker has paid out. Accepted federation trust.
fn e15a_rollback_drops_taker_lock(env: &mut Env) -> Res<()> {
    let t = env.terms(env.tip()? + 300, false)?;
    let taker_start = env.holds(&t, &t.taker)?;
    env.lock_liquid(&t, &t.taker, false)?;
    env.node.mine(2)?;
    let coin = env.coin(&t, 2)?;
    let lock_block = coin.coin.height.ok_or("unconfirmed coin")?;
    let deadline = env.sol.slot()? + WINDOW;
    let escrow = env.sol.lock(&env.maker_sol, t.hash, env.taker_sol.pubkey(), SOL_AMOUNT, deadline, true)?;
    let e = env.escrow(&escrow)?;
    env.sol.claim(&env.taker_sol, &escrow, &e, t.revealed)?;
    check(env.escrow(&escrow)?.state == EscrowState::Claimed, "the taker has been paid on Solana")?;
    let tip_before = env.tip()?;

    println!("    The federation rolls Liquid back past the lock");
    env.node.roll_back_to_before(lock_block)?;
    check(env.tip()? < tip_before, "the tip went down, which a daemon must treat as an emergency")?;
    check(env.coin(&t, 0).is_err(), "the lock is gone")?;

    println!("    The taker double-spends the same coins back to itself");
    env.pay(&t.taker, vec![Payment { asset: usdt(), amount: LIQ_AMOUNT, address: t.taker.address() }])?;
    env.node.mine(2)?;
    check(env.holds(&t, &t.taker)? == taker_start, "the taker has all its Liquid funds back")?;
    check(env.coin(&t, 0).is_err(), "there is nothing at the HTLC for the maker to claim")?;
    println!("    LOSS CONFIRMED: a rollback past the lock costs the maker the swap. Only exposure caps limit this.");
    Ok(())
}

/// E15b: a rollback drops the taker's Liquid claim after the maker has claimed on Solana.
fn e15b_rollback_drops_taker_claim(env: &mut Env) -> Res<()> {
    // First the good case: the taker is watching, and rebroadcasts.
    let t = env.terms(env.tip()? + 8, true)?;
    let escrow = env.sol.lock(&env.taker_sol, t.hash, env.maker_sol.pubkey(), SOL_AMOUNT, env.sol.slot()? + 5_000, false)?;
    env.lock_liquid(&t, &t.maker, true)?;
    env.node.mine(2)?;
    let coin = env.coin(&t, 2)?;
    let fee = env.fee_coin(&t)?;
    let claim_txid = env.claim_liquid(&t, &coin, &fee, t.secret)?;
    let claim_tx = env.chain.tx(&claim_txid)?;
    env.node.mine(1)?;
    let claim_block = env.tip()?;
    let secret = secret_from_chain(env, &t, &coin.coin.outpoint)?;
    let e = env.escrow(&escrow)?;
    env.sol.claim(&env.maker_sol, &escrow, &e, revealing(&secret)?)?;
    println!("    The federation rolls Liquid back past the taker's claim");
    env.node.roll_back_to_before(claim_block)?;
    check(env.chain.spender(&coin.coin.outpoint)?.is_none(), "the claim is gone, while the maker keeps its Solana funds")?;
    env.chain.broadcast(&claim_tx)?;
    env.node.mine(1)?;
    check(env.chain.spender(&coin.coin.outpoint)?.is_some(), "a taker that rebroadcasts its claim still gets paid")?;

    // Then the bad case: the taker is gone, and the maker's refund opens.
    let lock_height = env.tip()? + 6;
    let t = env.terms(lock_height, true)?;
    let escrow = env.sol.lock(&env.taker_sol, t.hash, env.maker_sol.pubkey(), SOL_AMOUNT, env.sol.slot()? + 5_000, false)?;
    env.lock_liquid(&t, &t.maker, true)?;
    env.node.mine(2)?;
    let coin = env.coin(&t, 2)?;
    let fee = env.fee_coin(&t)?;
    env.claim_liquid(&t, &coin, &fee, t.secret)?;
    env.node.mine(1)?;
    let claim_block = env.tip()?;
    let secret = secret_from_chain(env, &t, &coin.coin.outpoint)?;
    let e = env.escrow(&escrow)?;
    env.sol.claim(&env.maker_sol, &escrow, &e, revealing(&secret)?)?;
    env.node.roll_back_to_before(claim_block)?;
    while env.tip()? < lock_height {
        env.node.mine(1)?;
    }
    let refund_fee = env.fund_fee_coin(&t, &t.maker)?;
    env.node.mine(1)?;
    env.refund_liquid(&t, &coin, &refund_fee, t.maker.address())?;
    env.node.mine(1)?;
    check(env.escrow(&escrow)?.state == EscrowState::Claimed, "the maker holds the Solana funds")?;
    let spent_to_maker = env.chain.spender(&coin.coin.outpoint)?.ok_or("coin not spent")?;
    check(spent_to_maker.output[0].script_pubkey == t.maker.address().script_pubkey(), "and got the Liquid funds back too")?;
    println!("    LOSS CONFIRMED: a rollback past the claim, with an absent taker, costs the taker the swap.");
    Ok(())
}

/// E16: Tether freezes accounts mid-swap, using a real freeze authority.
fn e16_freezes(env: &Env) -> Res<()> {
    let auth = &env.mint_authority;

    // The taker's account: simulation stops the claim, so the secret stays secret and both sides refund.
    let lock_height = env.tip()? + 8;
    let t = env.terms(lock_height, false)?;
    env.lock_liquid(&t, &t.taker, false)?;
    env.node.mine(2)?;
    let coin = env.coin(&t, 2)?;
    let deadline = env.sol.slot()? + WINDOW;
    let escrow = env.sol.lock(&env.maker_sol, t.hash, env.taker_sol.pubkey(), SOL_AMOUNT, deadline, true)?;
    let e = env.escrow(&escrow)?;
    let taker_ata = ata(&env.taker_sol.pubkey());
    env.sol.set_frozen(auth, &taker_ata, true)?;
    rejected(env.sol.claim(&env.taker_sol, &escrow, &e, t.revealed), "simulation failed", "a frozen taker's claim is stopped before sending")?;
    check(env.escrow(&escrow)?.revealed == [0; 32], "the secret was not published")?;
    env.sol.set_frozen(auth, &taker_ata, false)?;
    env.wait_slot(deadline)?;
    env.sol.refund(&env.maker_sol, &escrow, &e)?;
    check(env.escrow(&escrow)?.state == EscrowState::Refunded, "the maker refunded on Solana")?;
    let fee = env.fund_fee_coin(&t, &t.taker)?;
    while env.tip()? < lock_height {
        env.node.mine(1)?;
    }
    env.refund_liquid(&t, &coin, &fee, t.taker.address())?;
    env.node.mine(1)?;
    check(env.chain.spender(&coin.coin.outpoint)?.is_some(), "the taker refunded on Liquid, so nobody lost anything")?;

    // The escrow's own vault: both paths are stuck until a thaw, then one works.
    let t = env.terms(env.tip()? + 300, false)?;
    let deadline = env.sol.slot()? + WINDOW;
    let escrow = env.sol.lock(&env.maker_sol, t.hash, env.taker_sol.pubkey(), SOL_AMOUNT, deadline, true)?;
    let e = env.escrow(&escrow)?;
    let vault = vault_pda(&escrow);
    env.sol.set_frozen(auth, &vault, true)?;
    rejected(env.sol.claim(&env.taker_sol, &escrow, &e, t.revealed), "simulation failed", "a claim from a frozen vault is stopped")?;
    env.wait_slot(deadline)?;
    rejected(env.sol.refund(&env.maker_sol, &escrow, &e), "simulation failed", "so is a refund")?;
    check(env.escrow(&escrow)?.state == EscrowState::Open, "the escrow is unchanged")?;
    env.sol.set_frozen(auth, &vault, false)?;
    env.sol.refund(&env.maker_sol, &escrow, &e)?;
    check(env.escrow(&escrow)?.state == EscrowState::Refunded, "after a thaw the refund works")?;

    // The maker's account in S→L, after the taker has revealed. A thaw in time saves the maker, and one too late doesn't.
    let t = env.terms(env.tip()? + 20, true)?;
    let deadline = env.sol.slot()? + WINDOW;
    let escrow = env.sol.lock(&env.taker_sol, t.hash, env.maker_sol.pubkey(), SOL_AMOUNT, deadline, false)?;
    env.lock_liquid(&t, &t.maker, true)?;
    env.node.mine(2)?;
    let coin = env.coin(&t, 2)?;
    let fee = env.fee_coin(&t)?;
    let maker_ata = ata(&env.maker_sol.pubkey());
    env.sol.set_frozen(auth, &maker_ata, true)?;
    env.claim_liquid(&t, &coin, &fee, t.secret)?;
    env.node.mine(1)?;
    let secret = secret_from_chain(env, &t, &coin.coin.outpoint)?;
    let e = env.escrow(&escrow)?;
    rejected(env.sol.claim(&env.maker_sol, &escrow, &e, revealing(&secret)?), "simulation failed", "a frozen maker can't collect after the reveal")?;
    env.wait_slot(deadline)?;
    env.sol.refund(&env.taker_sol, &escrow, &e)?;
    env.sol.set_frozen(auth, &maker_ata, false)?;
    check(env.escrow(&escrow)?.state == EscrowState::Refunded, "the deadline passed, and the taker refunded")?;
    check(env.holds(&t, &t.taker)? >= 11 * LIQ_AMOUNT, "the taker also holds the Liquid funds")?;
    println!("    LOSS CONFIRMED: a freeze of the maker's account across the deadline costs the maker the swap.");
    Ok(())
}

#[test]
#[ignore = "needs elementsd and funded devnet wallets; see the module docs"]
fn failure_scenarios() {
    let mut env = Env::start().expect("environment");
    let mut failures = vec![];
    type Scenario = (&'static str, fn(&mut Env) -> Res<()>);
    let scenarios: Vec<Scenario> = vec![
        ("E4 last-moment reveal", |e| e4_last_moment_reveal(e)),
        ("E5 maker offline", |e| e5_maker_offline(e)),
        ("E9 wrong coins", |e| e9_wrong_coins(e)),
        ("E11 Liquid halt, with extend", |e| e11_liquid_halt_with_extend(e)),
        ("E12 Liquid halt, without extend", |e| e12_liquid_halt_without_extend(e)),
        ("E13 Liquid halt during L to S", |e| e13_liquid_halt_l2s(e)),
        ("E15a rollback drops the lock", e15a_rollback_drops_taker_lock),
        ("E15b rollback drops the claim", e15b_rollback_drops_taker_claim),
        ("E16 Tether freezes", |e| e16_freezes(e)),
    ];
    // `SCENARIO` runs those whose names contain any of its comma-separated parts.
    let filter = std::env::var("SCENARIO").unwrap_or_default();
    for (name, run) in scenarios {
        if !filter.split(',').any(|part| name.contains(part.trim())) {
            continue;
        }
        println!("\n==> {name}");
        let start = Instant::now();
        match run(&mut env) {
            Ok(()) => println!("PASS {name} ({}s)", start.elapsed().as_secs()),
            Err(e) => {
                println!("FAIL {name}: {e}");
                failures.push(name);
            }
        }
    }
    assert!(failures.is_empty(), "failed: {failures:?}");
}
