//! CLI-only end-to-end swaps on Liquid testnet and Solana devnet (or a local validator).
//!
//! This drives both roles in one process, keeping each role's secrets to itself: the "maker" here only learns the
//! secret from the chain. It applies the checks from docs/protocol.md and docs/confidential.md, and refuses to touch mainnet.
//!
//!   liq2sol-cli addresses             print the wallets to fund
//!   liq2sol-cli faucet                ask the Liquid testnet faucet for L-BTC and TEST for both wallets
//!   liq2sol-cli faucet-init <program> create the devnet faucet's vault
//!   liq2sol-cli setup                 create the stand-in mint and token accounts
//!   liq2sol-cli swap l2s|s2l          a full swap
//!   liq2sol-cli abort l2s|s2l         the counterparty goes quiet: both sides recover through refunds
//!
//! Environment: SOLANA_RPC (default http://127.0.0.1:8899), LIQ2SOL_KEYS (default cli/.keys).

use liq2sol_chain::{liquid, sol};

use std::path::PathBuf;
use solana_sdk::instruction::{AccountMeta, Instruction};
use std::str::FromStr;
use std::time::{Duration, Instant};

use anchor_lang::prelude::Pubkey;
use liq2sol_chain::swap::{answer_claim, check_claim_shape, spent_outputs, TakerClaim};
use liq2sol_swapkey::{
    combine, complete, extract_secret, key_spend_sighash, point_of, secret_from_solana, set_key_spend_witness,
    solana_value, verify_share, Session, SwapOutput, SwapParams,
};
use liquid::{
    build_issuance, build_swap_spend, exact_secret_coin, keypair_from, lbtc, network, published_signature, random32, sign_keypath, usdt,
    Payment, Res, SecretCoin, Wallet, FEE,
};
use liq2sol_chain::esplora::{build_confidential_pay, Esplora};
use simplicityhl::elements::encode::serialize_hex;
use simplicityhl::elements::secp256k1_zkp::{Keypair as LiquidKey, Message, PublicKey, Secp256k1, SecretKey};
use simplicityhl::elements::{Address, AssetId, BlockHash, OutPoint, Sequence, Transaction};
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
    taker_liq: Wallet,
    maker_liq: Wallet,
    taker_sol: Keypair,
    maker_sol: Keypair,
    mint_authority: Keypair,
}

fn keys_dir() -> PathBuf {
    std::env::var("LIQ2SOL_KEYS").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/.keys")))
}

fn load_wallet(role: &str) -> Res<Wallet> {
    let key = keypair_from(&load_secret(&format!("{role}-liquid.key"))?)?;
    let blinding = SecretKey::from_slice(&load_secret(&format!("{role}-blinding.key"))?).map_err(|e| e.to_string())?;
    Ok(Wallet { key, blinding })
}

fn load_secret(name: &str) -> Res<[u8; 32]> {
    let path = keys_dir().join(name);
    if !path.exists() {
        std::fs::create_dir_all(keys_dir()).map_err(|e| e.to_string())?;
        std::fs::write(&path, random32()?).map_err(|e| e.to_string())?;
    }
    std::fs::read(&path).map_err(|e| e.to_string())?.try_into().map_err(|_| "bad key file".to_string())
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
        taker_liq: load_wallet("taker")?,
        maker_liq: load_wallet("maker")?,
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

/// What both sides agreed on in the RFQ. Each role only ever uses its own keys, and the maker learns the secret from the chain.
struct Swap {
    secret: [u8; 32],
    point: [u8; 33],
    solana_point: [u8; 64],
    maker_key: LiquidKey,
    taker_key: LiquidKey,
    blinding: SecretKey,
    output: SwapOutput,
    address: Address,
}

/// `refund_by_taker`: who funded the coin, and so who can take it back.
fn new_swap(lock_height: u32, refund_by_taker: bool) -> Res<Swap> {
    let secret = random32()?;
    let point = point_of(&secret).map_err(|e| e.to_string())?;
    let solana_point = liq2sol_swapkey::solana_point(&secret).map_err(|e| e.to_string())?;
    let (maker_key, taker_key) = (keypair_from(&random32()?)?, keypair_from(&random32()?)?);
    let blinding = SecretKey::from_slice(&random32()?).map_err(|e| e.to_string())?;
    let refunder = if refund_by_taker { &taker_key } else { &maker_key };
    let output = SwapOutput::new(&SwapParams {
        maker_key: maker_key.public_key().serialize(),
        taker_key: taker_key.public_key().serialize(),
        refund_pk: refunder.x_only_public_key().0.serialize(),
        lock_height,
    })
    .map_err(|e| e.to_string())?;
    let blinder = PublicKey::from_secret_key(&Secp256k1::new(), &blinding);
    let address = output.address(blinder, network().params);
    println!("    confidential address {address}");
    println!("    lock height {lock_height}, lock point {}", hex(&point));
    Ok(Swap { secret, point, solana_point, maker_key, taker_key, blinding, output, address })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Wait for the coin to unblind to exactly the agreed asset and amount, at this depth. The chain checks neither.
fn wait_exact_coin(esplora: &Esplora, swap: &Swap, confs: u32, who: &str) -> Res<SecretCoin> {
    wait_for(&format!("{who} to read the exact coin with {confs} confirmations"), Duration::from_secs(900), || {
        let tip = esplora.tip()?;
        Ok(exact_secret_coin(&esplora.secret_coins(&swap.address, &swap.blinding)?, usdt(), LIQ_AMOUNT, tip, confs))
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

/// A confirmed fee coin paid to a throwaway key, because a claim's signature commits to the coin it spends.
fn make_fee_coin(net: &Net, payer: &Wallet) -> Res<(Wallet, SecretCoin)> {
    let holder = Wallet {
        key: keypair_from(&random32()?)?,
        blinding: SecretKey::from_slice(&random32()?).map_err(|e| e.to_string())?,
    };
    let payment = Payment { asset: lbtc(), amount: FEE, address: holder.address() };
    let tx = build_confidential_pay(&net.esplora, payer, vec![payment], net.genesis)?;
    println!("    fee coin in {}", net.esplora.broadcast(&tx)?);
    let coin = wait_for("the fee coin to confirm", Duration::from_secs(300), || {
        let coins = net.esplora.secret_coins(&holder.address(), &holder.blinding)?;
        Ok(coins.into_iter().find(|c| c.secrets.value == FEE && c.coin.height.is_some()))
    })?;
    Ok((holder, coin))
}

/// What the wallet holds, blinded and explicit alike. A wallet starts with explicit faucet coins, and its change is blinded.
fn liquid_balance(esplora: &Esplora, w: &Wallet) -> Res<(u64, u64)> {
    let coins = esplora.secret_coins(&w.address(), &w.blinding)?;
    let sum = |asset| coins.iter().filter(|c| c.secrets.asset == asset).map(|c| c.secrets.value).sum::<u64>();
    Ok((sum(usdt()), sum(lbtc())))
}

fn verify_maker_escrow(e: &Escrow, swap: &Swap, recipient: &Pubkey, slot: u64) -> Res<()> {
    if e.state != EscrowState::Open || e.amount != SOL_AMOUNT || e.lock_point != swap.solana_point || e.recipient != *recipient {
        return Err(format!("the maker's escrow does not match the terms: {e:?}"));
    }
    if e.deadline_slot <= slot + 600 {
        return Err("too close to the deadline to reveal the secret".into());
    }
    Ok(())
}

/// Show what an outside observer gets from a spend of the swap's coin.
fn report_privacy(tx: &Transaction, coin: &OutPoint) -> Res<()> {
    let hidden = tx.output.iter().filter(|o| !o.is_fee()).all(|o| !o.asset.is_explicit() && !o.value.is_explicit());
    let raw = serialize_hex(tx);
    println!("    outputs hidden: {hidden}, witness items on the swap coin: {:?}", witness_items(tx, coin));
    println!("    size {} bytes, txid {}", raw.len() / 2, tx.txid());
    if !hidden {
        return Err("an output is visible".into());
    }
    Ok(())
}

fn witness_items(tx: &Transaction, coin: &OutPoint) -> Option<usize> {
    tx.input.iter().find(|i| i.previous_output == *coin).map(|i| i.witness.script_witness.len())
}

/// L-USDt to SOL-USDT. Taker pays first on Liquid, long; maker locks on Solana, short, once it holds the adapter signature.
fn swap_l2s(net: &Net, w: &Wallets, abort: bool) -> Res<()> {
    let tip = net.esplora.tip()?;
    // Normal timeouts are protocol.md section 3. The abort run shortens them so it finishes in minutes.
    let lock_height = if abort { tip + 12 } else { tip + 300 };
    let gate = if abort { 3 } else { 210 };
    let window: u64 = if abort { 600 } else { 8_400 };

    step("RFQ: the taker makes the secret and the blinding key, and both sides fix the terms");
    let swap = new_swap(lock_height, true)?;
    let (start_taker, start_maker) = (liquid_balance(&net.esplora, &w.taker_liq)?, liquid_balance(&net.esplora, &w.maker_liq)?);

    step("Taker pays the exact amount to the confidential address, as any wallet could");
    let payment = Payment { asset: usdt(), amount: LIQ_AMOUNT, address: swap.address.clone() };
    let funding = build_confidential_pay(&net.esplora, &w.taker_liq, vec![payment], net.genesis)?;
    println!("    {}", net.esplora.broadcast(&funding)?);

    step("Maker reads the coin and checks its gate");
    let coin = wait_exact_coin(&net.esplora, &swap, LIQ_CONFS, "the maker")?;
    let tip = net.esplora.tip()?;
    if lock_height.saturating_sub(tip) < gate {
        return Err(format!("maker gate: only {} blocks left", lock_height - tip));
    }

    step("Signing: the maker gets an adapter signature for its claim, before risking anything");
    let (maker_fee, fee_coin) = make_fee_coin(net, &w.maker_liq)?;
    let (mut claim_tx, utxos) = build_swap_spend(&coin, &fee_coin, &w.maker_liq.address(), 0, Sequence::MAX)?;
    let message = key_spend_sighash(&claim_tx, &utxos, 0, net.genesis).map_err(|e| e.to_string())?;
    let ctx = swap.output.key_ctx();
    let maker_sk = swap.maker_key.secret_bytes();
    let session = Session::start(&maker_sk, ctx, &message).map_err(|e| e.to_string())?;
    let maker_nonce = session.public.clone();
    let (taker_nonce, taker_share) = answer_claim(
        &net.esplora,
        &swap.taker_key,
        &swap.output,
        &coin.coin.outpoint,
        &swap.point,
        &maker_nonce,
        &claim_tx,
        net.genesis,
    )?;
    let (maker_share, aggregate) =
        session.sign(&maker_sk, ctx, &taker_nonce, &swap.point, &message).map_err(|e| e.to_string())?;
    let taker_compressed = swap.taker_key.public_key().serialize();
    verify_share(ctx, taker_share, &aggregate, &taker_compressed, &taker_nonce, &swap.point, &message)
        .map_err(|e| e.to_string())?;
    let adaptor = combine(ctx, &aggregate, [maker_share, taker_share], &swap.point, &message).map_err(|e| e.to_string())?;
    println!("    the adapter signature checks out against the output key");

    step("Maker locks on Solana");
    let slot = net.sol.slot()?;
    let deadline = slot + window;
    let escrow_addr = net.sol.lock(&w.maker_sol, swap.solana_point, w.taker_sol.pubkey(), SOL_AMOUNT, deadline, true)?;
    println!("    escrow {escrow_addr}, deadline slot {deadline}");

    if abort {
        step("The taker goes quiet: the maker's Solana refund opens at its deadline");
        wait_slot(&net.sol, deadline)?;
        let e = net.sol.escrow(&escrow_addr)?.ok_or("escrow missing")?;
        println!("    {}", net.sol.refund(&w.maker_sol, &escrow_addr, &e)?);
        let e = net.sol.escrow(&escrow_addr)?.ok_or("escrow missing")?;
        assert_state(&e, EscrowState::Refunded)?;

        step("Too early: the node must refuse the taker's Liquid refund before the lock height");
        let (taker_fee, fee_coin) = make_fee_coin(net, &w.taker_liq)?;
        let refund = |net: &Net| -> Res<Transaction> {
            let (tx, utxos) =
                build_swap_spend(&coin, &fee_coin, &w.taker_liq.address(), lock_height, Sequence::ZERO)?;
            let digest = swap.output.refund_sighash(&tx, &utxos, 0, net.genesis).map_err(|e| e.to_string())?;
            let sig = *Secp256k1::new().sign_schnorr_no_aux_rand(&Message::from_digest(digest), &swap.taker_key).as_ref();
            let mut tx = swap.output.finalize_refund(tx, &utxos, 0, net.genesis, sig).map_err(|e| e.to_string())?;
            sign_keypath(&mut tx, &utxos, 1, &taker_fee.key, net.genesis)?;
            Ok(tx)
        };
        if net.esplora.tip()? < lock_height {
            let early = net.esplora.broadcast(&refund(net)?);
            println!("    {}", early.as_ref().map(|t| t.to_string()).unwrap_or_else(|e| e.chars().take(120).collect()));
            if early.is_ok() {
                return Err("an early refund was accepted".into());
            }
        }

        step("The taker refunds on Liquid once the tip reaches the lock height");
        wait_height(&net.esplora, lock_height)?;
        let tx = refund(net)?;
        println!("    {}", net.esplora.broadcast(&tx)?);
        wait_spent(&net.esplora, &coin.coin.outpoint)?;
        report_privacy(&tx, &coin.coin.outpoint)?;
        println!("\nABORT l2s OK: both sides recovered their funds");
        return Ok(());
    }

    step("Taker verifies the escrow, simulates, and claims. This reveals the secret to the maker, and only the maker");
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
    let revealed = solana_value(&swap.secret).map_err(|e| e.to_string())?;
    println!("    {}", net.sol.claim(&w.taker_sol, &escrow_addr, &e, revealed)?);
    let after = net.sol.token_balance(&ata(&w.taker_sol.pubkey()))?;
    if after != before + SOL_AMOUNT {
        return Err(format!("the taker received {} instead of {SOL_AMOUNT}", after - before));
    }

    step("Maker computes the secret from what the claim revealed, finishes its signature, and publishes");
    let settled = net.sol.escrow(&escrow_addr)?.ok_or("escrow missing")?;
    assert_state(&settled, EscrowState::Claimed)?;
    let learned = secret_from_solana(&settled.revealed).map_err(|e| e.to_string())?;
    if point_of(&learned).map_err(|e| e.to_string())? != swap.point {
        return Err("what the claim revealed does not open the point".into());
    }
    let signature = complete(&adaptor, &learned).map_err(|e| e.to_string())?;
    set_key_spend_witness(&mut claim_tx, 0, &signature);
    sign_keypath(&mut claim_tx, &utxos, 1, &maker_fee.key, net.genesis)?;
    println!("    {}", net.esplora.broadcast(&claim_tx)?);
    wait_spent(&net.esplora, &coin.coin.outpoint)?;
    report_privacy(&claim_tx, &coin.coin.outpoint)?;

    wait_for("the maker's proceeds to confirm", Duration::from_secs(300), || {
        Ok((liquid_balance(&net.esplora, &w.maker_liq)?.0 == start_maker.0 + LIQ_AMOUNT).then_some(()))
    })?;
    let (end_taker, end_maker) = (liquid_balance(&net.esplora, &w.taker_liq)?, liquid_balance(&net.esplora, &w.maker_liq)?);
    println!("\nSWAP l2s OK. USDt units: taker {} -> {}, maker {} -> {}", start_taker.0, end_taker.0, start_maker.0, end_maker.0);
    println!("Solana stand-in received by the taker: {SOL_AMOUNT}");
    Ok(())
}

/// SOL-USDT to L-USDt. Taker locks first on Solana, long; maker pays on Liquid, short.
fn swap_s2l(net: &Net, w: &Wallets, abort: bool) -> Res<()> {
    let tip = net.esplora.tip()?;
    let lock_height = if abort { tip + 6 } else { tip + 90 };
    let window: u64 = if abort { 1_200 } else { 101_400 };
    let min_left: u64 = if abort { 600 } else { 84_500 };

    step("RFQ: the taker makes the secret and the blinding key, and both sides fix the terms");
    let swap = new_swap(lock_height, false)?;

    step("Taker locks on Solana");
    let deadline = net.sol.slot()? + window;
    let escrow_addr = net.sol.lock(&w.taker_sol, swap.solana_point, w.maker_sol.pubkey(), SOL_AMOUNT, deadline, false)?;
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

    step("Maker verifies the escrow and its gate, then pays the address and the taker's fee coin, both hidden");
    let e = net.sol.escrow(&escrow_addr)?.ok_or("the maker does not see the escrow")?;
    if e.state != EscrowState::Open || e.close_on_restart || e.amount != SOL_AMOUNT || e.recipient != w.maker_sol.pubkey() || e.lock_point != swap.solana_point {
        return Err(format!("the taker's escrow does not match the terms: {e:?}"));
    }
    if e.deadline_slot.saturating_sub(net.sol.slot()?) < min_left {
        return Err("maker gate: too few slots left".into());
    }
    let taker_fee = Wallet { key: swap.taker_key, blinding: swap.blinding };
    let funding = build_confidential_pay(
        &net.esplora,
        &w.maker_liq,
        vec![
            Payment { asset: usdt(), amount: LIQ_AMOUNT, address: swap.address.clone() },
            Payment { asset: lbtc(), amount: FEE, address: taker_fee.address() },
        ],
        net.genesis,
    )?;
    println!("    {}", net.esplora.broadcast(&funding)?);

    step("Taker reads the lock on two independent endpoints");
    let coin = wait_exact_coin(&net.esplora, &swap, LIQ_CONFS, "the taker (first endpoint)")?;
    wait_exact_coin(&net.second, &swap, LIQ_CONFS, "the taker (second endpoint)")?;
    if lock_height.saturating_sub(net.esplora.tip()?) < 10 {
        return Err("claim cutoff: too close to the lock height to reveal".into());
    }
    let fee_coin = net.esplora.secret_coins(&taker_fee.address(), &swap.blinding)?.into_iter().find(|c| c.secrets.value == FEE).ok_or("no fee coin from the maker")?;

    step("Signing: the maker keeps the adapter signature before giving its share");
    let (tx, _) = build_swap_spend(&coin, &fee_coin, &w.taker_liq.address(), 0, Sequence::MAX)?;
    let claim = TakerClaim::start(&net.esplora, &swap.taker_key, &swap.output, tx, net.genesis)?;
    let taker_nonce = claim.nonce();
    // The maker's side, from what the taker sent.
    check_claim_shape(&claim.tx, &coin.coin.outpoint)?;
    let utxos = spent_outputs(&net.esplora, &claim.tx)?;
    let message = key_spend_sighash(&claim.tx, &utxos, 0, net.genesis).map_err(|e| e.to_string())?;
    let ctx = swap.output.key_ctx();
    let maker_sk = swap.maker_key.secret_bytes();
    let session = Session::start(&maker_sk, ctx, &message).map_err(|e| e.to_string())?;
    let maker_nonce = session.public.clone();
    let (taker_share, signed) = claim.sign(&swap.taker_key, &swap.output, &swap.point, &maker_nonce)?;
    let (maker_share, aggregate) =
        session.sign(&maker_sk, ctx, &taker_nonce, &swap.point, &message).map_err(|e| e.to_string())?;
    let taker_compressed = swap.taker_key.public_key().serialize();
    verify_share(ctx, taker_share, &aggregate, &taker_compressed, &taker_nonce, &swap.point, &message)
        .map_err(|e| e.to_string())?;
    let held = combine(ctx, &aggregate, [maker_share, taker_share], &swap.point, &message).map_err(|e| e.to_string())?;

    step("Taker finishes the claim and publishes. Nothing on-chain shows it is part of a swap");
    let maker_compressed = swap.maker_key.public_key().serialize();
    let final_tx = signed.finish(&swap.taker_key, &swap.output, &maker_compressed, maker_share, &swap.point, &swap.secret, net.genesis)?;
    println!("    {}", net.esplora.broadcast(&final_tx)?);

    step("Maker reads the secret from the published signature, and claims on Solana");
    let spender = wait_for("the maker to see the claim", Duration::from_secs(300), || net.esplora.spender(&coin.coin.outpoint))?;
    report_privacy(&spender, &coin.coin.outpoint)?;
    let signature = published_signature(&spender, &coin.coin.outpoint).ok_or("no key-path signature")?;
    let secret = extract_secret(&held, &signature, &swap.point).ok_or("the signature does not give the secret")?;
    println!("    recovered the secret from the signature");
    let before = net.sol.token_balance(&ata(&w.maker_sol.pubkey()))?;
    let revealed = solana_value(&secret).map_err(|e| e.to_string())?;
    println!("    {}", net.sol.claim(&w.maker_sol, &escrow_addr, &e, revealed)?);
    let after = net.sol.token_balance(&ata(&w.maker_sol.pubkey()))?;
    if after != before + SOL_AMOUNT {
        return Err(format!("the maker received {} instead of {SOL_AMOUNT}", after - before));
    }
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

/// Issue a stand-in USDt on Liquid testnet, because the faucet's TEST asset is too scarce for realistic sizes. The issued coin is
/// explicit, and joins the wallet like a faucet coin. Connecting first makes sure this is Liquid testnet.
fn issue_usdt(net: &Net, w: &Wallet, units: u64) -> Res<()> {
    let coins = net.esplora.secret_coins(&w.address(), &w.blinding)?;
    let coin = coins
        .into_iter()
        .filter(|c| c.secrets.asset == lbtc() && c.coin.height.is_some())
        .max_by_key(|c| c.secrets.value)
        .ok_or("the wallet has no confirmed L-BTC")?;
    let (tx, asset) = build_issuance(&coin, w, units, net.genesis)?;
    println!("issued {units} units of {asset} in {}", net.esplora.broadcast(&tx)?);
    Ok(())
}

/// Send the stand-in USDt or L-BTC from a wallet to anyone. Its change, and this payment, are blinded if the address is.
fn send(net: &Net, w: &Wallet, asset: AssetId, amount: u64, to: &str) -> Res<()> {
    let address = Address::from_str(to).map_err(|e| format!("bad Liquid address: {e}"))?;
    let tx = build_confidential_pay(&net.esplora, w, vec![Payment { asset, amount, address }], net.genesis)?;
    println!("{}", net.esplora.broadcast(&tx)?);
    Ok(())
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
            println!("taker Liquid  {}", w.taker_liq.address());
            println!("maker Liquid  {}", w.maker_liq.address());
            println!("taker Solana  {}", w.taker_sol.pubkey());
            println!("maker Solana  {}", w.maker_sol.pubkey());
            println!("mint authority {}", w.mint_authority.pubkey());
        }
        ["faucet"] => {
            // The faucet pays the plain address. Those coins join the wallet, and its first payment blinds their change.
            for wallet in [&w.taker_liq, &w.maker_liq] {
                let address = wallet.address().to_unconfidential().to_string();
                for action in ["lbtc", "test"] {
                    println!("{action} -> {address}: {}", faucet(&address, action)?.chars().take(200).collect::<String>());
                    std::thread::sleep(Duration::from_secs(3));
                }
            }
        }
        ["balances"] => {
            let net = connect()?;
            println!("taker Liquid (USDt, L-BTC) {:?}", liquid_balance(&net.esplora, &w.taker_liq)?);
            println!("maker Liquid (USDt, L-BTC) {:?}", liquid_balance(&net.esplora, &w.maker_liq)?);
            for (name, k) in [("taker", &w.taker_sol), ("maker", &w.maker_sol)] {
                println!("{name} Solana: {} lamports, {} stand-in units", net.sol.lamports(&k.pubkey())?, net.sol.token_balance(&ata(&k.pubkey()))?);
            }
        }
        ["setup", mint_key] => {
            let net = connect()?;
            let mint = read_keypair_file(mint_key).map_err(|e| e.to_string())?;
            net.sol.setup_mint(&w.mint_authority, Some(&mint), &[w.taker_sol.pubkey(), w.maker_sol.pubkey()], 100 * SOL_AMOUNT)?;
            net.sol.top_up_token(&w.mint_authority, &w.maker_sol.pubkey(), 100 * SOL_AMOUNT)?;
            println!("stand-in mint {} ready", solana_escrow::USDT_MINT);
        }
        ["coins", role] => {
            let net = connect()?;
            let wallet = if *role == "maker" { &w.maker_liq } else { &w.taker_liq };
            for c in net.esplora.secret_coins(&wallet.address(), &wallet.blinding)? {
                let hidden = !c.coin.out.asset.is_explicit() && !c.coin.out.value.is_explicit();
                println!("{} {} confirmed at {:?}, hidden {hidden}, {}:{}", c.secrets.value, c.secrets.asset, c.coin.height, c.coin.outpoint.txid, c.coin.outpoint.vout);
            }
        }
        ["mint", owner, units] => {
            let net = connect()?;
            let owner = Pubkey::from_str(owner).map_err(|e| format!("bad Solana address: {e}"))?;
            let units: u64 = units.parse().map_err(|_| "bad amount")?;
            net.sol.top_up_token(&w.mint_authority, &owner, units)?;
            println!("minted {units} stand-in units to {owner}");
        }
        // For CI: a local validator started with the stand-in mint preloaded (scripts/ci-mint-account.py). Liquid isn't
        // touched, so this needs neither Liquid testnet nor the mint's keypair.
        ["ci-setup", faucet_program] => {
            let url = std::env::var("SOLANA_RPC").map_err(|_| "set SOLANA_RPC to the local validator")?;
            if !url.starts_with("http://127.0.0.1") && !url.starts_with("http://localhost") {
                return Err(format!("refusing: ci-setup is for a local validator, not {url}"));
            }
            // Each step waits for `confirmed` only, then everything once for `finalized`, which is what the suites read at.
            let sol = Sol::new_confirmed(&url);
            let holders = [w.taker_sol.pubkey(), w.maker_sol.pubkey()];
            sol.setup_mint(&w.mint_authority, None, &holders, 100 * SOL_AMOUNT)?;
            sol.top_up_token(&w.mint_authority, &w.maker_sol.pubkey(), 100 * SOL_AMOUNT)?;
            let program = Pubkey::from_str(faucet_program).map_err(|e| format!("bad program id: {e}"))?;
            let (vault, _) = Pubkey::find_program_address(&[b"vault"], &program);
            let accounts = vec![
                AccountMeta::new(w.mint_authority.pubkey(), true),
                AccountMeta::new_readonly(solana_escrow::USDT_MINT, false),
                AccountMeta::new(vault, false),
                AccountMeta::new_readonly(anchor_spl::token::ID, false),
                AccountMeta::new_readonly(anchor_lang::system_program::ID, false),
            ];
            let data = solana_sdk::hash::hash(b"global:init").to_bytes()[..8].to_vec();
            sol.send(&[Instruction { program_id: program, accounts, data }], &w.mint_authority, &[])?;
            sol.mint_to(&w.mint_authority, &vault, 100_000 * 1_000_000)?;
            let finalized = Sol::new(&url);
            let start = std::time::Instant::now();
            while finalized.token_balance(&vault).unwrap_or(0) < 100_000 * 1_000_000 {
                if start.elapsed() > std::time::Duration::from_secs(120) {
                    return Err("the setup never finalized".into());
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            println!("local validator funded: stand-in mint, both wallets, faucet vault {vault}");
        }
        ["faucet-init", program] => {
            let net = connect()?;
            let program = Pubkey::from_str(program).map_err(|e| format!("bad program id: {e}"))?;
            let (vault, _) = Pubkey::find_program_address(&[b"vault"], &program);
            let accounts = vec![
                AccountMeta::new(w.mint_authority.pubkey(), true),
                AccountMeta::new_readonly(solana_escrow::USDT_MINT, false),
                AccountMeta::new(vault, false),
                AccountMeta::new_readonly(anchor_spl::token::ID, false),
                AccountMeta::new_readonly(anchor_lang::system_program::ID, false),
            ];
            let data = solana_sdk::hash::hash(b"global:init").to_bytes()[..8].to_vec();
            net.sol.send(&[Instruction { program_id: program, accounts, data }], &w.mint_authority, &[])?;
            println!("faucet vault {vault} ready");
        }
        ["issue-usdt", role, units] => {
            let units: u64 = units.parse().map_err(|_| "bad amount")?;
            issue_usdt(&connect()?, if *role == "maker" { &w.maker_liq } else { &w.taker_liq }, units)?
        }
        ["send", role, asset, amount, to] => {
            let asset = match *asset {
                "usdt" => usdt(),
                "lbtc" => lbtc(),
                other => return Err(format!("unknown asset {other}")),
            };
            let amount: u64 = amount.parse().map_err(|_| "bad amount")?;
            send(&connect()?, if *role == "maker" { &w.maker_liq } else { &w.taker_liq }, asset, amount, to)?
        }
        ["swap", "l2s"] => swap_l2s(&connect()?, &w, false)?,
        ["swap", "s2l"] => swap_s2l(&connect()?, &w, false)?,
        ["abort", "l2s"] => swap_l2s(&connect()?, &w, true)?,
        ["abort", "s2l"] => swap_s2l(&connect()?, &w, true)?,
        _ => return Err("usage: addresses | faucet | balances | coins maker|taker | mint <owner> <units> | issue-usdt maker|taker <units> | send maker|taker usdt|lbtc <units> <address> | setup <mint-keypair> | swap l2s|s2l | abort l2s|s2l".into()),
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

