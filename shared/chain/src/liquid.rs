//! Liquid side: an Esplora client and a small key-path wallet holding explicit coins only.

use std::str::FromStr;

use liq2sol_htlc::{Htlc, HtlcParams, Spend};
use serde_json::Value as Json;
use simplicityhl::elements::confidential::{Asset, Nonce, Value};
use simplicityhl::elements::encode::{deserialize, serialize_hex};
use simplicityhl::elements::schnorr::{TapTweak, UntweakedKeypair};
use simplicityhl::elements::secp256k1_zkp::{Keypair, Message, Secp256k1};
use simplicityhl::elements::sighash::{Prevouts, SighashCache};
use simplicityhl::elements::{
    Address, AddressParams, AssetId, AssetIssuance, BlockHash, LockTime, OutPoint,
    SchnorrSighashType, Script, Sequence, Transaction, TxIn, TxInWitness, TxOut, TxOutWitness,
    Txid, Witness,
};

/// The network this process works on, chosen once before any address is derived.
pub struct NetworkConfig {
    pub params: &'static AddressParams,
    pub usdt: AssetId,
    pub lbtc: AssetId,
}

static NETWORK: std::sync::OnceLock<NetworkConfig> = std::sync::OnceLock::new();

/// Use regtest assets instead of the Liquid testnet defaults. Only the test harness does this.
pub fn use_network(config: NetworkConfig) {
    let _ = NETWORK.set(config);
}

pub fn network() -> &'static NetworkConfig {
    NETWORK.get_or_init(|| NetworkConfig {
        params: &AddressParams::LIQUID_TESTNET,
        usdt: AssetId::from_str(TEST_ASSET).expect("constant"),
        lbtc: AssetId::from_str(TEST_LBTC).expect("constant"),
    })
}

pub const LIQUID_TESTNET_GENESIS: &str =
    "a771da8e52ee6ad581ed1e9a99825e5b3b7992225534eaa2ae23244fe26ab1c1";
/// Liquid mainnet. Nothing in this project may run against it.
pub const LIQUID_MAINNET_GENESIS: &str =
    "1466275836220db2944ca059a3a10ef6fd2ea684b0688d2c379296888a206003";
/// The faucet's registered test asset, standing in for L-USDt.
pub const TEST_ASSET: &str = "38fca2d939696061a8f76d4e6b5eecd54e3b4221c846f24a6b279e79952850a5";
/// L-BTC on Liquid testnet.
pub const TEST_LBTC: &str = "144c654344aa716d6f3abcc1ca90e5641e4e2a7f633bc09fe3baf64585819a49";
/// Whole-coin fee used for every spend, per docs/contracts.md.
pub const FEE: u64 = 300;

pub type Res<T> = Result<T, String>;

pub fn usdt() -> AssetId {
    network().usdt
}

pub fn lbtc() -> AssetId {
    network().lbtc
}

fn get(url: &str) -> Res<String> {
    let mut last = String::new();
    for _ in 0..5 {
        match ureq::get(url)
            .timeout(std::time::Duration::from_secs(20))
            .call()
        {
            Ok(r) => return r.into_string().map_err(|e| e.to_string()),
            Err(e) => last = format!("GET {url}: {e}"),
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    Err(last)
}

fn unhex(s: &str) -> Res<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err("odd hex".into());
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

/// A local node driven through `elements-cli`, for regtest.
#[derive(Clone)]
pub struct Rpc {
    pub cli: String,
    pub args: Vec<String>,
}

impl Rpc {
    pub fn call(&self, args: &[&str]) -> Res<String> {
        let out = std::process::Command::new(&self.cli)
            .args(&self.args)
            .args(args)
            .output()
            .map_err(|e| format!("run {}: {e}", self.cli))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(format!(
                "{:?}: {}",
                args.first(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn json(&self, args: &[&str]) -> Res<Json> {
        serde_json::from_str(&self.call(args)?).map_err(|e| e.to_string())
    }
}

/// Where chain data comes from: a public Esplora API, or a local node.
#[derive(Clone)]
pub enum Esplora {
    Http(String),
    Node(Rpc),
}

/// A confirmed or unconfirmed explicit coin.
#[derive(Clone, Debug)]
pub struct Coin {
    pub outpoint: OutPoint,
    pub out: TxOut,
    pub height: Option<u32>,
}

impl Esplora {
    pub fn tip(&self) -> Res<u32> {
        match self {
            Esplora::Http(u) => get(&format!("{u}/blocks/tip/height"))?
                .trim()
                .parse()
                .map_err(|e| format!("tip: {e}")),
            Esplora::Node(n) => n
                .call(&["getblockcount"])?
                .parse()
                .map_err(|e| format!("tip: {e}")),
        }
    }

    /// The tip block's own timestamp, used to tell how long the chain has been stalled.
    pub fn tip_time(&self) -> Res<u64> {
        match self {
            Esplora::Http(u) => {
                let hash = get(&format!("{u}/blocks/tip/hash"))?;
                let block: Json =
                    serde_json::from_str(&get(&format!("{u}/block/{}", hash.trim()))?)
                        .map_err(|e| e.to_string())?;
                block["timestamp"]
                    .as_u64()
                    .ok_or_else(|| "block timestamp".to_string())
            }
            Esplora::Node(n) => {
                let hash = n.call(&["getbestblockhash"])?;
                n.json(&["getblockheader", &hash])?["time"]
                    .as_u64()
                    .ok_or_else(|| "block time".to_string())
            }
        }
    }

    /// The block hash at a height, to notice the chain changing underneath us.
    pub fn block_hash(&self, height: u32) -> Res<String> {
        match self {
            Esplora::Http(u) => Ok(get(&format!("{u}/block-height/{height}"))?.trim().to_string()),
            Esplora::Node(n) => n.call(&["getblockhash", &height.to_string()]),
        }
    }

    /// Whether a transaction is in a block, not just the mempool.
    pub fn is_confirmed(&self, txid: &Txid) -> Res<bool> {
        match self {
            Esplora::Http(u) => {
                let status: Json = serde_json::from_str(&get(&format!("{u}/tx/{txid}/status"))?).map_err(|e| e.to_string())?;
                Ok(status["confirmed"].as_bool() == Some(true))
            }
            Esplora::Node(n) => {
                let tx = n.json(&["getrawtransaction", &txid.to_string(), "1"])?;
                Ok(tx["confirmations"].as_u64().unwrap_or(0) > 0)
            }
        }
    }

    pub fn genesis(&self) -> Res<String> {
        match self {
            Esplora::Http(u) => Ok(get(&format!("{u}/block-height/0"))?.trim().to_string()),
            Esplora::Node(n) => n.call(&["getblockhash", "0"]),
        }
    }

    pub fn tx(&self, txid: &Txid) -> Res<Transaction> {
        let hex = match self {
            Esplora::Http(u) => get(&format!("{u}/tx/{txid}/hex"))?,
            Esplora::Node(n) => n.call(&["getrawtransaction", &txid.to_string()])?,
        };
        deserialize(&unhex(hex.trim())?).map_err(|e| format!("decode tx: {e}"))
    }

    pub fn broadcast(&self, tx: &Transaction) -> Res<Txid> {
        match self {
            Esplora::Http(u) => {
                let url = format!("{u}/tx");
                match ureq::post(&url)
                    .timeout(std::time::Duration::from_secs(30))
                    .send_string(&serialize_hex(tx))
                {
                    Ok(r) => Txid::from_str(r.into_string().map_err(|e| e.to_string())?.trim())
                        .map_err(|e| e.to_string()),
                    Err(ureq::Error::Status(code, r)) => Err(format!(
                        "broadcast {code}: {}",
                        r.into_string().unwrap_or_default()
                    )),
                    Err(e) => Err(format!("broadcast: {e}")),
                }
            }
            Esplora::Node(n) => {
                Txid::from_str(&n.call(&["sendrawtransaction", &serialize_hex(tx)])?)
                    .map_err(|e| e.to_string())
            }
        }
    }

    /// (txid, vout, confirmed height) of each unspent output at an address.
    fn unspent(&self, address: &Address) -> Res<Vec<(Txid, u32, Option<u32>)>> {
        let parse =
            |t: &Json| Txid::from_str(t.as_str().unwrap_or_default()).map_err(|e| e.to_string());
        match self {
            Esplora::Http(u) => {
                let body = get(&format!("{u}/address/{address}/utxo"))?;
                let list: Vec<Json> = serde_json::from_str(&body).map_err(|e| e.to_string())?;
                list.iter()
                    .map(|c| {
                        let height = c["status"]["block_height"].as_u64().map(|h| h as u32);
                        Ok((
                            parse(&c["txid"])?,
                            c["vout"].as_u64().ok_or("utxo vout")? as u32,
                            height,
                        ))
                    })
                    .collect()
            }
            Esplora::Node(n) => {
                let desc = format!("[\"addr({address})\"]");
                let found = n.json(&["scantxoutset", "start", &desc])?;
                found["unspents"]
                    .as_array()
                    .ok_or("scantxoutset result")?
                    .iter()
                    .map(|c| {
                        let height = c["height"].as_u64().map(|h| h as u32);
                        Ok((
                            parse(&c["txid"])?,
                            c["vout"].as_u64().ok_or("utxo vout")? as u32,
                            height,
                        ))
                    })
                    .collect()
            }
        }
    }

    /// All unspent explicit coins at an address, read from the raw transactions rather than an API's summary.
    pub fn coins(&self, address: &Address) -> Res<Vec<Coin>> {
        let mut coins = vec![];
        for (txid, vout, height) in self.unspent(address)? {
            let tx = self.tx(&txid)?;
            let out = tx
                .output
                .get(vout as usize)
                .ok_or("utxo vout range")?
                .clone();
            if out.script_pubkey != address.script_pubkey()
                || !out.asset.is_explicit()
                || !out.value.is_explicit()
            {
                continue;
            }
            coins.push(Coin {
                outpoint: OutPoint::new(txid, vout),
                out,
                height,
            });
        }
        Ok(coins)
    }

    /// The transaction spending an outpoint, if any, mempool included.
    pub fn spender(&self, outpoint: &OutPoint) -> Res<Option<Transaction>> {
        match self {
            Esplora::Http(u) => {
                let body = get(&format!(
                    "{u}/tx/{}/outspend/{}",
                    outpoint.txid, outpoint.vout
                ))?;
                let status: Json = serde_json::from_str(&body).map_err(|e| e.to_string())?;
                match status["txid"]
                    .as_str()
                    .filter(|_| status["spent"].as_bool() == Some(true))
                {
                    Some(txid) => Ok(Some(
                        self.tx(&Txid::from_str(txid).map_err(|e| e.to_string())?)?,
                    )),
                    None => Ok(None),
                }
            }
            Esplora::Node(n) => {
                // Regtest chains here are short, so scanning the mempool and all blocks is fine.
                let spends =
                    |tx: &Transaction| tx.input.iter().any(|i| i.previous_output == *outpoint);
                for id in n.json(&["getrawmempool"])?.as_array().ok_or("mempool")? {
                    let tx = self.tx(&Txid::from_str(id.as_str().unwrap_or_default())
                        .map_err(|e| e.to_string())?)?;
                    if spends(&tx) {
                        return Ok(Some(tx));
                    }
                }
                for height in (1..=self.tip()?).rev() {
                    let hash = n.call(&["getblockhash", &height.to_string()])?;
                    let block = n.json(&["getblock", &hash])?;
                    for id in block["tx"].as_array().ok_or("block txs")? {
                        let tx = self.tx(&Txid::from_str(id.as_str().unwrap_or_default())
                            .map_err(|e| e.to_string())?)?;
                        if spends(&tx) {
                            return Ok(Some(tx));
                        }
                    }
                }
                Ok(None)
            }
        }
    }
}

/// Exactly `amount` of `asset`, explicit, at a confirmed depth of `confs`.
pub fn exact_coin(
    coins: &[Coin],
    asset: AssetId,
    amount: u64,
    tip: u32,
    confs: u32,
) -> Option<Coin> {
    coins
        .iter()
        .find(|c| {
            c.out.asset == Asset::Explicit(asset)
                && c.out.value == Value::Explicit(amount)
                && c.height.is_some_and(|h| tip + 1 >= h + confs)
        })
        .cloned()
}

pub fn txout(asset: AssetId, amount: u64, script: Script) -> TxOut {
    TxOut {
        asset: Asset::Explicit(asset),
        value: Value::Explicit(amount),
        nonce: Nonce::Null,
        script_pubkey: script,
        witness: TxOutWitness::default(),
    }
}

fn input(previous_output: OutPoint, sequence: Sequence) -> TxIn {
    TxIn {
        previous_output,
        is_pegin: false,
        script_sig: Script::new(),
        sequence,
        asset_issuance: AssetIssuance::default(),
        witness: TxInWitness::default(),
    }
}

pub fn keypair_from(seed: &[u8; 32]) -> Res<Keypair> {
    Keypair::from_seckey_slice(&Secp256k1::new(), seed).map_err(|e| e.to_string())
}

pub fn random32() -> Res<[u8; 32]> {
    let mut b = [0u8; 32];
    getrandom::getrandom(&mut b).map_err(|e| e.to_string())?;
    Ok(b)
}

pub fn address_of(key: &Keypair) -> Address {
    Address::p2tr(
        &Secp256k1::new(),
        key.x_only_public_key().0,
        None,
        None,
        network().params,
    )
}

fn sign_keypath(
    tx: &mut Transaction,
    utxos: &[TxOut],
    index: usize,
    key: &Keypair,
    genesis: BlockHash,
) -> Res<()> {
    let secp = Secp256k1::new();
    let hash = SighashCache::new(&*tx)
        .taproot_key_spend_signature_hash(
            index,
            &Prevouts::All(utxos),
            SchnorrSighashType::All,
            genesis,
        )
        .map_err(|e| e.to_string())?;
    let tweaked = UntweakedKeypair::tap_tweak(*key, &secp, None);
    let sig = secp.sign_schnorr_no_aux_rand(
        &Message::from_digest(hash.to_byte_array()),
        &tweaked.to_inner(),
    );
    let mut bytes = sig.as_ref().to_vec();
    bytes.push(SchnorrSighashType::All as u8);
    tx.input[index].witness.script_witness = Witness::from_slice(&[bytes]);
    Ok(())
}

/// Pay exact explicit outputs from a key-path wallet and broadcast.
pub fn pay(esplora: &Esplora, key: &Keypair, outputs: Vec<TxOut>, genesis: BlockHash) -> Res<Txid> {
    esplora.broadcast(&build_pay(esplora, key, outputs, genesis)?)
}

/// A signed payment of exact explicit outputs, returning change to the wallet and using `FEE` sats of L-BTC as the fee.
pub fn build_pay(
    esplora: &Esplora,
    key: &Keypair,
    outputs: Vec<TxOut>,
    genesis: BlockHash,
) -> Res<Transaction> {
    let address = address_of(key);
    let coins = esplora.coins(&address)?;
    let (mut need_usdt, mut need_lbtc) = (0u64, FEE);
    for o in &outputs {
        match (o.asset, o.value) {
            (Asset::Explicit(a), Value::Explicit(v)) if a == usdt() => need_usdt += v,
            (Asset::Explicit(a), Value::Explicit(v)) if a == lbtc() => need_lbtc += v,
            _ => return Err("only explicit stand-in USDt and L-BTC outputs".into()),
        }
    }
    let pick = |asset: AssetId, need: u64| -> Res<(Vec<Coin>, u64)> {
        let mut total = 0;
        let mut chosen = vec![];
        for c in coins
            .iter()
            .filter(|c| c.out.asset == Asset::Explicit(asset))
        {
            if total >= need {
                break;
            }
            total += c.out.value.explicit().unwrap_or(0);
            chosen.push(c.clone());
        }
        if total < need {
            return Err(format!(
                "wallet {address} has {total} of {asset}, needs {need}. Fund it from the faucet."
            ));
        }
        Ok((chosen, total - need))
    };
    let (usdt_coins, usdt_change) = if need_usdt > 0 {
        pick(usdt(), need_usdt)?
    } else {
        (vec![], 0)
    };
    let (lbtc_coins, lbtc_change) = pick(lbtc(), need_lbtc)?;

    let chosen: Vec<Coin> = usdt_coins.into_iter().chain(lbtc_coins).collect();
    let utxos: Vec<TxOut> = chosen.iter().map(|c| c.out.clone()).collect();
    let mut output = outputs;
    if usdt_change > 0 {
        output.push(txout(usdt(), usdt_change, address.script_pubkey()));
    }
    if lbtc_change > 0 {
        output.push(txout(lbtc(), lbtc_change, address.script_pubkey()));
    }
    output.push(TxOut::new_fee(FEE, lbtc()));
    let mut tx = Transaction {
        version: 2,
        lock_time: LockTime::ZERO,
        input: chosen
            .iter()
            .map(|c| input(c.outpoint, Sequence::MAX))
            .collect(),
        output,
    };
    for i in 0..tx.input.len() {
        sign_keypath(&mut tx, &utxos, i, key, genesis)?;
    }
    Ok(tx)
}

/// What the HTLC coin is being spent for, and who signs.
pub enum HtlcSpend {
    Claim {
        preimage: [u8; 32],
        recipient: Keypair,
        to: Script,
    },
    Refund {
        sender: Keypair,
        lock_height: u32,
        to: Script,
    },
}

/// Spend an HTLC coin plus one L-BTC fee coin and broadcast.
pub fn spend_htlc(
    esplora: &Esplora,
    params: &HtlcParams,
    coin: &Coin,
    fee_coin: &Coin,
    fee_key: &Keypair,
    how: HtlcSpend,
    genesis: BlockHash,
) -> Res<Txid> {
    esplora.broadcast(&build_spend(params, coin, fee_coin, fee_key, how, genesis)?)
}

/// A signed spend of an HTLC coin plus one L-BTC fee coin, keeping any fee coin value above `FEE` as change.
pub fn build_spend(
    params: &HtlcParams,
    coin: &Coin,
    fee_coin: &Coin,
    fee_key: &Keypair,
    how: HtlcSpend,
    genesis: BlockHash,
) -> Res<Transaction> {
    let htlc = Htlc::new(params).map_err(|e| e.to_string())?;
    let (Asset::Explicit(asset), Value::Explicit(amount)) = (coin.out.asset, coin.out.value) else {
        return Err("blinded HTLC coin".into());
    };
    let (to, lock_time, sequence) = match &how {
        HtlcSpend::Claim { to, .. } => (to.clone(), LockTime::ZERO, Sequence::MAX),
        HtlcSpend::Refund {
            to, lock_height, ..
        } => (
            to.clone(),
            LockTime::from_consensus(*lock_height),
            Sequence::ZERO,
        ),
    };
    let mut output = vec![txout(asset, amount, to)];
    let fee_value = fee_coin.out.value.explicit().ok_or("blinded fee coin")?;
    let change = fee_value.checked_sub(FEE).ok_or("fee coin too small")?;
    if change > 0 {
        output.push(txout(lbtc(), change, address_of(fee_key).script_pubkey()));
    }
    output.push(TxOut::new_fee(FEE, lbtc()));
    let tx = Transaction {
        version: 2,
        lock_time,
        input: vec![
            input(coin.outpoint, sequence),
            input(fee_coin.outpoint, sequence),
        ],
        output,
    };
    let utxos = vec![coin.out.clone(), fee_coin.out.clone()];
    let digest = htlc
        .sighash_all(&tx, &utxos, 0, genesis)
        .map_err(|e| e.to_string())?;
    let secp = Secp256k1::new();
    let spend = match &how {
        HtlcSpend::Claim {
            preimage,
            recipient,
            ..
        } => Spend::Claim {
            preimage: *preimage,
            signature: *secp
                .sign_schnorr_no_aux_rand(&Message::from_digest(digest), recipient)
                .as_ref(),
        },
        HtlcSpend::Refund { sender, .. } => Spend::Refund {
            signature: *secp
                .sign_schnorr_no_aux_rand(&Message::from_digest(digest), sender)
                .as_ref(),
        },
    };
    let (mut tx, report) = htlc
        .finalize(tx, &utxos, 0, genesis, spend)
        .map_err(|e| e.to_string())?;
    println!(
        "    pruned program {} bytes, cost {} of {} WU",
        report.program_bytes, report.cost_wu, report.budget_wu
    );
    sign_keypath(&mut tx, &utxos, 1, fee_key, genesis)?;
    Ok(tx)
}

pub fn unhex_tx_witness_preimage(
    tx: &Transaction,
    outpoint: &OutPoint,
    payment_hash: &[u8; 32],
) -> Option<[u8; 32]> {
    let spend = tx.input.iter().find(|i| i.previous_output == *outpoint)?;
    if spend.witness.script_witness.is_empty() {
        return None;
    }
    liq2sol_htlc::extract_preimage(&spend.witness.script_witness[0], payment_hash)
}
