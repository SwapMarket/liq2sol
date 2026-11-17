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

/// When each host may be asked again after telling us to slow down (HTTP 429), and how many times in a row it has.
fn cooldowns() -> &'static Mutex<HashMap<String, (std::time::Instant, u32)>> {
    static COOL: OnceLock<Mutex<HashMap<String, (std::time::Instant, u32)>>> = OnceLock::new();
    COOL.get_or_init(Mutex::default)
}

fn host_of(url: &str) -> String {
    url.split('/').take(3).collect::<Vec<_>>().join("/")
}

/// How long to leave a host alone after a 429: what it asks for in `Retry-After`, else 15 seconds, doubling each time in a row,
/// up to 5 minutes. Retrying at once would only keep us over its limit.
pub fn backoff(retry_after: Option<&str>, in_a_row: u32) -> std::time::Duration {
    let asked = retry_after.and_then(|v| v.trim().parse::<u64>().ok());
    let doubling = 15u64.saturating_mul(1 << in_a_row.min(5));
    std::time::Duration::from_secs(asked.unwrap_or(doubling).clamp(1, 300))
}

fn get(url: &str) -> Res<String> {
    let host = host_of(url);
    let mut last = String::new();
    for _ in 0..5 {
        // Every request in this process to a host that asked us to slow down waits, not only this one.
        let wait = cooldowns().lock().ok().and_then(|c| c.get(&host).map(|(until, _)| *until));
        if let Some(until) = wait {
            let now = std::time::Instant::now();
            if until > now {
                std::thread::sleep(until - now);
            }
        }
        match ureq::get(url).timeout(std::time::Duration::from_secs(20)).call() {
            Ok(r) => {
                if let Ok(mut c) = cooldowns().lock() {
                    c.remove(&host);
                }
                return r.into_string().map_err(|e| e.to_string());
            }
            Err(ureq::Error::Status(429, r)) => {
                let after = r.header("retry-after").map(str::to_string);
                if let Ok(mut c) = cooldowns().lock() {
                    let in_a_row = c.get(&host).map_or(0, |(_, n)| n + 1);
                    let pause = backoff(after.as_deref(), in_a_row);
                    c.insert(host.clone(), (std::time::Instant::now() + pause, in_a_row));
                    last = format!("GET {url}: rate limited (429), pausing {}s", pause.as_secs());
                }
                // No retry here: the next tick asks again, after the pause.
                break;
            }
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

    /// (txid, vout, confirmed height) of each unspent output at any of these addresses. A node reads them all in one
    /// scan. An API is asked once per address, so a list is only for a node: an API would see every address come from one IP.
    fn unspent(&self, addresses: &[Address]) -> Res<Vec<(Txid, u32, Option<u32>)>> {
        let parse = |t: &Json| Txid::from_str(t.as_str().unwrap_or_default()).map_err(|e| e.to_string());
        match self {
            Esplora::Http(u) => {
                let mut all = vec![];
                for address in addresses {
                    let body = get(&format!("{u}/address/{address}/utxo"))?;
                    let list: Vec<Json> = serde_json::from_str(&body).map_err(|e| e.to_string())?;
                    for c in &list {
                        let height = c["status"]["block_height"].as_u64().map(|h| h as u32);
                        all.push((parse(&c["txid"])?, c["vout"].as_u64().ok_or("utxo vout")? as u32, height));
                    }
                }
                Ok(all)
            }
            Esplora::Node(n) => {
                let descs: Vec<String> = addresses.iter().map(|a| format!("addr({a})")).collect();
                let desc = serde_json::to_string(&descs).map_err(|e| e.to_string())?;
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
                // Still unspent, mempool included, is the usual answer, and one call. Walking the chain for it instead hung a
                // mainnet maker: there are millions of blocks.
                let (txid, vout) = (outpoint.txid.to_string(), outpoint.vout.to_string());
                if !n.call(&["gettxout", &txid, &vout, "true"])?.trim().is_empty() {
                    return Ok(None);
                }
                let spends = |tx: &Transaction| tx.input.iter().any(|i| i.previous_output == *outpoint);
                for id in n.json(&["getrawmempool"])?.as_array().ok_or("mempool")? {
                    let tx = self.tx(&Txid::from_str(id.as_str().unwrap_or_default()).map_err(|e| e.to_string())?)?;
                    if spends(&tx) {
                        return Ok(Some(tx));
                    }
                }
                // Spent in a block, so at or after the coin's own block. A coin the node doesn't know, or not yet in a
                // block, has no spend there.
                let Ok(made) = n.json(&["getrawtransaction", &txid, "1"]) else { return Ok(None) };
                let Some(block) = made["blockhash"].as_str() else { return Ok(None) };
                let from = n.json(&["getblockheader", block])?["height"].as_u64().ok_or("block height")? as u32;
                for height in (from..=self.tip()?).rev() {
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
        self.outputs_at(std::slice::from_ref(address))
    }

    /// Unspent outputs at any of these addresses, each checked against the script it claims to pay.
    fn outputs_at(&self, addresses: &[Address]) -> Res<Vec<Coin>> {
        let plain: Vec<Address> = addresses.iter().map(Address::to_unconfidential).collect();
        let scripts: Vec<_> = plain.iter().map(Address::script_pubkey).collect();
        let mut coins = vec![];
        for (txid, vout, height) in self.unspent(&plain)? {
            let tx = self.tx(&txid)?;
            let out = tx.output.get(vout as usize).ok_or("utxo vout range")?.clone();
            if scripts.contains(&out.script_pubkey) {
                coins.push(Coin { outpoint: OutPoint::new(txid, vout), out, height });
            }
        }
        Ok(coins)
    }

    /// The outputs at this address that we can read.
    pub fn secret_coins(&self, address: &Address, blinding: &SecretKey) -> Res<Vec<SecretCoin>> {
        self.secret_coins_at(&[(address.clone(), *blinding)])
    }

    /// The outputs we can read at any of these addresses, each with its own blinding key.
    pub fn secret_coins_at(&self, wallets: &[(Address, SecretKey)]) -> Res<Vec<SecretCoin>> {
        let addresses: Vec<Address> = wallets.iter().map(|(a, _)| a.clone()).collect();
        Ok(self
            .outputs_at(&addresses)?
            .into_iter()
            .filter_map(|coin| {
                let (_, blinding) = wallets.iter().find(|(a, _)| a.script_pubkey() == coin.out.script_pubkey)?;
                read_output(&coin.out, blinding).map(|secrets| SecretCoin { coin, secrets })
            })
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
