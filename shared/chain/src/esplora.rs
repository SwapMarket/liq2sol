//! Reading and writing the Liquid chain: a public Esplora API, or a local node. Native only, because it uses blocking HTTP and
//! starts `elements-cli`. The browser client reaches the same APIs from JavaScript.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};

use serde_json::Value as Json;
use simplicityhl::elements::encode::{deserialize, serialize_hex};
use simplicityhl::elements::secp256k1_zkp::SecretKey;
use simplicityhl::elements::{Address, BlockHash, OutPoint, Transaction, Txid};

const MAX_CACHED_TXS: usize = 5_000;

fn tx_cache() -> &'static Mutex<HashMap<Txid, Transaction>> {
    static CACHE: OnceLock<Mutex<HashMap<Txid, Transaction>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

use crate::liquid::{pay_from_coins, read_output, Coin, Payment, Res, SecretCoin, Wallet};

fn get(url: &str) -> Res<String> {
    let mut last = String::new();
    for _ in 0..5 {
        match ureq::get(url).timeout(std::time::Duration::from_secs(20)).call() {
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
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string())).collect()
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
            Err(format!("{:?}: {}", args.first(), String::from_utf8_lossy(&out.stderr).trim()))
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

impl Esplora {
    pub fn tip(&self) -> Res<u32> {
        match self {
            Esplora::Http(u) => get(&format!("{u}/blocks/tip/height"))?.trim().parse().map_err(|e| format!("tip: {e}")),
            Esplora::Node(n) => n.call(&["getblockcount"])?.parse().map_err(|e| format!("tip: {e}")),
        }
    }

    /// The tip block's own timestamp, used to tell how long the chain has been stalled.
    pub fn tip_time(&self) -> Res<u64> {
        match self {
            Esplora::Http(u) => {
                let hash = get(&format!("{u}/blocks/tip/hash"))?;
                let block: Json =
                    serde_json::from_str(&get(&format!("{u}/block/{}", hash.trim()))?).map_err(|e| e.to_string())?;
                block["timestamp"].as_u64().ok_or_else(|| "block timestamp".to_string())
            }
            Esplora::Node(n) => {
                let hash = n.call(&["getbestblockhash"])?;
                n.json(&["getblockheader", &hash])?["time"].as_u64().ok_or_else(|| "block time".to_string())
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
                let status: Json =
                    serde_json::from_str(&get(&format!("{u}/tx/{txid}/status"))?).map_err(|e| e.to_string())?;
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

    /// A transaction by its id. Over a public API these are kept, because its contents can never change and those APIs rate limit:
    /// a wallet's coins would otherwise be fetched again on every read. The hash is checked, so a wrong answer is refused, not kept.
    pub fn tx(&self, txid: &Txid) -> Res<Transaction> {
        if matches!(self, Esplora::Http(_)) {
            if let Some(tx) = tx_cache().lock().ok().and_then(|c| c.get(txid).cloned()) {
                return Ok(tx);
            }
        }
        let tx = self.fetch_tx(txid)?;
        if tx.txid() != *txid {
            return Err(format!("asked for transaction {txid}, and got {}", tx.txid()));
        }
        if matches!(self, Esplora::Http(_)) {
            if let Ok(mut cache) = tx_cache().lock() {
                if cache.len() >= MAX_CACHED_TXS {
                    cache.clear();
                }
                cache.insert(*txid, tx.clone());
            }
        }
        Ok(tx)
    }

    fn fetch_tx(&self, txid: &Txid) -> Res<Transaction> {
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
                match ureq::post(&url).timeout(std::time::Duration::from_secs(30)).send_string(&serialize_hex(tx)) {
                    Ok(r) => {
                        Txid::from_str(r.into_string().map_err(|e| e.to_string())?.trim()).map_err(|e| e.to_string())
                    }
                    Err(ureq::Error::Status(code, r)) => {
                        Err(format!("broadcast {code}: {}", r.into_string().unwrap_or_default()))
                    }
                    Err(e) => Err(format!("broadcast: {e}")),
                }
            }
            Esplora::Node(n) => {
                Txid::from_str(&n.call(&["sendrawtransaction", &serialize_hex(tx)])?).map_err(|e| e.to_string())
            }
        }
    }

    /// (txid, vout, confirmed height) of each unspent output at an address.
    fn unspent(&self, address: &Address) -> Res<Vec<(Txid, u32, Option<u32>)>> {
        let parse = |t: &Json| Txid::from_str(t.as_str().unwrap_or_default()).map_err(|e| e.to_string());
        match self {
            Esplora::Http(u) => {
                let body = get(&format!("{u}/address/{address}/utxo"))?;
                let list: Vec<Json> = serde_json::from_str(&body).map_err(|e| e.to_string())?;
                list.iter()
                    .map(|c| {
                        let height = c["status"]["block_height"].as_u64().map(|h| h as u32);
                        Ok((parse(&c["txid"])?, c["vout"].as_u64().ok_or("utxo vout")? as u32, height))
                    })
                    .collect()
            }
            Esplora::Node(n) => {
                let desc = format!("[\"addr({address})\"]");
                // A node runs one scan at a time, and a maker's daemon and signer share a node, so wait for a turn.
                let mut tries = 0;
                let found = loop {
                    match n.json(&["scantxoutset", "start", &desc]) {
                        Err(e) if e.contains("error code: -8") && tries < 100 => {
                            tries += 1;
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                        other => break other?,
                    }
                };
                found["unspents"]
                    .as_array()
                    .ok_or("scantxoutset result")?
                    .iter()
                    .map(|c| {
                        let height = c["height"].as_u64().map(|h| h as u32);
                        Ok((parse(&c["txid"])?, c["vout"].as_u64().ok_or("utxo vout")? as u32, height))
                    })
                    .collect()
            }
        }
    }

    /// The transaction spending an outpoint, if any, mempool included.
    pub fn spender(&self, outpoint: &OutPoint) -> Res<Option<Transaction>> {
        match self {
            Esplora::Http(u) => {
                let body = get(&format!("{u}/tx/{}/outspend/{}", outpoint.txid, outpoint.vout))?;
                let status: Json = serde_json::from_str(&body).map_err(|e| e.to_string())?;
                match status["txid"].as_str().filter(|_| status["spent"].as_bool() == Some(true)) {
                    Some(txid) => Ok(Some(self.tx(&Txid::from_str(txid).map_err(|e| e.to_string())?)?)),
                    None => Ok(None),
                }
            }
            Esplora::Node(n) => {
                // Regtest chains here are short, so scanning the mempool and all blocks is fine.
                let spends = |tx: &Transaction| tx.input.iter().any(|i| i.previous_output == *outpoint);
                for id in n.json(&["getrawmempool"])?.as_array().ok_or("mempool")? {
                    let tx = self.tx(&Txid::from_str(id.as_str().unwrap_or_default()).map_err(|e| e.to_string())?)?;
                    if spends(&tx) {
                        return Ok(Some(tx));
                    }
                }
                for height in (1..=self.tip()?).rev() {
                    let hash = n.call(&["getblockhash", &height.to_string()])?;
                    let block = n.json(&["getblock", &hash])?;
                    for id in block["tx"].as_array().ok_or("block txs")? {
                        let tx =
                            self.tx(&Txid::from_str(id.as_str().unwrap_or_default()).map_err(|e| e.to_string())?)?;
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
impl Esplora {
    /// Every unspent output paying this address's script, blinded or not. The blinding key has no effect on the script.
    pub fn outputs(&self, address: &Address) -> Res<Vec<Coin>> {
        let plain = address.to_unconfidential();
        let mut coins = vec![];
        for (txid, vout, height) in self.unspent(&plain)? {
            let tx = self.tx(&txid)?;
            let out = tx.output.get(vout as usize).ok_or("utxo vout range")?.clone();
            if out.script_pubkey == plain.script_pubkey() {
                coins.push(Coin { outpoint: OutPoint::new(txid, vout), out, height });
            }
        }
        Ok(coins)
    }

    /// The outputs at this address that we can read.
    pub fn secret_coins(&self, address: &Address, blinding: &SecretKey) -> Res<Vec<SecretCoin>> {
        Ok(self
            .outputs(address)?
            .into_iter()
            .filter_map(|coin| read_output(&coin.out, blinding).map(|secrets| SecretCoin { coin, secrets }))
            .collect())
    }
}

/// A signed payment from a confidential wallet. Every output but the L-BTC fee is blinded.
pub fn build_confidential_pay(
    esplora: &Esplora,
    wallet: &Wallet,
    payments: Vec<Payment>,
    genesis: BlockHash,
) -> Res<Transaction> {
    let coins = esplora.secret_coins(&wallet.address(), &wallet.blinding)?;
    pay_from_coins(&coins, wallet, payments, genesis)
}
