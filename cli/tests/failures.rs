//! Failure scenarios from docs/test-plan.md section 5, on a real Elements regtest node and real Solana devnet.
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

use anchor_lang::prelude::Pubkey;
use liq2sol_chain::liquid::{
    address_of, exact_coin, keypair_from, lbtc, pay, random32, spend_htlc, txout, unhex_tx_witness_preimage, use_network,
    usdt, Coin, Esplora, HtlcSpend, NetworkConfig, Res, Rpc, FEE,
};
use liq2sol_chain::sol::{ata, vault_pda, Sol, MAINNET_GENESIS};
use liq2sol_htlc::{asset_param, script_hash, Htlc, HtlcParams};
use simplicityhl::elements::hashes::sha256;
use simplicityhl::elements::secp256k1_zkp::Keypair as LiquidKey;
use simplicityhl::elements::{Address, AddressParams, AssetId, BlockHash, OutPoint, Script, Transaction, Txid};
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
    params: HtlcParams,
    htlc: Htlc,
    preimage: [u8; 32],
    hash: [u8; 32],
    claim_key: LiquidKey,
    refund_key: LiquidKey,
    fee_key: LiquidKey,
    to: Script,
    /// Each side's Liquid wallet, funded for this scenario only.
    taker: LiquidKey,
    maker: LiquidKey,
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

    /// `taker_claims` is true for S→L, where the taker holds the Liquid claim key.
    fn terms(&self, lock_height: u32, taker_claims: bool) -> Res<Terms> {
        let (taker, maker) = (keypair_from(&random32()?)?, keypair_from(&random32()?)?);
        for key in [&taker, &maker] {
            self.node.fund(&address_of(key), 100_000, "bitcoin")?;
            self.node.fund(&address_of(key), 10 * LIQ_AMOUNT, &usdt().to_string())?;
        }
        self.node.mine(1)?;
        let preimage = random32()?;
        let hash = sha256::Hash::hash(&preimage).to_byte_array();
        let (claim_key, refund_key) = (keypair_from(&random32()?)?, keypair_from(&random32()?)?);
        let to = address_of(if taker_claims { &taker } else { &maker }).script_pubkey();
        let params = HtlcParams {
            payment_hash: hash,
            recipient_pk: claim_key.x_only_public_key().0.serialize(),
            sender_pk: refund_key.x_only_public_key().0.serialize(),
            lock_height,
            usdt_asset: asset_param(&usdt()),
            amount: LIQ_AMOUNT,
            recipient_script_hash: script_hash(&to),
        };
        let htlc = Htlc::new(&params).map_err(|e| e.to_string())?;
        Ok(Terms { params, htlc, preimage, hash, claim_key, refund_key, fee_key: keypair_from(&random32()?)?, to, taker, maker })
    }

    fn tip(&self) -> Res<u32> {
        self.chain.tip()
    }

    fn lock_liquid(&self, t: &Terms, payer: &LiquidKey, with_fee_coin: bool) -> Res<Txid> {
        let mut outputs = vec![txout(usdt(), LIQ_AMOUNT, t.htlc.address(&AddressParams::ELEMENTS).script_pubkey())];
        if with_fee_coin {
            outputs.push(txout(lbtc(), FEE, address_of(&t.fee_key).script_pubkey()));
        }
        pay(&self.chain, payer, outputs, self.genesis)
    }

    fn fee_coin(&self, t: &Terms) -> Res<Coin> {
        self.chain
            .coins(&address_of(&t.fee_key))?
            .into_iter()
            .find(|c| c.out.value.explicit() == Some(FEE))
            .ok_or_else(|| "no confirmed fee coin".to_string())
    }

    fn fund_fee_coin(&self, t: &Terms, payer: &LiquidKey) -> Res<Coin> {
        pay(&self.chain, payer, vec![txout(lbtc(), FEE, address_of(&t.fee_key).script_pubkey())], self.genesis)?;
        self.node.mine(1)?;
        self.fee_coin(t)
    }

    fn coin(&self, t: &Terms, confs: u32) -> Res<Coin> {
        exact_coin(&self.chain.coins(&t.htlc.address(&AddressParams::ELEMENTS))?, usdt(), LIQ_AMOUNT, self.tip()?, confs)
            .ok_or_else(|| "the exact HTLC coin is not there".to_string())
    }

    fn claim_liquid(&self, t: &Terms, coin: &Coin, fee: &Coin, preimage: [u8; 32]) -> Res<Txid> {
        spend_htlc(
            &self.chain,
            &t.params,
            coin,
            fee,
            &t.fee_key,
            HtlcSpend::Claim { preimage, recipient: t.claim_key, to: t.to.clone() },
            self.genesis,
        )
    }

    fn refund_liquid(&self, t: &Terms, coin: &Coin, fee: &Coin, to: Script) -> Res<Txid> {
        spend_htlc(
            &self.chain,
            &t.params,
            coin,
            fee,
            &t.fee_key,
            HtlcSpend::Refund { sender: t.refund_key, lock_height: t.params.lock_height, to },
            self.genesis,
        )
    }

    fn holds(&self, t: &Terms, script_of: &LiquidKey) -> Res<u64> {
        let _ = t;
        Ok(self
            .chain
            .coins(&address_of(script_of))?
            .iter()
            .filter(|c| c.out.asset.explicit() == Some(usdt()))
            .filter_map(|c| c.out.value.explicit())
            .sum())
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

fn preimage_from_chain(env: &Env, outpoint: &OutPoint, hash: &[u8; 32]) -> Res<[u8; 32]> {
    let tx: Transaction = env.chain.spender(outpoint)?.ok_or("nothing spent the coin")?;
    unhex_tx_witness_preimage(&tx, outpoint, hash).ok_or_else(|| "no preimage in the witness".to_string())
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
    env.sol.claim(&env.taker_sol, &escrow, &e, t.preimage)?;
    check(env.tokens(&env.taker_sol.pubkey())? == before + SOL_AMOUNT, "the taker's late claim paid out")?;
    let settled = env.escrow(&escrow)?;
    check(settled.preimage == t.preimage, "the preimage is readable after the deadline window")?;
    let fee = env.fund_fee_coin(&t, &t.maker)?;
    env.claim_liquid(&t, &coin, &fee, settled.preimage)?;
    env.node.mine(1)?;
    check(env.chain.spender(&coin.outpoint)?.is_some(), "the maker claimed on Liquid with hours to spare")?;

    // Past the deadline the claim is refused in simulation, so the preimage is never published.
    let t = env.terms(env.tip()? + 300, false)?;
    let deadline = env.sol.slot()? + 40;
    let escrow = env.sol.lock(&env.maker_sol, t.hash, env.taker_sol.pubkey(), SOL_AMOUNT, deadline, true)?;
    env.wait_slot(deadline + 1)?;
    let e = env.escrow(&escrow)?;
    rejected(env.sol.claim(&env.taker_sol, &escrow, &e, t.preimage), "simulation failed", "a claim past the deadline is stopped before sending")?;
    check(env.escrow(&escrow)?.preimage == [0; 32], "the preimage was not published")?;
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
    env.sol.claim(&env.taker_sol, &escrow, &e, t.preimage)?;
    let fee = env.fund_fee_coin(&t, &t.maker)?;
    while env.tip()? < lock_height - 1 {
        env.node.mine(1)?;
    }
    env.claim_liquid(&t, &coin, &fee, env.escrow(&escrow)?.preimage)?;
    env.node.mine(1)?;
    check(env.chain.spender(&coin.outpoint)?.is_some(), "a maker back one block before the lock height is paid")?;

    // Back too late: the taker refunds first, and now holds both sides.
    let lock_height = env.tip()? + 8;
    let t = env.terms(lock_height, false)?;
    env.lock_liquid(&t, &t.taker, false)?;
    env.node.mine(2)?;
    let coin = env.coin(&t, 2)?;
    let deadline = env.sol.slot()? + WINDOW;
    let escrow = env.sol.lock(&env.maker_sol, t.hash, env.taker_sol.pubkey(), SOL_AMOUNT, deadline, true)?;
    let e = env.escrow(&escrow)?;
    env.sol.claim(&env.taker_sol, &escrow, &e, t.preimage)?;
    let fee = env.fund_fee_coin(&t, &t.taker)?;
    while env.tip()? < lock_height {
        env.node.mine(1)?;
    }
    env.refund_liquid(&t, &coin, &fee, address_of(&t.taker).script_pubkey())?;
    env.node.mine(1)?;
    let maker_fee = env.fund_fee_coin_for_late_maker(&t)?;
    rejected(env.claim_liquid(&t, &coin, &maker_fee, env.escrow(&escrow)?.preimage), "error code: -25", "the node refuses the late maker's claim, because the coin is spent")?;
    let spender = env.chain.spender(&coin.outpoint)?.ok_or("coin not spent")?;
    check(spender.output[0].script_pubkey == address_of(&t.taker).script_pubkey(), "the taker's refund took the coin, so the taker holds both sides")?;
    println!("    RISK CONFIRMED: a maker offline past its margin loses the swap. This is what the margin and monitoring are for.");
    Ok(())
}

/// E9: wrong coins never get a Solana lock, can't be claimed, and always come back through the refund path.
fn e9_wrong_coins(env: &Env) -> Res<()> {
    let lock_height = env.tip()? + 5;
    let t = env.terms(lock_height, false)?;
    let address = t.htlc.address(&AddressParams::ELEMENTS);

    // Too little, and a fake asset of the right amount.
    pay(&env.chain, &t.taker, vec![txout(usdt(), LIQ_AMOUNT - 100, address.script_pubkey())], env.genesis)?;
    env.node.mine(1)?;
    env.node.fund(&address, LIQ_AMOUNT, &env.fake_asset.to_string())?;
    env.node.mine(2)?;
    let coins = env.chain.coins(&address)?;
    check(coins.len() == 2, "both wrong coins are at the address")?;
    check(exact_coin(&coins, usdt(), LIQ_AMOUNT, env.tip()?, 2).is_none(), "the maker sees no acceptable coin, so it never locks")?;

    let fee = env.fund_fee_coin(&t, &t.taker)?;
    for coin in &coins {
        rejected(env.claim_liquid(&t, coin, &fee, t.preimage), "Rejected", "a wrong coin can't be claimed, even with the preimage")?;
    }
    while env.tip()? < lock_height {
        env.node.mine(1)?;
    }
    let to = address_of(&t.taker).script_pubkey();
    env.refund_liquid(&t, &coins[0], &fee, to.clone())?;
    let fee2 = env.fund_fee_coin_in(&t, &t.taker)?;
    env.refund_liquid(&t, &coins[1], &fee2, to)?;
    env.node.mine(1)?;
    check(env.chain.coins(&address)?.is_empty(), "both wrong coins were refunded")?;

    // Paid twice: the maker acts on one exact coin, and the other is refundable.
    let lock_height = env.tip()? + 6;
    let t = env.terms(lock_height, false)?;
    let address = t.htlc.address(&AddressParams::ELEMENTS);
    env.lock_liquid(&t, &t.taker, false)?;
    env.node.mine(1)?;
    env.lock_liquid(&t, &t.taker, false)?;
    env.node.mine(2)?;
    let coins = env.chain.coins(&address)?;
    check(coins.len() == 2, "two exact coins at one address")?;
    let fee = env.fund_fee_coin(&t, &t.maker)?;
    env.claim_liquid(&t, &coins[0], &fee, t.preimage)?;
    env.node.mine(1)?;
    let fee2 = env.fund_fee_coin_in(&t, &t.taker)?;
    while env.tip()? < lock_height {
        env.node.mine(1)?;
    }
    env.refund_liquid(&t, &coins[1], &fee2, address_of(&t.taker).script_pubkey())?;
    env.node.mine(1)?;
    check(env.chain.coins(&address)?.is_empty(), "the second payment came back")?;
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
    env.claim_liquid(&t, &coin, &fee, t.preimage)?;
    env.node.mine(1)?;
    let preimage = preimage_from_chain(env, &coin.outpoint, &t.hash)?;
    let before = env.tokens(&env.maker_sol.pubkey())?;
    env.sol.claim(&env.maker_sol, &escrow, &e, preimage)?;
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
    check(env.tip()? < t.params.lock_height, "the maker's refund is still not open")?;
    env.claim_liquid(&t, &coin, &fee, t.preimage)?;
    env.node.mine(1)?;
    check(env.holds(&t, &t.taker)? >= 11 * LIQ_AMOUNT, "the taker also holds the Liquid funds")?;

    let preimage = preimage_from_chain(env, &coin.outpoint, &t.hash)?;
    rejected(env.sol.claim(&env.maker_sol, &escrow, &e, preimage), "", "the maker, now knowing the preimage, has nothing left to claim")?;
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
    env.sol.claim(&env.taker_sol, &escrow, &e, t.preimage)?;
    env.wait_slot(deadline + 1)?;
    check(env.tip()? == halted_at && halted_at < lock_height, "the lock height can't pass during the halt")?;
    let settled = env.escrow(&escrow)?;
    env.claim_liquid(&t, &coin, &fee, settled.preimage)?;
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
    let lock_block = coin.height.ok_or("unconfirmed coin")?;
    let deadline = env.sol.slot()? + WINDOW;
    let escrow = env.sol.lock(&env.maker_sol, t.hash, env.taker_sol.pubkey(), SOL_AMOUNT, deadline, true)?;
    let e = env.escrow(&escrow)?;
    env.sol.claim(&env.taker_sol, &escrow, &e, t.preimage)?;
    check(env.escrow(&escrow)?.state == EscrowState::Claimed, "the taker has been paid on Solana")?;
    let tip_before = env.tip()?;

    println!("    The federation rolls Liquid back past the lock");
    env.node.roll_back_to_before(lock_block)?;
    check(env.tip()? < tip_before, "the tip went down, which a daemon must treat as an emergency")?;
    check(env.coin(&t, 0).is_err(), "the lock is gone")?;

    println!("    The taker double-spends the same coins back to itself");
    pay(&env.chain, &t.taker, vec![txout(usdt(), LIQ_AMOUNT, address_of(&t.taker).script_pubkey())], env.genesis)?;
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
    let claim_txid = env.claim_liquid(&t, &coin, &fee, t.preimage)?;
    let claim_tx = env.chain.tx(&claim_txid)?;
    env.node.mine(1)?;
    let claim_block = env.tip()?;
    let preimage = preimage_from_chain(env, &coin.outpoint, &t.hash)?;
    let e = env.escrow(&escrow)?;
    env.sol.claim(&env.maker_sol, &escrow, &e, preimage)?;
    println!("    The federation rolls Liquid back past the taker's claim");
    env.node.roll_back_to_before(claim_block)?;
    check(env.chain.spender(&coin.outpoint)?.is_none(), "the claim is gone, while the maker keeps its Solana funds")?;
    env.chain.broadcast(&claim_tx)?;
    env.node.mine(1)?;
    check(env.chain.spender(&coin.outpoint)?.is_some(), "a taker that rebroadcasts its claim still gets paid")?;

    // Then the bad case: the taker is gone, and the maker's refund opens.
    let lock_height = env.tip()? + 6;
    let t = env.terms(lock_height, true)?;
    let escrow = env.sol.lock(&env.taker_sol, t.hash, env.maker_sol.pubkey(), SOL_AMOUNT, env.sol.slot()? + 5_000, false)?;
    env.lock_liquid(&t, &t.maker, true)?;
    env.node.mine(2)?;
    let coin = env.coin(&t, 2)?;
    let fee = env.fee_coin(&t)?;
    env.claim_liquid(&t, &coin, &fee, t.preimage)?;
    env.node.mine(1)?;
    let claim_block = env.tip()?;
    let preimage = preimage_from_chain(env, &coin.outpoint, &t.hash)?;
    let e = env.escrow(&escrow)?;
    env.sol.claim(&env.maker_sol, &escrow, &e, preimage)?;
    env.node.roll_back_to_before(claim_block)?;
    while env.tip()? < lock_height {
        env.node.mine(1)?;
    }
    let refund_fee = env.fund_fee_coin_in(&t, &t.maker)?;
    env.node.mine(1)?;
    let to = address_of(&t.maker).script_pubkey();
    env.refund_liquid(&t, &coin, &refund_fee, to)?;
    env.node.mine(1)?;
    check(env.escrow(&escrow)?.state == EscrowState::Claimed, "the maker holds the Solana funds")?;
    let spent_to_maker = env.chain.spender(&coin.outpoint)?.ok_or("coin not spent")?;
    check(spent_to_maker.output[0].script_pubkey == address_of(&t.maker).script_pubkey(), "and got the Liquid funds back too")?;
    println!("    LOSS CONFIRMED: a rollback past the claim, with an absent taker, costs the taker the swap.");
    Ok(())
}

/// E16: Tether freezes accounts mid-swap, using a real freeze authority.
fn e16_freezes(env: &Env) -> Res<()> {
    let auth = &env.mint_authority;

    // The taker's account: simulation stops the claim, so the preimage stays secret and both sides refund.
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
    rejected(env.sol.claim(&env.taker_sol, &escrow, &e, t.preimage), "simulation failed", "a frozen taker's claim is stopped before sending")?;
    check(env.escrow(&escrow)?.preimage == [0; 32], "the preimage was not published")?;
    env.sol.set_frozen(auth, &taker_ata, false)?;
    env.wait_slot(deadline)?;
    env.sol.refund(&env.maker_sol, &escrow, &e)?;
    check(env.escrow(&escrow)?.state == EscrowState::Refunded, "the maker refunded on Solana")?;
    let fee = env.fund_fee_coin(&t, &t.taker)?;
    while env.tip()? < lock_height {
        env.node.mine(1)?;
    }
    env.refund_liquid(&t, &coin, &fee, address_of(&t.taker).script_pubkey())?;
    env.node.mine(1)?;
    check(env.chain.spender(&coin.outpoint)?.is_some(), "the taker refunded on Liquid, so nobody lost anything")?;

    // The escrow's own vault: both paths are stuck until a thaw, then one works.
    let t = env.terms(env.tip()? + 300, false)?;
    let deadline = env.sol.slot()? + WINDOW;
    let escrow = env.sol.lock(&env.maker_sol, t.hash, env.taker_sol.pubkey(), SOL_AMOUNT, deadline, true)?;
    let e = env.escrow(&escrow)?;
    let vault = vault_pda(&escrow);
    env.sol.set_frozen(auth, &vault, true)?;
    rejected(env.sol.claim(&env.taker_sol, &escrow, &e, t.preimage), "simulation failed", "a claim from a frozen vault is stopped")?;
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
    env.claim_liquid(&t, &coin, &fee, t.preimage)?;
    env.node.mine(1)?;
    let preimage = preimage_from_chain(env, &coin.outpoint, &t.hash)?;
    let e = env.escrow(&escrow)?;
    rejected(env.sol.claim(&env.maker_sol, &escrow, &e, preimage), "simulation failed", "a frozen maker can't collect after the reveal")?;
    env.wait_slot(deadline)?;
    env.sol.refund(&env.taker_sol, &escrow, &e)?;
    env.sol.set_frozen(auth, &maker_ata, false)?;
    check(env.escrow(&escrow)?.state == EscrowState::Refunded, "the deadline passed, and the taker refunded")?;
    check(env.holds(&t, &t.taker)? >= 11 * LIQ_AMOUNT, "the taker also holds the Liquid funds")?;
    println!("    LOSS CONFIRMED: a freeze of the maker's account across the deadline costs the maker the swap.");
    Ok(())
}

impl Env {
    /// A second fee coin to the same key, with a distinct key so that scenarios can hold more than one unspent.
    fn fund_fee_coin_in(&self, t: &Terms, payer: &LiquidKey) -> Res<Coin> {
        pay(&self.chain, payer, vec![txout(lbtc(), FEE, address_of(&t.fee_key).script_pubkey())], self.genesis)?;
        self.node.mine(1)?;
        let spent: Vec<Coin> = self.chain.coins(&address_of(&t.fee_key))?;
        spent.into_iter().next_back().ok_or_else(|| "no fee coin".to_string())
    }

    fn fund_fee_coin_for_late_maker(&self, t: &Terms) -> Res<Coin> {
        self.fund_fee_coin_in(t, &t.maker)
    }
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
    let filter = std::env::var("SCENARIO").unwrap_or_default();
    for (name, run) in scenarios {
        if !name.contains(&filter) {
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
