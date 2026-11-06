//! The taker client's protocol logic. JavaScript handles the page, the network and the wallet, and every decision that
//! touches funds or secrets is made here, in the Rust the maker and CLI already test.
//!
//! Arguments and results are JSON, with amounts as strings. A signing nonce never leaves this module.

// `deny` and not `forbid`, only because the clock's macro below exports a symbol, which counts as unsafe.
#![deny(unsafe_code)]

pub mod solana;

/// The browser's clock, which the Nostr library asks for by a linker symbol on this target.
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
mod clock {
    use std::time::Duration;

    use universal_time::{define_time_provider, Instant, MonotonicClock, SystemTime, WallClock};

    struct Browser;

    fn now() -> Duration {
        Duration::from_millis(js_sys::Date::now() as u64)
    }

    impl WallClock for Browser {
        fn system_time(&self) -> SystemTime {
            SystemTime::from_unix_duration(now())
        }
    }

    impl MonotonicClock for Browser {
        fn instant(&self) -> Instant {
            Instant::from_ticks(now())
        }
    }

    define_time_provider!(Browser);
}

use std::str::FromStr;

use liq2sol_chain::liquid::{
    build_swap_spend, keypair_from, lbtc, random32, read_output, sign_keypath, usdt, use_network, Coin, NetworkConfig,
    SecretCoin, LIQUID_MAINNET_GENESIS,
};
use liq2sol_chain::swap::{answer_claim_with, swap_address, swap_output, taker_fee_address, SignedClaim, TakerClaim};
use liq2sol_rfq::messages::{self as msg, Body, Envelope, MakerTerms, Offer, Quote, Request};
use liq2sol_rfq::rules::{check_envelope, check_quote, check_terms, TakerRules};
use liq2sol_rfq::wrap;
use liq2sol_swapkey::{
    aggregate_bytes, aggregate_from, nonce_bytes, nonce_from, point_of, share_bytes, share_from, solana_point,
    solana_point_of, solana_value, SwapOutput,
};
use nostr::prelude::{Event, Keys, PublicKey as NostrKey, SecretKey as NostrSecret};
use serde::Deserialize;
use serde_json::{json, Value};
use simplicityhl::elements::encode::{deserialize, serialize_hex};
use simplicityhl::elements::secp256k1_zkp::{Keypair, Message, Secp256k1, SecretKey};
use simplicityhl::elements::{Address, AddressParams, AssetId, BlockHash, OutPoint, Sequence, Transaction, TxOut};
use wasm_bindgen::prelude::wasm_bindgen;

type Res<T> = Result<T, String>;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Res<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return Err("bad hex".into());
    }
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string())).collect()
}

fn arr<const N: usize>(s: &str) -> Res<[u8; N]> {
    unhex(s)?.try_into().map_err(|_| format!("expected {N} bytes"))
}

fn parse<T: for<'a> Deserialize<'a>>(text: &str) -> Res<T> {
    serde_json::from_str(text).map_err(|e| format!("bad arguments: {e}"))
}

fn tx(text: &str) -> Res<Transaction> {
    deserialize(&unhex(text)?).map_err(|e| format!("bad transaction: {e}"))
}

fn secret(text: &str) -> Res<SecretKey> {
    SecretKey::from_slice(&arr::<32>(text)?).map_err(|e| e.to_string())
}

fn genesis(text: &str) -> Res<BlockHash> {
    // Nothing here runs on mainnet. The check is here as well as in the page's configuration, which a user can edit.
    if text == LIQUID_MAINNET_GENESIS {
        return Err("refusing to run: this is Liquid mainnet".into());
    }
    BlockHash::from_str(text).map_err(|e| e.to_string())
}

/// Testnet is the built-in default. This selects regtest's assets, and is only used by tests.
#[wasm_bindgen]
pub fn use_regtest(usdt_id: &str, lbtc_id: &str) -> Result<(), String> {
    use_network(NetworkConfig {
        params: &AddressParams::ELEMENTS,
        usdt: AssetId::from_str(usdt_id).map_err(|e| e.to_string())?,
        lbtc: AssetId::from_str(lbtc_id).map_err(|e| e.to_string())?,
    });
    Ok(())
}

// ---- Swap keys ----

/// Fresh secrets for one swap, and what may be told to the maker about them.
#[wasm_bindgen]
pub fn new_swap_secrets() -> Result<String, String> {
    let lock_secret = random32()?;
    let taker_sk = random32()?;
    let blinding_key = random32()?;
    secret(&hex(&lock_secret))?;
    secret(&hex(&blinding_key))?;
    let key = keypair_from(&taker_sk)?;
    Ok(json!({
        "lock_secret": hex(&lock_secret),
        "taker_sk": hex(&taker_sk),
        "blinding_key": hex(&blinding_key),
        "lock_point": hex(&point_of(&lock_secret).map_err(|e| e.to_string())?),
        "lock_point_64": hex(&solana_point(&lock_secret).map_err(|e| e.to_string())?),
        "taker_key": hex(&key.public_key().serialize()),
    })
    .to_string())
}

/// What a claim reveals on Solana for this secret.
#[wasm_bindgen]
pub fn revealed_value(lock_secret: &str) -> Result<String, String> {
    Ok(hex(&solana_value(&arr(lock_secret)?).map_err(|e| e.to_string())?))
}

/// The point as the Solana program stores it, from its compressed form.
#[wasm_bindgen]
pub fn point_64(lock_point: &str) -> Result<String, String> {
    Ok(hex(&solana_point_of(&arr(lock_point)?).map_err(|e| e.to_string())?))
}

#[derive(Deserialize)]
struct SwapArgs {
    direction: String,
    maker_key: String,
    taker_key: String,
    blinding_key: String,
    lock_height: u32,
}

impl SwapArgs {
    fn output(&self) -> Res<SwapOutput> {
        let taker_funds = match self.direction.as_str() {
            "L2S" => true,
            "S2L" => false,
            _ => return Err("an unknown direction".into()),
        };
        swap_output(&arr(&self.maker_key)?, &arr(&self.taker_key)?, taker_funds, self.lock_height)
    }
}

/// The addresses of a swap, derived here and never taken from the maker.
#[wasm_bindgen]
pub fn swap_info(args: &str) -> Result<String, String> {
    let a: SwapArgs = parse(args)?;
    let output = a.output()?;
    let blinding = secret(&a.blinding_key)?;
    let address = swap_address(&output, &blinding);
    let fee = taker_fee_address(&arr(&a.taker_key)?, &blinding)?;
    Ok(json!({
        "address": address.to_string(),
        "plain_address": address.to_unconfidential().to_string(),
        "script": hex(address.script_pubkey().as_bytes()),
        "fee_address": fee.to_string(),
        "plain_fee_address": fee.to_unconfidential().to_string(),
        "fee_script": hex(fee.script_pubkey().as_bytes()),
        "cmr": hex(&output.cmr()),
    })
    .to_string())
}

// ---- Reading coins ----

/// Each output of this transaction paying this script, as read with this key. Nothing on-chain checks the asset or amount,
/// so `usdt` and `amount` here are what the client relies on. A label in a range proof that doesn't open the asset
/// commitment isn't read at all.
#[wasm_bindgen]
pub fn read_coins(tx_hex: &str, script: &str, blinding_key: &str) -> Result<String, String> {
    let t = tx(tx_hex)?;
    let spk = unhex(script)?;
    let key = secret(blinding_key)?;
    let found: Vec<Value> = t
        .output
        .iter()
        .enumerate()
        .filter(|(_, o)| o.script_pubkey.as_bytes() == spk.as_slice())
        .filter_map(|(vout, o)| {
            read_output(o, &key).map(|s| {
                json!({
                    "vout": vout,
                    "amount": s.value.to_string(),
                    "is_usdt": s.asset == usdt(),
                    "is_lbtc": s.asset == lbtc(),
                    "hidden": !o.asset.is_explicit() && !o.value.is_explicit(),
                })
            })
        })
        .collect();
    Ok(Value::Array(found).to_string())
}

/// Which parent transactions a transaction spends, in input order.
#[wasm_bindgen]
pub fn parents_of(tx_hex: &str) -> Result<String, String> {
    let t = tx(tx_hex)?;
    let list: Vec<String> = t.input.iter().map(|i| i.previous_output.txid.to_string()).collect();
    Ok(json!(list).to_string())
}

/// The outputs spent by a transaction, from its parents in input order. The parents' ids are checked, so a lying API can't
/// change what gets signed.
fn spent_from(t: &Transaction, parents: &[String]) -> Res<Vec<TxOut>> {
    if parents.len() != t.input.len() {
        return Err("one parent transaction is needed per input".into());
    }
    t.input
        .iter()
        .zip(parents)
        .map(|(i, p)| {
            let parent = tx(p)?;
            if parent.txid() != i.previous_output.txid {
                return Err("a parent transaction doesn't match its input".to_string());
            }
            parent
                .output
                .get(i.previous_output.vout as usize)
                .cloned()
                .ok_or("an input's output is missing".to_string())
        })
        .collect()
}

fn secret_coin(tx_hex: &str, vout: u32, key: &SecretKey) -> Res<SecretCoin> {
    let t = tx(tx_hex)?;
    let out = t.output.get(vout as usize).ok_or("no such output")?.clone();
    let secrets = read_output(&out, key).ok_or("this output can't be read")?;
    Ok(SecretCoin { coin: Coin { outpoint: OutPoint::new(t.txid(), vout), out, height: None }, secrets })
}

// ---- L to S: signing the maker's claim ----

#[derive(Deserialize)]
struct OfferArgs {
    swap: SwapArgs,
    taker_sk: String,
    lock_point: String,
    coin_tx: String,
    coin_vout: u32,
    claim_tx: String,
    maker_nonce: String,
    parents: Vec<String>,
    genesis: String,
}

/// Check the maker's claim of our coin, and sign it, locked to our point. The maker can't use this until we reveal the secret
/// on Solana, which pays us first. The nonce is made and used in this one call.
#[wasm_bindgen]
pub fn answer_claim_offer(args: &str) -> Result<String, String> {
    let a: OfferArgs = parse(args)?;
    let claim = tx(&a.claim_tx)?;
    let coin = OutPoint::new(tx(&a.coin_tx)?.txid(), a.coin_vout);
    let utxos = spent_from(&claim, &a.parents)?;
    let (nonce, share) = answer_claim_with(
        &keypair_from(&arr(&a.taker_sk)?)?,
        &a.swap.output()?,
        &coin,
        &arr(&a.lock_point)?,
        &nonce_from(&unhex(&a.maker_nonce)?).map_err(|e| e.to_string())?,
        &claim,
        &utxos,
        genesis(&a.genesis)?,
    )?;
    Ok(json!({ "nonce": hex(&nonce_bytes(&nonce)), "share": hex(&share_bytes(&share)) }).to_string())
}

// ---- S to L: our claim of the maker's coin ----

#[derive(Deserialize)]
struct ClaimArgs {
    swap: SwapArgs,
    taker_sk: String,
    lock_point: String,
    coin_tx: String,
    coin_vout: u32,
    fee_tx: String,
    fee_vout: u32,
    to_address: String,
    genesis: String,
}

enum Stage {
    Started(TakerClaim),
    Signed(SignedClaim),
    Empty,
}

/// One signing session. Its nonce is held here and used once, and a page reload simply abandons the session.
#[wasm_bindgen]
pub struct ClaimSession {
    stage: Stage,
    key: Keypair,
    output: SwapOutput,
    point: [u8; 33],
    tx: String,
    nonce: String,
}

#[wasm_bindgen]
impl ClaimSession {
    #[wasm_bindgen(constructor)]
    pub fn new(args: &str) -> Result<ClaimSession, String> {
        let a: ClaimArgs = parse(args)?;
        let blinding = secret(&a.swap.blinding_key)?;
        let output = a.swap.output()?;
        let key = keypair_from(&arr(&a.taker_sk)?)?;
        let genesis = genesis(&a.genesis)?;
        let coin = secret_coin(&a.coin_tx, a.coin_vout, &blinding)?;
        let fee = secret_coin(&a.fee_tx, a.fee_vout, &blinding)?;
        let to = Address::from_str(&a.to_address).map_err(|e| format!("bad Liquid address: {e}"))?;
        let (claim, utxos) = build_swap_spend(&coin, &fee, &to, 0, Sequence::MAX)?;
        let tx_hex = serialize_hex(&claim);
        let started = TakerClaim::start_with(&key, &output, claim, utxos, genesis)?;
        let nonce = hex(&nonce_bytes(&started.nonce()));
        Ok(ClaimSession {
            stage: Stage::Started(started),
            key,
            output,
            point: arr(&a.lock_point)?,
            tx: tx_hex,
            nonce,
        })
    }

    pub fn tx(&self) -> String {
        self.tx.clone()
    }

    pub fn nonce(&self) -> String {
        self.nonce.clone()
    }

    /// Our share, given the maker's nonce. The session can't sign anything else after this.
    pub fn sign(&mut self, maker_nonce: &str) -> Result<String, String> {
        let Stage::Started(claim) = std::mem::replace(&mut self.stage, Stage::Empty) else {
            return Err("this session has already signed".into());
        };
        let nonce = nonce_from(&unhex(maker_nonce)?).map_err(|e| e.to_string())?;
        let (share, signed) = claim.sign(&self.key, &self.output, &self.point, &nonce)?;
        self.stage = Stage::Signed(signed);
        Ok(hex(&share_bytes(&share)))
    }

    /// What to keep while waiting for the maker's share. The nonce is used up by now, so this can't sign anything else.
    pub fn signed_state(&self) -> Result<String, String> {
        let Stage::Signed(signed) = &self.stage else {
            return Err("this session hasn't signed".into());
        };
        Ok(json!({
            "tx": serialize_hex(&signed.tx),
            "message": hex(&signed.message),
            "aggregate": hex(&aggregate_bytes(&signed.aggregate)),
            "share": hex(&share_bytes(&signed.share)),
            "maker_nonce": hex(&nonce_bytes(&signed.maker_nonce)),
        })
        .to_string())
    }
}

#[derive(Deserialize)]
struct FinishArgs {
    swap: SwapArgs,
    taker_sk: String,
    lock_point: String,
    lock_secret: String,
    signed: SignedState,
    parents: Vec<String>,
    maker_share: String,
    genesis: String,
}

#[derive(Deserialize)]
struct SignedState {
    tx: String,
    message: String,
    aggregate: String,
    share: String,
    maker_nonce: String,
}

/// The finished claim, once the maker's share checks out. **Publishing it reveals the secret to the maker.**
#[wasm_bindgen]
pub fn finish_claim(args: &str) -> Result<String, String> {
    let a: FinishArgs = parse(args)?;
    let t = tx(&a.signed.tx)?;
    let utxos = spent_from(&t, &a.parents)?;
    let signed = SignedClaim {
        tx: t,
        utxos,
        message: arr(&a.signed.message)?,
        aggregate: aggregate_from(&unhex(&a.signed.aggregate)?).map_err(|e| e.to_string())?,
        share: share_from(&unhex(&a.signed.share)?).map_err(|e| e.to_string())?,
        maker_nonce: nonce_from(&unhex(&a.signed.maker_nonce)?).map_err(|e| e.to_string())?,
    };
    let done = signed.finish(
        &keypair_from(&arr(&a.taker_sk)?)?,
        &a.swap.output()?,
        &arr(&a.swap.maker_key)?,
        share_from(&unhex(&a.maker_share)?).map_err(|e| e.to_string())?,
        &arr(&a.lock_point)?,
        &arr(&a.lock_secret)?,
        genesis(&a.genesis)?,
    )?;
    Ok(serialize_hex(&done))
}

// ---- L to S: our refund ----

#[derive(Deserialize)]
struct RefundArgs {
    swap: SwapArgs,
    taker_sk: String,
    coin_tx: String,
    coin_vout: u32,
    fee_tx: String,
    fee_vout: u32,
    to_address: String,
    genesis: String,
}

/// Take back a coin we funded, through the refund leaf. Any coin at the address can come back this way, whatever it holds.
#[wasm_bindgen]
pub fn build_refund(args: &str) -> Result<String, String> {
    let a: RefundArgs = parse(args)?;
    let blinding = secret(&a.swap.blinding_key)?;
    let output = a.swap.output()?;
    let key = keypair_from(&arr(&a.taker_sk)?)?;
    let genesis = genesis(&a.genesis)?;
    let coin = secret_coin(&a.coin_tx, a.coin_vout, &blinding)?;
    let fee = secret_coin(&a.fee_tx, a.fee_vout, &blinding)?;
    let to = Address::from_str(&a.to_address).map_err(|e| format!("bad Liquid address: {e}"))?;
    let (t, utxos) = build_swap_spend(&coin, &fee, &to, a.swap.lock_height, Sequence::ZERO)?;
    let digest = output.refund_sighash(&t, &utxos, 0, genesis).map_err(|e| e.to_string())?;
    let signature = *Secp256k1::new().sign_schnorr_no_aux_rand(&Message::from_digest(digest), &key).as_ref();
    let mut t = output.finalize_refund(t, &utxos, 0, genesis, signature).map_err(|e| e.to_string())?;
    sign_keypath(&mut t, &utxos, 1, &key, genesis)?;
    Ok(serialize_hex(&t))
}

// ---- Nostr ----

fn keys(sk: &str) -> Res<Keys> {
    Ok(Keys::new(NostrSecret::from_hex(sk).map_err(|e| e.to_string())?))
}

fn pubkey(text: &str) -> Res<NostrKey> {
    NostrKey::from_hex(text).map_err(|e| e.to_string())
}

/// A throwaway identity. A taker has no reputation to keep, and a new key per swap links nothing.
#[wasm_bindgen]
pub fn new_nostr_key() -> Result<String, String> {
    let k = Keys::generate();
    Ok(json!({ "secret": k.secret_key().to_secret_hex(), "public": k.public_key().to_hex() }).to_string())
}

#[wasm_bindgen]
pub fn wrap_message(sk: &str, to: &str, id: &str, network: &str, body: &str, now: u64) -> Result<String, String> {
    let body: Body = parse(body)?;
    let envelope = Envelope { v: msg::VERSION, id: id.to_string(), network: network.to_string(), sent_at: now, body };
    let (event, rumor) = wrap::wrap(&keys(sk)?, &pubkey(to)?, &envelope, now).map_err(|e| format!("{e:?}"))?;
    Ok(json!({ "event": serde_json::from_str::<Value>(&event.as_json()).map_err(|e| e.to_string())?, "rumor_id": rumor })
        .to_string())
}

/// Open a message, checking every layer, the network and its age. `null` for anything that fails: anyone can send us anything.
#[wasm_bindgen]
pub fn unwrap_message(sk: &str, event: &str, network: &str, now: u64) -> Result<String, String> {
    let k = keys(sk)?;
    let Ok(event) = Event::from_json(event) else { return Ok("null".into()) };
    let Ok(got) = wrap::unwrap(&k, &event) else { return Ok("null".into()) };
    if check_envelope(&got.envelope, network, now).is_err() {
        return Ok("null".into());
    }
    Ok(json!({
        "sender": got.sender.to_hex(),
        "rumor_id": got.rumor_id,
        "id": got.envelope.id,
        "body": serde_json::to_value(&got.envelope.body).map_err(|e| e.to_string())?,
    })
    .to_string())
}

/// A maker's offer, with its author, or `null` if it is unsigned, expired or for another network.
#[wasm_bindgen]
pub fn read_offer(event: &str, network: &str, now: u64) -> Result<String, String> {
    let Ok(event) = Event::from_json(event) else { return Ok("null".into()) };
    let Some(offer) = wrap::read_offer(&event, network, now) else { return Ok("null".into()) };
    let offer: Offer = offer;
    Ok(json!({ "maker": event.pubkey.to_hex(), "created_at": event.created_at.as_secs(), "offer": offer }).to_string())
}

#[derive(Deserialize)]
struct RulesArgs {
    network: String,
    program_id: String,
    usdt_asset: String,
    l2s_min_blocks: u32,
    l2s_max_blocks: u32,
    s2l_min_blocks: u32,
    s2l_max_blocks: u32,
    max_spread_bps: u32,
    max_deadline_slots: u64,
}

impl RulesArgs {
    fn rules(self) -> TakerRules {
        TakerRules {
            network: self.network,
            program_id: self.program_id,
            usdt_asset: self.usdt_asset,
            template_version: msg::TEMPLATE_VERSION,
            l2s_min_blocks: self.l2s_min_blocks,
            l2s_max_blocks: self.l2s_max_blocks,
            s2l_min_blocks: self.s2l_min_blocks,
            s2l_max_blocks: self.s2l_max_blocks,
            max_spread_bps: self.max_spread_bps,
            max_deadline_slots: self.max_deadline_slots,
        }
    }
}

/// `""` if the quote is acceptable, or why it isn't.
#[wasm_bindgen]
pub fn quote_problem(rules: &str, request: &str, quote: &str, now: u64) -> Result<String, String> {
    let r: RulesArgs = parse(rules)?;
    let (request, quote): (Request, Quote) = (parse(request)?, parse(quote)?);
    Ok(check_quote(&r.rules(), &request, &quote, now).err().map_or(String::new(), |e| format!("{e:?}")))
}

/// `""` if the terms are acceptable, or why they aren't. This is the last check before funding, with `tip` from our own chain view.
#[wasm_bindgen]
pub fn terms_problem(
    rules: &str,
    direction: &str,
    quote: &str,
    accept_rumor: &str,
    terms: &str,
    tip: u32,
) -> Result<String, String> {
    let r: RulesArgs = parse(rules)?;
    let (quote, terms): (Quote, MakerTerms) = (parse(quote)?, parse(terms)?);
    let d = match direction {
        "L2S" => msg::Direction::L2S,
        "S2L" => msg::Direction::S2L,
        _ => return Err("an unknown direction".into()),
    };
    Ok(check_terms(&r.rules(), d, &quote, accept_rumor, &terms, tip).err().map_or(String::new(), |e| format!("{e:?}")))
}

// ---- Solana ----

#[wasm_bindgen]
pub fn escrow_address(initiator: &str, lock_point_64: &str, program: &str) -> Result<String, String> {
    let (i, p) = (solana::key(initiator)?, solana::key(program)?);
    Ok(solana::text(&solana::escrow_pda(&i, &arr(lock_point_64)?, &p)))
}

#[wasm_bindgen]
pub fn token_account(owner: &str, mint: &str) -> Result<String, String> {
    Ok(solana::text(&solana::ata(&solana::key(owner)?, &solana::key(mint)?)))
}

#[wasm_bindgen]
pub fn parse_escrow(data_hex: &str) -> Result<String, String> {
    serde_json::to_string(&solana::parse_escrow(&unhex(data_hex)?)?).map_err(|e| e.to_string())
}

const LOCK_UNITS: u32 = 120_000;
const CLAIM_UNITS: u32 = 100_000;
const REFUND_UNITS: u32 = 60_000;

fn finish_tx(payer: &solana::Key, mut ixs: Vec<solana::Ix>, units: u32, price: u64, blockhash: &str) -> Res<String> {
    let mut all = solana::budget_ixs(units, price);
    all.append(&mut ixs);
    let message = solana::message(payer, &all, &solana::key(blockhash)?);
    Ok(hex(&solana::unsigned_tx(&message)))
}

/// S→L: lock the taker's funds. Always a long-leg escrow, so `close_on_restart` is off.
#[allow(clippy::too_many_arguments)]
#[wasm_bindgen]
pub fn build_lock_tx(
    program: &str,
    mint: &str,
    initiator: &str,
    lock_point_64: &str,
    recipient: &str,
    amount: &str,
    deadline_slot: u64,
    blockhash: &str,
    micro_lamports: u64,
) -> Result<String, String> {
    let payer = solana::key(initiator)?;
    let amount: u64 = amount.parse().map_err(|_| "bad amount")?;
    let ix = solana::lock_ix(
        &solana::key(program)?,
        &solana::key(mint)?,
        &payer,
        &arr(lock_point_64)?,
        &solana::key(recipient)?,
        amount,
        deadline_slot,
        false,
    );
    finish_tx(&payer, vec![ix], LOCK_UNITS, micro_lamports, blockhash)
}

/// L→S: claim the maker's escrow. **This carries the secret's value.** The caller simulates first, and sends only after that.
#[allow(clippy::too_many_arguments)]
#[wasm_bindgen]
pub fn build_claim_tx(
    program: &str,
    mint: &str,
    payer: &str,
    escrow: &str,
    recipient: &str,
    rent_payer: &str,
    revealed: &str,
    blockhash: &str,
    micro_lamports: u64,
) -> Result<String, String> {
    let (p, m, pay, r) = (solana::key(program)?, solana::key(mint)?, solana::key(payer)?, solana::key(recipient)?);
    let ixs = vec![
        solana::create_ata_ix(&pay, &r, &m),
        solana::claim_ix(&p, &m, &solana::key(escrow)?, &r, &solana::key(rent_payer)?, &arr(revealed)?),
    ];
    finish_tx(&pay, ixs, CLAIM_UNITS, micro_lamports, blockhash)
}

/// S→L: take back our funds after the deadline. Anyone may send this, and it can only pay the initiator.
#[allow(clippy::too_many_arguments)]
#[wasm_bindgen]
pub fn build_refund_tx(
    program: &str,
    payer: &str,
    escrow: &str,
    refund_token: &str,
    rent_payer: &str,
    blockhash: &str,
    micro_lamports: u64,
) -> Result<String, String> {
    let pay = solana::key(payer)?;
    let ix = solana::refund_ix(
        &solana::key(program)?,
        &solana::key(escrow)?,
        &solana::key(refund_token)?,
        &solana::key(rent_payer)?,
    );
    finish_tx(&pay, vec![ix], REFUND_UNITS, micro_lamports, blockhash)
}
