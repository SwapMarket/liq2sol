//! The maker's signer. See docs/maker.md section 2.
//!
//! This process holds every hot key, and it is the only thing that can spend. It treats whoever is on the socket as
//! possibly compromised, so:
//!
//! - It builds each transaction itself. Our claims and refunds pay our own wallet, because nothing else can be asked for.
//! - It reads heights, slots, coins and escrows through its own chain connections, and never takes them from a request.
//! - It runs `policy::check`, commits the result durably, and only then releases a signature.
//! - It never broadcasts, and it never returns or logs a key.

#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anchor_lang::prelude::Pubkey;
use liq2sol_chain::liquid::{
    self, address_of, build_pay, build_spend, keypair_from, network, txout, usdt, use_network, Coin, Esplora,
    HtlcSpend, NetworkConfig, Rpc, FEE, LIQUID_MAINNET_GENESIS,
};
use liq2sol_chain::sol::{self, escrow_pda, Sol, MAINNET_GENESIS};
use liq2sol_htlc::{asset_param, script_hash, Htlc, HtlcParams};
use liq2sol_maker_core::policy::{self, CoinEvidence, EscrowEvidence, Ledger, Limits, Refusal, Request};
use liq2sol_maker_core::store::SignerLedger;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use simplicityhl::elements::encode::{deserialize, serialize, serialize_hex};
use simplicityhl::elements::secp256k1_zkp::Keypair as LiquidKey;
use simplicityhl::elements::{AddressParams, AssetId, BlockHash, Script, Transaction as LiquidTx};
use solana_escrow::state::{Escrow, EscrowState};
use solana_sdk::signature::{read_keypair_file, write_keypair_file, Keypair};
use solana_sdk::signer::Signer as _;

/// Longest accepted request line. Real requests are well under this.
pub const MAX_REQUEST_BYTES: u64 = 16 * 1024;
/// Open connections beyond this are closed at once, so a flood can't exhaust threads.
pub const MAX_CONNECTIONS: usize = 16;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LiquidSource {
    Esplora(String),
    Node { cli: String, args: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub socket: PathBuf,
    pub ledger: PathBuf,
    pub keys_dir: PathBuf,
    pub liquid: LiquidSource,
    pub solana_rpc: String,
    pub limits: Limits,
    /// Regtest assets as (stand-in USDt, L-BTC). Liquid testnet is the default.
    pub regtest_assets: Option<(String, String)>,
    /// The cold owner of the maker vault this signer operates, if any. With one, Solana locks draw from the vault under
    /// its on-chain window cap, and a stolen hot key can't exceed that. The owner's key is never on this host.
    pub vault_owner: Option<String>,
}

/// L→S terms from `rfq-terms`: the taker pays the HTLC, and we pay the taker on Solana.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L2sTerms {
    pub payment_hash: [u8; 32],
    pub taker_pk: [u8; 32],
    pub lock_height: u32,
    pub liquid_amount: u64,
    pub solana_amount: u64,
    pub taker_solana: String,
}

/// S→L terms from `rfq-terms`: the taker pays the escrow, and we pay the HTLC plus a fee coin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S2lTerms {
    pub payment_hash: [u8; 32],
    pub taker_pk: [u8; 32],
    pub taker_script: Vec<u8>,
    pub taker_fee_script: Vec<u8>,
    pub lock_height: u32,
    pub liquid_amount: u64,
    pub taker_solana: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Call {
    Identity,
    SwapKey {
        payment_hash: [u8; 32],
    },
    LockSolana {
        terms: L2sTerms,
        deadline_slot: u64,
    },
    LockLiquid {
        terms: S2lTerms,
    },
    ClaimLiquid {
        terms: L2sTerms,
        preimage: [u8; 32],
    },
    ClaimSolana {
        taker_solana: String,
        payment_hash: [u8; 32],
        preimage: [u8; 32],
    },
    RefundLiquid {
        terms: S2lTerms,
    },
    RefundSolana {
        payment_hash: [u8; 32],
    },
    /// Move collected tokens from the hot wallet into the vault, so they are back under the cap.
    DepositToVault {
        amount: u64,
    },
    Extend {
        taker_solana: String,
        payment_hash: [u8; 32],
        new_deadline: u64,
    },
    Release {
        taker_solana: String,
        payment_hash: [u8; 32],
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    Identity {
        liquid_address: String,
        liquid_script: Vec<u8>,
        solana: String,
        /// What our Solana escrows are seeded with: the vault if there is one, or `solana`.
        escrow_initiator: String,
    },
    SwapKey {
        x_only: [u8; 32],
    },
    /// A signed Liquid transaction, in hex. The caller broadcasts it.
    Liquid(String),
    /// A signed Solana transaction, bincode-encoded, in hex. The caller broadcasts it.
    Solana(String),
    Refused(Refusal),
    /// Could not decide, such as a chain read failing. Nothing was signed or recorded.
    Failed(String),
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd hex".into());
    }
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string())).collect()
}

/// Nothing in this project may run against mainnet. Checked before any key is loaded.
pub fn refuse_mainnet(liquid_genesis: &str, solana_genesis: &str) -> Result<(), String> {
    if liquid_genesis == LIQUID_MAINNET_GENESIS {
        return Err("refusing to start: this is Liquid mainnet".into());
    }
    if solana_genesis == MAINNET_GENESIS {
        return Err("refusing to start: this is Solana mainnet".into());
    }
    Ok(())
}

fn load_or_create_secret(path: &Path) -> Result<[u8; 32], String> {
    if !path.exists() {
        let mut secret = [0u8; 32];
        getrandom::getrandom(&mut secret).map_err(|e| e.to_string())?;
        std::fs::write(path, secret).map_err(|e| e.to_string())?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    std::fs::read(path).map_err(|e| e.to_string())?.try_into().map_err(|_| "bad key file".to_string())
}

/// The HTLC for an L→S swap. Shared with the daemon, so both derive the same address.
pub fn l2s_params(t: &L2sTerms, our_swap_key: [u8; 32], our_script: &Script) -> HtlcParams {
    HtlcParams {
        payment_hash: t.payment_hash,
        recipient_pk: our_swap_key,
        sender_pk: t.taker_pk,
        lock_height: t.lock_height,
        usdt_asset: asset_param(&usdt()),
        amount: t.liquid_amount,
        recipient_script_hash: script_hash(our_script),
    }
}

/// The HTLC for an S→L swap. Shared with the daemon, so both derive the same address.
pub fn s2l_params(t: &S2lTerms, our_swap_key: [u8; 32]) -> HtlcParams {
    HtlcParams {
        payment_hash: t.payment_hash,
        recipient_pk: t.taker_pk,
        sender_pk: our_swap_key,
        lock_height: t.lock_height,
        usdt_asset: asset_param(&usdt()),
        amount: t.liquid_amount,
        recipient_script_hash: script_hash(&Script::from(t.taker_script.clone())),
    }
}

pub struct Signer {
    limits: Limits,
    chain: Esplora,
    sol: Sol,
    ledger: SignerLedger,
    genesis: BlockHash,
    wallet: LiquidKey,
    solana: Keypair,
    swap_seed: [u8; 32],
    vault_owner: Option<Pubkey>,
}

/// Refuse, or fail, without signing anything.
enum Stop {
    Refused(Refusal),
    Failed(String),
}

impl From<String> for Stop {
    fn from(e: String) -> Self {
        Stop::Failed(e)
    }
}

impl From<&str> for Stop {
    fn from(e: &str) -> Self {
        Stop::Failed(e.to_string())
    }
}

impl From<Refusal> for Stop {
    fn from(r: Refusal) -> Self {
        Stop::Refused(r)
    }
}

type Outcome = Result<Reply, Stop>;

impl Signer {
    pub fn new(cfg: &Config) -> Result<Self, String> {
        let chain = match &cfg.liquid {
            LiquidSource::Esplora(url) => Esplora::Http(url.clone()),
            LiquidSource::Node { cli, args } => Esplora::Node(Rpc { cli: cli.clone(), args: args.clone() }),
        };
        let sol = Sol::new(&cfg.solana_rpc);
        let genesis = chain.genesis()?;
        refuse_mainnet(&genesis, &sol.genesis()?)?;
        if let Some((usdt_id, lbtc_id)) = &cfg.regtest_assets {
            use_network(NetworkConfig {
                params: &AddressParams::ELEMENTS,
                usdt: AssetId::from_str(usdt_id).map_err(|e| e.to_string())?,
                lbtc: AssetId::from_str(lbtc_id).map_err(|e| e.to_string())?,
            });
        }
        std::fs::create_dir_all(&cfg.keys_dir).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&cfg.keys_dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
        let wallet = keypair_from(&load_or_create_secret(&cfg.keys_dir.join("liquid-wallet.key"))?)?;
        let swap_seed = load_or_create_secret(&cfg.keys_dir.join("swap-seed.key"))?;
        let solana_path = cfg.keys_dir.join("solana.json");
        if !solana_path.exists() {
            write_keypair_file(&Keypair::new(), &solana_path).map_err(|e| e.to_string())?;
        }
        std::fs::set_permissions(&solana_path, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
        let solana = read_keypair_file(&solana_path).map_err(|e| e.to_string())?;
        Ok(Self {
            limits: cfg.limits.clone(),
            chain,
            sol,
            ledger: SignerLedger::open(&cfg.ledger).map_err(|e| e.to_string())?,
            genesis: BlockHash::from_str(&genesis).map_err(|e| e.to_string())?,
            wallet,
            solana,
            swap_seed,
            vault_owner: cfg.vault_owner.as_deref().map(parse_pubkey).transpose()?,
        })
    }

    /// One key per swap, derived so that a restored seed can still spend every open swap.
    fn swap_key(&self, payment_hash: &[u8; 32]) -> Result<LiquidKey, String> {
        let digest: [u8; 32] = Sha256::new()
            .chain_update(b"liq2sol-swap-key-v1")
            .chain_update(self.swap_seed)
            .chain_update(payment_hash)
            .finalize()
            .into();
        keypair_from(&digest)
    }

    fn x_only(key: &LiquidKey) -> [u8; 32] {
        key.x_only_public_key().0.serialize()
    }

    fn wallet_script(&self) -> Script {
        address_of(&self.wallet).script_pubkey()
    }

    fn l2s_params(&self, t: &L2sTerms) -> Result<HtlcParams, String> {
        Ok(l2s_params(t, Self::x_only(&self.swap_key(&t.payment_hash)?), &self.wallet_script()))
    }

    fn s2l_params(&self, t: &S2lTerms) -> Result<HtlcParams, String> {
        Ok(s2l_params(t, Self::x_only(&self.swap_key(&t.payment_hash)?)))
    }

    /// The exact coin at this HTLC, as this process's own chain connection sees it.
    fn exact_coin(&self, params: &HtlcParams) -> Result<Option<Coin>, String> {
        let htlc = Htlc::new(params).map_err(|e| e.to_string())?;
        let coins = self.chain.coins(&htlc.address(network().params))?;
        Ok(liquid::exact_coin(&coins, usdt(), params.amount, self.chain.tip()?, 0))
    }

    /// An L-BTC coin of our own, to pay a spend's fee. Any excess returns to the wallet.
    fn fee_coin(&self) -> Result<Coin, String> {
        self.chain
            .coins(&address_of(&self.wallet))?
            .into_iter()
            .find(|c| c.out.asset.explicit() == Some(liquid::lbtc()) && c.out.value.explicit().unwrap_or(0) >= FEE)
            .ok_or_else(|| "the Liquid wallet has no L-BTC for the fee".to_string())
    }

    fn escrow_initiator(&self) -> Pubkey {
        self.vault_owner.map_or_else(|| self.solana.pubkey(), |owner| sol::maker_vault_pda(&owner))
    }

    fn deposit_to_vault(&self, amount: u64) -> Outcome {
        let owner = self.vault_owner.ok_or("no vault is configured")?;
        // The destination is fixed to our own vault, so this needs no limit.
        self.solana_reply(&[sol::vault_deposit_ix(&self.solana.pubkey(), &owner, amount)])
    }

    fn escrow(&self, address: &Pubkey) -> Result<Option<Escrow>, String> {
        self.sol.escrow(address)
    }

    /// Check policy, persist, and only then hand back the signed bytes. A crash before the commit releases nothing.
    fn check(&self, request: &Request) -> Result<policy::SwapRecord, Stop> {
        Ok(policy::check(&self.limits, &self.ledger, now_secs(), request)?)
    }

    fn commit(
        &self,
        hash: &[u8; 32],
        request: &Request,
        record: &policy::SwapRecord,
        lock_tx: Option<&[u8]>,
    ) -> Result<(), Stop> {
        let before = self.ledger.record(hash);
        let newly = policy::newly_locked(&before, request).map(|amount| (now_secs(), amount));
        self.ledger.commit(hash, record, newly, lock_tx).map_err(|e| Stop::Failed(e.to_string()))
    }

    fn solana_reply(&self, ixs: &[solana_sdk::instruction::Instruction]) -> Outcome {
        let tx = self.sol.build(ixs, &self.solana, &[])?;
        Ok(Reply::Solana(hex(&bincode::serialize(&tx).map_err(|e| e.to_string())?)))
    }

    fn lock_solana(&self, t: &L2sTerms, deadline_slot: u64) -> Outcome {
        let taker = parse_pubkey(&t.taker_solana)?;
        let params = self.l2s_params(t)?;
        let tip = self.chain.tip()?;
        let taker_coin = self.exact_coin(&params)?.and_then(|c| {
            let confirmations = c.height.map_or(0, |h| (tip + 1).saturating_sub(h));
            Some(CoinEvidence { amount: c.out.value.explicit()?, confirmations })
        });
        let request = Request::LockSolana {
            payment_hash: t.payment_hash,
            amount: t.solana_amount,
            deadline_slot,
            solana_slot: self.sol.slot()?,
            liquid_lock_height: t.lock_height,
            liquid_tip: tip,
            taker_coin,
        };
        let record = self.check(&request)?;
        let operator = self.solana.pubkey();
        let ix = match &self.vault_owner {
            Some(owner) => {
                sol::lock_from_vault_ix(&operator, owner, t.payment_hash, taker, t.solana_amount, deadline_slot, true)
            }
            None => sol::lock_ix(&operator, t.payment_hash, taker, t.solana_amount, deadline_slot, true),
        };
        let reply = self.solana_reply(&[ix])?;
        self.commit(&t.payment_hash, &request, &record, None)?;
        Ok(reply)
    }

    fn lock_liquid(&self, t: &S2lTerms) -> Outcome {
        let params = self.s2l_params(t)?;
        let htlc = Htlc::new(&params).map_err(|e| e.to_string())?;
        let htlc_script = htlc.address(network().params).script_pubkey();

        // A retry gets the same bytes, so the HTLC can never be paid twice. Different terms get nothing.
        if let Some(raw) = self.ledger.lock_tx(&t.payment_hash).map_err(|e| e.to_string())? {
            let tx: LiquidTx = deserialize(&raw).map_err(|e| e.to_string())?;
            if tx.output.first().map(|o| &o.script_pubkey) != Some(&htlc_script) {
                return Err(Refusal::AlreadyLocked.into());
            }
            return Ok(Reply::Liquid(hex(&raw)));
        }

        let taker = parse_pubkey(&t.taker_solana)?;
        let slot = self.sol.slot()?;
        let taker_escrow = self.escrow(&escrow_pda(&taker, &t.payment_hash))?.map(|e| EscrowEvidence {
            amount: e.amount,
            matches_terms: e.state == EscrowState::Open
                && e.payment_hash == t.payment_hash
                && e.recipient == self.solana.pubkey()
                && !e.close_on_restart
                && !e.extended,
            slots_left: e.deadline_slot.saturating_sub(slot),
        });
        let request = Request::LockLiquid {
            payment_hash: t.payment_hash,
            liquid_amount: t.liquid_amount,
            lock_height: t.lock_height,
            liquid_tip: self.chain.tip()?,
            taker_escrow,
        };
        let record = self.check(&request)?;
        let outputs = vec![
            txout(usdt(), t.liquid_amount, htlc_script),
            // Exactly one fee coin's worth of L-BTC, so this can't be used to drain the wallet's L-BTC.
            txout(liquid::lbtc(), FEE, Script::from(t.taker_fee_script.clone())),
        ];
        let tx = build_pay(&self.chain, &self.wallet, outputs, self.genesis)?;
        let raw = serialize(&tx);
        self.commit(&t.payment_hash, &request, &record, Some(&raw))?;
        Ok(Reply::Liquid(hex(&raw)))
    }

    fn claim_liquid(&self, t: &L2sTerms, preimage: [u8; 32]) -> Outcome {
        let request = Request::ClaimLiquid { payment_hash: t.payment_hash, preimage };
        let record = self.check(&request)?;
        let params = self.l2s_params(t)?;
        let coin = self.exact_coin(&params)?.ok_or("the HTLC coin is not there")?;
        let how = HtlcSpend::Claim { preimage, recipient: self.swap_key(&t.payment_hash)?, to: self.wallet_script() };
        let tx = build_spend(&params, &coin, &self.fee_coin()?, &self.wallet, how, self.genesis)?;
        self.commit(&t.payment_hash, &request, &record, None)?;
        Ok(Reply::Liquid(serialize_hex(&tx)))
    }

    fn claim_solana(&self, taker_solana: &str, payment_hash: [u8; 32], preimage: [u8; 32]) -> Outcome {
        let request = Request::ClaimSolana { payment_hash, preimage };
        let record = self.check(&request)?;
        let address = escrow_pda(&parse_pubkey(taker_solana)?, &payment_hash);
        let escrow = self.escrow(&address)?.ok_or("the escrow is not there")?;
        let reply = self.solana_reply(&sol::claim_ixs(&self.solana.pubkey(), &address, &escrow, preimage))?;
        // Recorded before release: from here on, the Liquid coin is the taker's, even if this is never sent.
        self.commit(&payment_hash, &request, &record, None)?;
        Ok(reply)
    }

    fn refund_liquid(&self, t: &S2lTerms) -> Outcome {
        let request = Request::RefundLiquid {
            payment_hash: t.payment_hash,
            lock_height: t.lock_height,
            liquid_tip: self.chain.tip()?,
        };
        let record = self.check(&request)?;
        let params = self.s2l_params(t)?;
        let coin = self.exact_coin(&params)?.ok_or("the HTLC coin is not there")?;
        let how = HtlcSpend::Refund {
            sender: self.swap_key(&t.payment_hash)?,
            lock_height: t.lock_height,
            to: self.wallet_script(),
        };
        let tx = build_spend(&params, &coin, &self.fee_coin()?, &self.wallet, how, self.genesis)?;
        self.commit(&t.payment_hash, &request, &record, None)?;
        Ok(Reply::Liquid(serialize_hex(&tx)))
    }

    fn refund_solana(&self, payment_hash: [u8; 32]) -> Outcome {
        let request = Request::RefundSolana { payment_hash };
        let record = self.check(&request)?;
        let address = escrow_pda(&self.escrow_initiator(), &payment_hash);
        let escrow = self.escrow(&address)?.ok_or("the escrow is not there")?;
        let reply = self.solana_reply(&[sol::refund_ix(&address, &escrow)])?;
        self.commit(&payment_hash, &request, &record, None)?;
        Ok(reply)
    }

    fn extend(&self, taker_solana: &str, payment_hash: [u8; 32], new_deadline: u64) -> Outcome {
        let address = escrow_pda(&parse_pubkey(taker_solana)?, &payment_hash);
        let escrow = self.escrow(&address)?.ok_or("the escrow is not there")?;
        let request = Request::Extend {
            payment_hash,
            current_deadline: escrow.deadline_slot,
            new_deadline,
            liquid_tip_age_secs: now_secs().saturating_sub(self.chain.tip_time()?),
        };
        let record = self.check(&request)?;
        let reply = self.solana_reply(&[sol::extend_ix(&self.solana.pubkey(), &address, new_deadline)])?;
        self.commit(&payment_hash, &request, &record, None)?;
        Ok(reply)
    }

    fn release(&self, taker_solana: &str, payment_hash: [u8; 32]) -> Outcome {
        let request = Request::Release { payment_hash };
        let record = self.check(&request)?;
        let address = escrow_pda(&parse_pubkey(taker_solana)?, &payment_hash);
        let escrow = self.escrow(&address)?.ok_or("the escrow is not there")?;
        let reply = self.solana_reply(&[sol::release_ix(&self.solana.pubkey(), &address, &escrow)])?;
        self.commit(&payment_hash, &request, &record, None)?;
        Ok(reply)
    }

    pub fn handle(&self, call: &Call) -> Reply {
        let outcome: Outcome = match call {
            Call::Identity => Ok(Reply::Identity {
                liquid_address: address_of(&self.wallet).to_string(),
                liquid_script: self.wallet_script().to_bytes(),
                solana: self.solana.pubkey().to_string(),
                escrow_initiator: self.escrow_initiator().to_string(),
            }),
            Call::SwapKey { payment_hash } => {
                self.swap_key(payment_hash).map(|k| Reply::SwapKey { x_only: Self::x_only(&k) }).map_err(Stop::Failed)
            }
            Call::LockSolana { terms, deadline_slot } => self.lock_solana(terms, *deadline_slot),
            Call::LockLiquid { terms } => self.lock_liquid(terms),
            Call::ClaimLiquid { terms, preimage } => self.claim_liquid(terms, *preimage),
            Call::ClaimSolana { taker_solana, payment_hash, preimage } => {
                self.claim_solana(taker_solana, *payment_hash, *preimage)
            }
            Call::RefundLiquid { terms } => self.refund_liquid(terms),
            Call::RefundSolana { payment_hash } => self.refund_solana(*payment_hash),
            Call::DepositToVault { amount } => self.deposit_to_vault(*amount),
            Call::Extend { taker_solana, payment_hash, new_deadline } => {
                self.extend(taker_solana, *payment_hash, *new_deadline)
            }
            Call::Release { taker_solana, payment_hash } => self.release(taker_solana, *payment_hash),
        };
        match outcome {
            Ok(reply) => reply,
            Err(Stop::Refused(r)) => Reply::Refused(r),
            Err(Stop::Failed(e)) => Reply::Failed(e),
        }
    }
}

fn parse_pubkey(s: &str) -> Result<Pubkey, String> {
    Pubkey::from_str(s).map_err(|e| format!("bad Solana address: {e}"))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Serve one connection until it closes. A bad or oversized request gets an error, and nothing else happens.
/// Requests are read before taking the lock, so a slow or stalled client can never hold up anyone else.
fn serve(signer: &Mutex<Signer>, stream: UnixStream) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = Vec::new();
        let read = (&mut reader).take(MAX_REQUEST_BYTES + 1).read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(());
        }
        if line.len() as u64 > MAX_REQUEST_BYTES {
            let reply = Reply::Failed("request too large".into());
            writeln!(writer, "{}", serde_json::to_string(&reply).unwrap_or_default())?;
            return Ok(());
        }
        let reply = match serde_json::from_slice::<Call>(&line) {
            // One request at a time, so policy checks can't race. A poisoned lock means a request panicked part way,
            // so stop signing rather than carry on from an unknown state.
            Ok(call) => signer.lock().map_err(|_| std::io::Error::other("signer poisoned"))?.handle(&call),
            Err(_) => Reply::Failed("malformed request".into()),
        };
        writeln!(writer, "{}", serde_json::to_string(&reply).unwrap_or_default())?;
    }
}

/// Run until the process is stopped.
pub fn run(config_path: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(config_path).map_err(|e| e.to_string())?;
    let cfg: Config = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    // A chain outage must not stop us coming back up, or we couldn't sign claims when it ends. The mainnet check stays
    // fail-closed: nothing is served until it has actually passed.
    let signer = loop {
        match Signer::new(&cfg) {
            Ok(signer) => break Arc::new(Mutex::new(signer)),
            Err(e) if e.starts_with("refusing") => return Err(e),
            Err(e) => {
                eprintln!("waiting for the chains: {e}");
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        }
    };
    let _ = std::fs::remove_file(&cfg.socket);
    // The socket's own mode is set just after binding, so the directory must already keep everyone else out.
    let parent = cfg.socket.parent().ok_or("socket has no parent directory")?;
    let mode = std::fs::metadata(parent).map_err(|e| e.to_string())?.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(format!("refusing to start: {} must be private to this user (mode 700)", parent.display()));
    }
    let listener = UnixListener::bind(&cfg.socket).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&cfg.socket, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    let open = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept failed: {e}");
                continue;
            }
        };
        if open.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            open.fetch_sub(1, Ordering::SeqCst);
            continue; // Dropping the stream closes it.
        }
        let (signer, open) = (signer.clone(), open.clone());
        std::thread::spawn(move || {
            if let Err(e) = serve(&signer, stream) {
                eprintln!("connection ended: {e}");
            }
            open.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

/// The engine's end of the socket.
pub struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Self, String> {
        let writer = UnixStream::connect(path).map_err(|e| e.to_string())?;
        let reader = BufReader::new(writer.try_clone().map_err(|e| e.to_string())?);
        Ok(Self { writer, reader })
    }

    pub fn call(&mut self, call: &Call) -> Result<Reply, String> {
        let text = serde_json::to_string(call).map_err(|e| e.to_string())?;
        writeln!(self.writer, "{text}").map_err(|e| e.to_string())?;
        let mut line = String::new();
        self.reader.read_line(&mut line).map_err(|e| e.to_string())?;
        serde_json::from_str(&line).map_err(|e| e.to_string())
    }

    /// Send raw bytes, for testing how the signer handles bad input.
    pub fn call_raw(&mut self, bytes: &[u8]) -> Result<String, String> {
        self.writer.write_all(bytes).map_err(|e| e.to_string())?;
        let mut line = String::new();
        self.reader.read_line(&mut line).map_err(|e| e.to_string())?;
        Ok(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainnet_is_refused_on_either_chain() {
        assert!(refuse_mainnet(LIQUID_MAINNET_GENESIS, "x").is_err());
        assert!(refuse_mainnet("x", MAINNET_GENESIS).is_err());
        assert!(refuse_mainnet("x", "y").is_ok());
    }

    #[test]
    fn no_wire_type_can_carry_a_key() {
        // A reply can only be one of these, and none holds a secret: this match fails to compile if one is added.
        let reply = Reply::Failed(String::new());
        match reply {
            Reply::Identity { .. }
            | Reply::SwapKey { .. }
            | Reply::Liquid(_)
            | Reply::Solana(_)
            | Reply::Refused(_)
            | Reply::Failed(_) => {}
        }
    }
}
