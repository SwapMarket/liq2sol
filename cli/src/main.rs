//! CLI-only end-to-end swaps on Liquid testnet and Solana devnet (or a local validator).
//!
//! This drives both roles in one process, keeping each role's secrets to itself: the "maker" here only learns the
//! preimage from the chain. It applies the checks from docs/protocol.md, and refuses to touch mainnet.
//!
//!   liq2sol-cli addresses             print the wallets to fund
//!   liq2sol-cli faucet                ask the Liquid testnet faucet for L-BTC and TEST for both wallets
//!   liq2sol-cli setup                 create the stand-in mint and token accounts
//!   liq2sol-cli liquid-check          the Liquid leg alone on testnet: one claim, and one refund with an early attempt
//!   liq2sol-cli swap l2s|s2l          a full swap
//!   liq2sol-cli abort l2s|s2l         the counterparty goes quiet: both sides recover through refunds
//!
//! Environment: SOLANA_RPC (default http://127.0.0.1:8899), LIQ2SOL_KEYS (default cli/.keys).

use liq2sol_chain::{liquid, sol};

use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, Instant};

use anchor_lang::prelude::Pubkey;
use liq2sol_htlc::{asset_param, script_hash, Htlc, HtlcParams};
use liquid::{address_of, exact_coin, keypair_from, lbtc, network, pay, random32, spend_htlc, txout, usdt, Coin, Esplora, HtlcSpend, Res, FEE};
use simplicityhl::elements::hashes::sha256;
use simplicityhl::elements::secp256k1_zkp::Keypair as LiquidKey;
use simplicityhl::elements::{BlockHash, OutPoint, Script};
use sol::{ata, Sol};
use solana_escrow::state::{Escrow, EscrowState};
use solana_sdk::signature::{read_keypair_file, write_keypair_file, Keypair};
use solana_sdk::signer::Signer;

/// Base units of the Solana stand-in (6 dp). The Liquid leg is this times 100, exactly.
const SOL_AMOUNT: u64 = 10;
const LIQ_AMOUNT: u64 = SOL_AMOUNT * 100;
const LIQ_CONFS: u32 = 2;
/// Slots per wall-clock minute are measured live, never assumed.
const POLL: Duration = Duration::from_secs(5);

const PRIMARY_ESPLORA: &str = "https://blockstream.info/liquidtestnet/api";
const SECOND_ESPLORA: &str = "https://liquid.network/liquidtestnet/api";

struct Wallets {
    taker_liq: LiquidKey,
    maker_liq: LiquidKey,
    taker_sol: Keypair,
    maker_sol: Keypair,
    mint_authority: Keypair,
}

fn keys_dir() -> PathBuf {
    std::env::var("LIQ2SOL_KEYS").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/.keys")))
}

fn load_liquid(name: &str) -> Res<LiquidKey> {
    let path = keys_dir().join(name);
    if !path.exists() {
        std::fs::create_dir_all(keys_dir()).map_err(|e| e.to_string())?;
        std::fs::write(&path, random32()?).map_err(|e| e.to_string())?;
    }
    let bytes: [u8; 32] = std::fs::read(&path).map_err(|e| e.to_string())?.try_into().map_err(|_| "bad key file")?;
    keypair_from(&bytes)
}

fn load_solana(name: &str) -> Res<Keypair> {
    let path = keys_dir().join(name);
    if !path.exists() {
        std::fs::create_dir_all(keys_dir()).map_err(|e| e.to_string())?;
        write_keypair_file(&Keypair::new(), &path).map_err(|e| e.to_string())?;
    }
    read_keypair_file(&path).map_err(|e| e.to_string())
}

fn wallets() -> Res<Wallets> {
    Ok(Wallets {
        taker_liq: load_liquid("taker-liquid.key")?,
        maker_liq: load_liquid("maker-liquid.key")?,
        taker_sol: load_solana("taker-solana.json")?,
        maker_sol: load_solana("maker-solana.json")?,
        mint_authority: load_solana("mint-authority.json")?,
    })
}

/// Every network is checked before any key is used.
struct Net {
    esplora: Esplora,
    second: Esplora,
    sol: Sol,
    genesis: BlockHash,
}

fn connect() -> Res<Net> {
    let esplora = Esplora::Http(PRIMARY_ESPLORA.into());
    let second = Esplora::Http(SECOND_ESPLORA.into());
    let genesis = esplora.genesis()?;
    if genesis != liquid::LIQUID_TESTNET_GENESIS {
        return Err(format!("refusing to run: not Liquid testnet (genesis {genesis})"));
    }
    let url = std::env::var("SOLANA_RPC").unwrap_or_else(|_| "http://127.0.0.1:8899".into());
    let sol = Sol::new(&url);
    let sol_genesis = sol.genesis()?;
    if sol_genesis == sol::MAINNET_GENESIS {
        return Err("refusing to run: this is Solana mainnet".into());
    }
    println!("Liquid testnet tip {}, Solana {url} slot {}", esplora.tip()?, sol.slot()?);
    Ok(Net { esplora, second, sol, genesis: BlockHash::from_str(&genesis).map_err(|e| e.to_string())? })
}

fn step(text: &str) {
    println!("\n==> {text}");
}

fn wait_for<T>(what: &str, limit: Duration, mut check: impl FnMut() -> Res<Option<T>>) -> Res<T> {
    let start = Instant::now();
    println!("    waiting for {what}");
    loop {
        match check() {
            Ok(Some(v)) => {
                println!("    done after {}s", start.elapsed().as_secs());
                return Ok(v);
            }
            Ok(None) => {}
            Err(e) => println!("    retrying: {}", e.chars().take(160).collect::<String>()),
        }
        if start.elapsed() > limit {
            return Err(format!("timed out waiting for {what}"));
        }
        std::thread::sleep(POLL);
    }
}

/// What both sides agreed on in the RFQ. Keys here are per-swap and ephemeral.
struct Swap {
    params: HtlcParams,
    htlc: Htlc,
    preimage: [u8; 32],
    payment_hash: [u8; 32],
    claim_key: LiquidKey,
    refund_key: LiquidKey,
    fee_key: LiquidKey,
    recipient_script: Script,
}

fn new_swap(lock_height: u32, claim_key: LiquidKey, refund_key: LiquidKey, recipient_script: Script) -> Res<Swap> {
    let preimage = random32()?;
    let payment_hash = sha256::Hash::hash(&preimage).to_byte_array();
    let params = HtlcParams {
        payment_hash,
        recipient_pk: claim_key.x_only_public_key().0.serialize(),
        sender_pk: refund_key.x_only_public_key().0.serialize(),
        lock_height,
        usdt_asset: asset_param(&usdt()),
        amount: LIQ_AMOUNT,
        recipient_script_hash: script_hash(&recipient_script),
    };
    let htlc = Htlc::new(&params).map_err(|e| e.to_string())?;
    println!("    HTLC address {} (CMR {})", htlc.address(network().params), hex(&htlc.cmr()));
    println!("    lock height {lock_height}, payment hash {}", hex(&payment_hash));
    let fee_key = keypair_from(&random32()?)?;
    Ok(Swap { params, htlc, preimage, payment_hash, claim_key, refund_key, fee_key, recipient_script })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Wait for the exact explicit coin at the expected depth. Other coins at the address are ignored.
fn wait_exact_coin(esplora: &Esplora, swap: &Swap, confs: u32, who: &str) -> Res<Coin> {
    let address = swap.htlc.address(network().params);
    wait_for(&format!("{who} to see the exact coin with {confs} confirmations"), Duration::from_secs(900), || {
        let tip = esplora.tip()?;
        Ok(exact_coin(&esplora.coins(&address)?, usdt(), LIQ_AMOUNT, tip, confs))
    })
}

/// Blocks per minute are close to 1, so this waits about a minute per block.
fn wait_height(esplora: &Esplora, height: u32) -> Res<()> {
    wait_for(&format!("Liquid tip to reach {height}"), Duration::from_secs(1800), || {
        Ok((esplora.tip()? >= height).then_some(()))
    })
}

fn wait_slot(sol: &Sol, slot: u64) -> Res<()> {
    wait_for(&format!("Solana slot {slot}"), Duration::from_secs(1800), || Ok((sol.slot()? >= slot).then_some(())))
}

/// L-BTC for a Liquid spend, kept separate so the coin carries exactly what the spend needs.
fn fund_fee_coin(net: &Net, payer: &LiquidKey, fee_key: &LiquidKey) -> Res<Coin> {
    pay_fee_coin(&net.esplora, payer, fee_key, net.genesis)
}

fn liquid_balance(esplora: &Esplora, key: &LiquidKey) -> Res<(u64, u64)> {
    let coins = esplora.coins(&address_of(key))?;
    let sum = |asset| {
        coins
            .iter()
            .filter(|c| c.out.asset.explicit() == Some(asset))
            .filter_map(|c| c.out.value.explicit())
            .sum::<u64>()
    };
    Ok((sum(usdt()), sum(lbtc())))
}

fn verify_maker_escrow(e: &Escrow, swap: &Swap, recipient: &Pubkey, slot: u64) -> Res<()> {
    if e.state != EscrowState::Open || e.amount != SOL_AMOUNT || e.payment_hash != swap.payment_hash || e.recipient != *recipient {
        return Err(format!("the maker's escrow does not match the terms: {e:?}"));
    }
    if e.deadline_slot <= slot + 600 {
        return Err("too close to the deadline to reveal the preimage".into());
    }
    Ok(())
}

/// L-USDt to SOL-USDT. Taker locks first on Liquid, long; maker locks on Solana, short.
fn swap_l2s(net: &Net, w: &Wallets, abort: bool) -> Res<()> {
    let tip = net.esplora.tip()?;
    // Normal timeouts are protocol.md section 3. The abort run shortens them so it finishes in minutes.
    let lock_height = if abort { tip + 8 } else { tip + 300 };
    let gate = if abort { 3 } else { 210 };
    let window: u64 = if abort { 600 } else { 8_400 };

    step("RFQ: the taker makes the secret, and both sides fix the terms");
    let taker_key = keypair_from(&random32()?)?; // ephemeral refund key
    let maker_key = keypair_from(&random32()?)?; // per-swap claim key
    let maker_script = address_of(&w.maker_liq).script_pubkey();
    let swap = new_swap(lock_height, maker_key, taker_key, maker_script)?;
    let (start_taker, start_maker) = (liquid_balance(&net.esplora, &w.taker_liq)?, liquid_balance(&net.esplora, &w.maker_liq)?);

    step("Taker pays the exact amount to the HTLC");
    let address = swap.htlc.address(network().params);
    let txid = pay(&net.esplora, &w.taker_liq, vec![txout(usdt(), LIQ_AMOUNT, address.script_pubkey())], net.genesis)?;
    println!("    {txid}");

    step("Maker checks the coin and its gate, then locks on Solana");
    let coin = wait_exact_coin(&net.esplora, &swap, LIQ_CONFS, "the maker")?;
    let tip = net.esplora.tip()?;
    if lock_height.saturating_sub(tip) < gate {
        return Err(format!("maker gate: only {} blocks left", lock_height - tip));
    }
    let slot = net.sol.slot()?;
    let deadline = slot + window;
    let escrow_addr = net.sol.lock(&w.maker_sol, swap.payment_hash, w.taker_sol.pubkey(), SOL_AMOUNT, deadline, true)?;
    println!("    escrow {escrow_addr}, deadline slot {deadline}");

    if abort {
        step("The taker goes quiet: the maker's Solana refund opens at its deadline");
        wait_slot(&net.sol, deadline)?;
        let e = net.sol.escrow(&escrow_addr)?.ok_or("escrow missing")?;
        println!("    {}", net.sol.refund(&w.maker_sol, &escrow_addr, &e)?);
        let e = net.sol.escrow(&escrow_addr)?.ok_or("escrow missing")?;
        assert_state(&e, EscrowState::Refunded)?;

        step("Too early: the node must refuse the taker's Liquid refund before the lock height");
        let fee_coin = fund_fee_coin(net, &w.taker_liq, &swap.fee_key)?;
        let refund = |net: &Net| {
            spend_htlc(
                &net.esplora,
                &swap.params,
                &coin,
                &fee_coin,
                &swap.fee_key,
                HtlcSpend::Refund { sender: swap.refund_key, lock_height, to: address_of(&w.taker_liq).script_pubkey() },
                net.genesis,
            )
        };
        if net.esplora.tip()? < lock_height {
            let early = refund(net);
            println!("    {}", early.as_ref().map(|t| t.to_string()).unwrap_or_else(|e| e.chars().take(120).collect()));
            if early.is_ok() {
                return Err("an early refund was accepted".into());
            }
        }

        step("The taker refunds on Liquid once the tip reaches the lock height");
        wait_height(&net.esplora, lock_height)?;
        println!("    {}", refund(net)?);
        wait_spent(&net.esplora, &coin.outpoint)?;
        println!("\nABORT l2s OK: both sides recovered their funds");
        return Ok(());
    }

    step("Taker verifies the escrow, simulates, and claims. This reveals the preimage");
    let e = wait_for("the taker to see the escrow", Duration::from_secs(120), || net.sol.escrow(&escrow_addr))?;
    verify_maker_escrow(&e, &swap, &w.taker_sol.pubkey(), net.sol.slot()?)?;
    if std::env::var("SOLANA_RPC2").is_err() {
        println!("    OVERRIDE: only one Solana endpoint is configured, so the two-endpoint check is skipped");
    } else {
        let second = Sol::new(&std::env::var("SOLANA_RPC2").map_err(|e| e.to_string())?);
        let other = second.escrow(&escrow_addr)?.ok_or("second endpoint does not see the escrow")?;
        verify_maker_escrow(&other, &swap, &w.taker_sol.pubkey(), second.slot()?)?;
    }
    let before = net.sol.token_balance(&ata(&w.taker_sol.pubkey()))?;
    println!("    {}", net.sol.claim(&w.taker_sol, &escrow_addr, &e, swap.preimage)?);
    let after = net.sol.token_balance(&ata(&w.taker_sol.pubkey()))?;
    if after != before + SOL_AMOUNT {
        return Err(format!("the taker received {} instead of {SOL_AMOUNT}", after - before));
    }

    step("Maker reads the preimage from the escrow account, and claims on Liquid");
    let settled = net.sol.escrow(&escrow_addr)?.ok_or("escrow missing")?;
    assert_state(&settled, EscrowState::Claimed)?;
    if sha256::Hash::hash(&settled.preimage).to_byte_array() != swap.payment_hash {
        return Err("the stored preimage does not match".into());
    }
    let fee_coin = fund_fee_coin(net, &w.maker_liq, &swap.fee_key)?;
    let claim = spend_htlc(
        &net.esplora,
        &swap.params,
        &coin,
        &fee_coin,
        &swap.fee_key,
        HtlcSpend::Claim { preimage: settled.preimage, recipient: swap.claim_key, to: swap.recipient_script.clone() },
        net.genesis,
    )?;
    println!("    {claim}");
    wait_spent(&net.esplora, &coin.outpoint)?;

    let (end_taker, end_maker) = (liquid_balance(&net.esplora, &w.taker_liq)?, liquid_balance(&net.esplora, &w.maker_liq)?);
    println!("\nSWAP l2s OK. TEST units: taker {} -> {}, maker {} -> {}", start_taker.0, end_taker.0, start_maker.0, end_maker.0);
    println!("Solana stand-in received by the taker: {SOL_AMOUNT}");
    Ok(())
}

/// SOL-USDT to L-USDt. Taker locks first on Solana, long; maker locks on Liquid, short.
fn swap_s2l(net: &Net, w: &Wallets, abort: bool) -> Res<()> {
    let tip = net.esplora.tip()?;
    let lock_height = if abort { tip + 6 } else { tip + 90 };
    let window: u64 = if abort { 1_200 } else { 101_400 };
    let min_left: u64 = if abort { 600 } else { 84_500 };

    step("RFQ: the taker makes the secret, and both sides fix the terms");
    let taker_key = keypair_from(&random32()?)?; // ephemeral claim key
    let maker_key = keypair_from(&random32()?)?; // per-swap refund key
    let taker_script = address_of(&w.taker_liq).script_pubkey();
    let swap = new_swap(lock_height, taker_key, maker_key, taker_script)?;

    step("Taker locks on Solana");
    let deadline = net.sol.slot()? + window;
    let escrow_addr = net.sol.lock(&w.taker_sol, swap.payment_hash, w.maker_sol.pubkey(), SOL_AMOUNT, deadline, false)?;
    println!("    escrow {escrow_addr}, deadline slot {deadline}");

    if abort {
        step("The maker never locks: the taker refunds on Solana at the deadline");
        wait_slot(&net.sol, deadline)?;
        let e = net.sol.escrow(&escrow_addr)?.ok_or("escrow missing")?;
        println!("    {}", net.sol.refund(&w.taker_sol, &escrow_addr, &e)?);
        assert_state(&net.sol.escrow(&escrow_addr)?.ok_or("escrow missing")?, EscrowState::Refunded)?;
        println!("\nABORT s2l OK: the taker recovered its funds");
        return Ok(());
    }

    step("Maker verifies the escrow and its gate, then locks on Liquid with the taker's fee coin");
    let e = net.sol.escrow(&escrow_addr)?.ok_or("the maker does not see the escrow")?;
    if e.state != EscrowState::Open || e.close_on_restart || e.amount != SOL_AMOUNT || e.recipient != w.maker_sol.pubkey() || e.payment_hash != swap.payment_hash {
        return Err(format!("the taker's escrow does not match the terms: {e:?}"));
    }
    if e.deadline_slot.saturating_sub(net.sol.slot()?) < min_left {
        return Err("maker gate: too few slots left".into());
    }
    let address = swap.htlc.address(network().params);
    let fee_address = address_of(&swap.fee_key);
    let txid = pay(
        &net.esplora,
        &w.maker_liq,
        vec![txout(usdt(), LIQ_AMOUNT, address.script_pubkey()), txout(lbtc(), FEE, fee_address.script_pubkey())],
        net.genesis,
    )?;
    println!("    {txid}");

    step("Taker confirms the lock on two independent endpoints, then claims. This reveals the preimage");
    let coin = wait_exact_coin(&net.esplora, &swap, LIQ_CONFS, "the taker (first endpoint)")?;
    wait_exact_coin(&net.second, &swap, LIQ_CONFS, "the taker (second endpoint)")?;
    if lock_height.saturating_sub(net.esplora.tip()?) < 10 {
        return Err("claim cutoff: too close to the lock height to reveal".into());
    }
    let fee_coin = net.esplora.coins(&fee_address)?.into_iter().find(|c| c.out.value.explicit() == Some(FEE)).ok_or("no fee coin from the maker")?;
    let claim = spend_htlc(
        &net.esplora,
        &swap.params,
        &coin,
        &fee_coin,
        &swap.fee_key,
        HtlcSpend::Claim { preimage: swap.preimage, recipient: swap.claim_key, to: swap.recipient_script.clone() },
        net.genesis,
    )?;
    println!("    {claim}");

    step("Maker reads the preimage from the Liquid witness, and claims on Solana");
    let spender = wait_for("the maker to see the claim", Duration::from_secs(300), || net.esplora.spender(&coin.outpoint))?;
    let preimage = liquid::unhex_tx_witness_preimage(&spender, &coin.outpoint, &swap.payment_hash).ok_or("no preimage in the witness")?;
    println!("    recovered preimage from the witness");
    let before = net.sol.token_balance(&ata(&w.maker_sol.pubkey()))?;
    println!("    {}", net.sol.claim(&w.maker_sol, &escrow_addr, &e, preimage)?);
    let after = net.sol.token_balance(&ata(&w.maker_sol.pubkey()))?;
    if after != before + SOL_AMOUNT {
        return Err(format!("the maker received {} instead of {SOL_AMOUNT}", after - before));
    }
    wait_spent(&net.esplora, &coin.outpoint)?;
    println!("\nSWAP s2l OK");
    Ok(())
}

fn assert_state(e: &Escrow, expected: EscrowState) -> Res<()> {
    if e.state == expected {
        Ok(())
    } else {
        Err(format!("escrow is {:?}, expected {expected:?}", e.state))
    }
}

fn wait_spent(esplora: &Esplora, outpoint: &OutPoint) -> Res<()> {
    wait_for("the Liquid spend to be visible", Duration::from_secs(300), || Ok(esplora.spender(outpoint)?.map(|_| ())))
}

/// The Liquid leg on the real testnet, with no Solana involved.
fn liquid_check(w: &Wallets) -> Res<()> {
    let esplora = Esplora::Http(PRIMARY_ESPLORA.into());
    let genesis = esplora.genesis()?;
    if genesis != liquid::LIQUID_TESTNET_GENESIS {
        return Err(format!("refusing to run: not Liquid testnet (genesis {genesis})"));
    }
    let genesis = BlockHash::from_str(&genesis).map_err(|e| e.to_string())?;
    let second = Esplora::Http(SECOND_ESPLORA.into());
    println!("Liquid testnet tip {}", esplora.tip()?);

    step("Claim: lock, wait for confirmations on both endpoints, claim, and read the preimage back");
    let script = address_of(&w.maker_liq).script_pubkey();
    let swap = new_swap(esplora.tip()? + 100, keypair_from(&random32()?)?, keypair_from(&random32()?)?, script.clone())?;
    let lock = pay(&esplora, &w.taker_liq, vec![txout(usdt(), LIQ_AMOUNT, swap.htlc.address(network().params).script_pubkey())], genesis)?;
    println!("    lock {lock}");
    let coin = wait_exact_coin(&esplora, &swap, LIQ_CONFS, "first endpoint")?;
    wait_exact_coin(&second, &swap, LIQ_CONFS, "second endpoint")?;
    let fee = pay_fee_coin(&esplora, &w.maker_liq, &swap.fee_key, genesis)?;
    let claim = spend_htlc(
        &esplora,
        &swap.params,
        &coin,
        &fee,
        &swap.fee_key,
        HtlcSpend::Claim { preimage: swap.preimage, recipient: swap.claim_key, to: script },
        genesis,
    )?;
    println!("    claim {claim}");
    let spender = wait_for("the claim", Duration::from_secs(300), || esplora.spender(&coin.outpoint))?;
    let read = liquid::unhex_tx_witness_preimage(&spender, &coin.outpoint, &swap.payment_hash);
    if read != Some(swap.preimage) {
        return Err("could not read the preimage back from the chain".into());
    }
    println!("    preimage read back from the chain");

    step("Refund: an early attempt must fail, and the refund must work at the lock height");
    let lock_height = esplora.tip()? + 5;
    let swap = new_swap(lock_height, keypair_from(&random32()?)?, keypair_from(&random32()?)?, address_of(&w.maker_liq).script_pubkey())?;
    let lock = pay(&esplora, &w.taker_liq, vec![txout(usdt(), LIQ_AMOUNT, swap.htlc.address(network().params).script_pubkey())], genesis)?;
    println!("    lock {lock}");
    let coin = wait_exact_coin(&esplora, &swap, 1, "first endpoint")?;
    let fee = pay_fee_coin(&esplora, &w.taker_liq, &swap.fee_key, genesis)?;
    let refund = || {
        spend_htlc(
            &esplora,
            &swap.params,
            &coin,
            &fee,
            &swap.fee_key,
            HtlcSpend::Refund { sender: swap.refund_key, lock_height, to: address_of(&w.taker_liq).script_pubkey() },
            genesis,
        )
    };
    if esplora.tip()? < lock_height {
        match refund() {
            Ok(t) => return Err(format!("an early refund was accepted: {t}")),
            Err(e) => println!("    early refund refused: {}", e.chars().take(140).collect::<String>()),
        }
    } else {
        println!("    NOT CHECKED: the tip reached the lock height before the early attempt");
    }
    wait_height(&esplora, lock_height)?;
    println!("    refund {}", refund()?);
    wait_spent(&esplora, &coin.outpoint)?;
    println!("\nLIQUID CHECK OK");
    Ok(())
}

fn pay_fee_coin(esplora: &Esplora, payer: &LiquidKey, fee_key: &LiquidKey, genesis: BlockHash) -> Res<Coin> {
    let address = address_of(fee_key);
    let txid = pay(esplora, payer, vec![txout(lbtc(), FEE, address.script_pubkey())], genesis)?;
    println!("    fee coin in {txid}");
    wait_for("the fee coin", Duration::from_secs(300), || {
        Ok(esplora.coins(&address)?.into_iter().find(|c| c.out.value.explicit() == Some(FEE)))
    })
}

fn faucet(address: &str, action: &str) -> Res<String> {
    let url = format!("https://liquidtestnet.com/api/faucet?address={address}&action={action}");
    ureq::get(&url)
        .timeout(Duration::from_secs(60))
        .call()
        .map_err(|e| format!("faucet: {e}"))?
        .into_string()
        .map_err(|e| e.to_string())
}

fn run() -> Res<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let w = wallets()?;
    match args.as_slice() {
        ["addresses"] => {
            println!("taker Liquid  {}", address_of(&w.taker_liq));
            println!("maker Liquid  {}", address_of(&w.maker_liq));
            println!("taker Solana  {}", w.taker_sol.pubkey());
            println!("maker Solana  {}", w.maker_sol.pubkey());
            println!("mint authority {}", w.mint_authority.pubkey());
        }
        ["faucet"] => {
            for key in [&w.taker_liq, &w.maker_liq] {
                let address = address_of(key).to_string();
                for action in ["lbtc", "test"] {
                    println!("{action} -> {address}: {}", faucet(&address, action)?.chars().take(200).collect::<String>());
                    std::thread::sleep(Duration::from_secs(3));
                }
            }
        }
        ["balances"] => {
            let net = connect()?;
            println!("taker Liquid (TEST, L-BTC) {:?}", liquid_balance(&net.esplora, &w.taker_liq)?);
            println!("maker Liquid (TEST, L-BTC) {:?}", liquid_balance(&net.esplora, &w.maker_liq)?);
            for (name, k) in [("taker", &w.taker_sol), ("maker", &w.maker_sol)] {
                println!("{name} Solana: {} lamports, {} stand-in units", net.sol.lamports(&k.pubkey())?, net.sol.token_balance(&ata(&k.pubkey()))?);
            }
        }
        ["setup", mint_key] => {
            let net = connect()?;
            let mint = read_keypair_file(mint_key).map_err(|e| e.to_string())?;
            net.sol.setup_mint(&w.mint_authority, &mint, &[w.taker_sol.pubkey(), w.maker_sol.pubkey()], 100 * SOL_AMOUNT)?;
            net.sol.top_up_token(&w.mint_authority, &w.maker_sol.pubkey(), 100 * SOL_AMOUNT)?;
            println!("stand-in mint {} ready", solana_escrow::USDT_MINT);
        }
        ["liquid-check"] => liquid_check(&w)?,
        ["swap", "l2s"] => swap_l2s(&connect()?, &w, false)?,
        ["swap", "s2l"] => swap_s2l(&connect()?, &w, false)?,
        ["abort", "l2s"] => swap_l2s(&connect()?, &w, true)?,
        ["abort", "s2l"] => swap_s2l(&connect()?, &w, true)?,
        _ => return Err("usage: addresses | faucet | balances | liquid-check | setup <mint-keypair> | swap l2s|s2l | abort l2s|s2l".into()),
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

