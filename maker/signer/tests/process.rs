//! The real signer binary, attacked through its socket as a compromised engine would. Uses a regtest node for Liquid
//! and a real Solana cluster, so everything it reads is real.
//!
//!   ELEMENTSD=~/bin/elementsd ELEMENTS_CLI=~/bin/elements-cli SOLANA_RPC=https://api.devnet.solana.com \
//!     cargo test -p liq2sol-signer --test process -- --ignored --nocapture
//!
//! Needs `cli/.keys` from `liq2sol-cli setup`, holding devnet SOL and stand-in tokens. Devnet only.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use liq2sol_chain::liquid::{
    address_of, keypair_from, pay, random32, txout, usdt, use_network, Esplora, NetworkConfig, Rpc,
};
use liq2sol_chain::sol::{self, ata, escrow_pda, vault_pda, Sol};
use liq2sol_htlc::{asset_param, script_hash, Htlc, HtlcParams};
use liq2sol_maker_core::engine::Config as EngineConfig;
use liq2sol_maker_core::hash_preimage;
use liq2sol_maker_core::policy::{Limits, Refusal};
use liq2sol_signer::{unhex, Call, Client, Config, L2sTerms, LiquidSource, Reply, S2lTerms, MAX_REQUEST_BYTES};
use simplicityhl::elements::encode::deserialize;
use simplicityhl::elements::secp256k1_zkp::Keypair as LiquidKey;
use simplicityhl::elements::{AddressParams, AssetId, BlockHash, Script, Transaction as LiquidTx};
use solana_escrow::state::EscrowState;
use solana_sdk::signature::{read_keypair_file, Keypair};
use solana_sdk::signer::Signer as _;
use solana_sdk::transaction::Transaction as SolanaTx;

const SOL_AMOUNT: u64 = 10;
const LIQ_AMOUNT: u64 = SOL_AMOUNT * 100;

struct Node {
    child: Child,
    rpc: Rpc,
    dir: PathBuf,
}

impl Node {
    fn start(root: &Path) -> Self {
        let daemon = std::env::var("ELEMENTSD").unwrap_or_else(|_| "elementsd".into());
        let cli = std::env::var("ELEMENTS_CLI").unwrap_or_else(|_| "elements-cli".into());
        let port = 40_000 + (std::process::id() % 20_000) as u16;
        let dir = root.join("node");
        std::fs::create_dir_all(&dir).unwrap();
        let child = Command::new(daemon)
            .arg(format!("-datadir={}", dir.display()))
            .args(["-chain=liquidregtest", "-listen=0", "-txindex=1", "-validatepegin=0", "-fallbackfee=0.00001"])
            .args(["-initialfreecoins=2100000000", "-anyonecanspendaremine=1", "-evbparams=simplicity:-1:::"])
            .args(["-rpcuser=t", "-rpcpassword=t", &format!("-rpcport={port}"), "-printtoconsole=0"])
            .stdout(Stdio::null())
            .spawn()
            .expect("start elementsd");
        let base: Vec<String> = ["-chain=liquidregtest", "-rpcuser=t", "-rpcpassword=t", &format!("-rpcport={port}")]
            .map(String::from)
            .to_vec();
        let rpc = Rpc { cli, args: base };
        let start = Instant::now();
        while rpc.call(&["getblockcount"]).is_err() {
            assert!(start.elapsed() < Duration::from_secs(60), "elementsd did not start");
            std::thread::sleep(Duration::from_millis(300));
        }
        let node = Node { child, rpc, dir };
        node.rpc.call(&["createwallet", "w"]).unwrap();
        node.rpc.call(&["rescanblockchain"]).unwrap();
        node
    }

    fn mine(&self, blocks: u32) {
        let address = self.rpc.call(&["getnewaddress"]).unwrap();
        self.rpc.call(&["generatetoaddress", &blocks.to_string(), &address]).unwrap();
    }

    fn fund(&self, address: &str, sats: u64, label: &str) {
        let amount = format!("{}.{:08}", sats / 100_000_000, sats % 100_000_000);
        self.rpc
            .call(&["sendtoaddress", address, &amount, "", "", "false", "false", "1", "UNSET", "false", label])
            .unwrap();
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.rpc.call(&["stop"]);
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct SignerProcess {
    child: Child,
    config: PathBuf,
    socket: PathBuf,
}

impl SignerProcess {
    fn start(config: &Path, socket: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_liq2sol-signer")).arg(config).spawn().expect("start signer");
        let start = Instant::now();
        while Client::connect(socket).is_err() {
            assert!(start.elapsed() < Duration::from_secs(30), "signer did not start");
            std::thread::sleep(Duration::from_millis(100));
        }
        Self { child, config: config.into(), socket: socket.into() }
    }

    fn restart(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        *self = Self::start(&self.config.clone(), &self.socket.clone());
    }
}

impl Drop for SignerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn liquid_tx(reply: Reply) -> LiquidTx {
    let Reply::Liquid(hex) = reply else { panic!("expected a Liquid transaction, got {reply:?}") };
    deserialize(&unhex(&hex).unwrap()).unwrap()
}

fn solana_tx(reply: Reply) -> SolanaTx {
    let Reply::Solana(hex) = reply else { panic!("expected a Solana transaction, got {reply:?}") };
    bincode::deserialize(&unhex(&hex).unwrap()).unwrap()
}

#[track_caller]
fn assert_refused(reply: Reply, expected: Refusal) {
    assert_eq!(reply, Reply::Refused(expected.clone()));
    println!("    ok: refused with {expected:?}");
}

#[track_caller]
fn assert_bad_timeout(reply: Reply) {
    assert!(matches!(reply, Reply::Refused(Refusal::BadTimeout(_))), "{reply:?}");
}

struct World {
    node: Node,
    chain: Esplora,
    sol: Sol,
    genesis: BlockHash,
    taker_sol: Keypair,
    maker_pubkey: String,
    maker_script: Vec<u8>,
    taker: LiquidKey,
}

impl World {
    fn send_solana(&self, tx: &SolanaTx) -> Result<(), String> {
        self.sol.rpc.send_and_confirm_transaction(tx).map(|_| ()).map_err(|e| e.to_string())
    }
}

fn hash_and_preimage() -> ([u8; 32], [u8; 32]) {
    let preimage = random32().unwrap();
    (hash_preimage(&preimage), preimage)
}

#[test]
#[ignore = "needs elementsd and funded devnet wallets; see the module docs"]
fn the_signer_process_resists_a_compromised_engine() {
    let root = std::env::temp_dir().join(format!("liq2sol-signer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let private = root.join("private");
    std::fs::create_dir_all(&private).unwrap();
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();

    let node = Node::start(&root);
    node.mine(1);
    let issued: serde_json::Value =
        serde_json::from_str(&node.rpc.call(&["issueasset", "1000", "0", "false"]).unwrap()).unwrap();
    let usdt_id = issued["asset"].as_str().unwrap().to_string();
    node.mine(1);
    let info: serde_json::Value = serde_json::from_str(&node.rpc.call(&["getsidechaininfo"]).unwrap()).unwrap();
    let lbtc_id = info["pegged_asset"].as_str().unwrap().to_string();
    use_network(NetworkConfig {
        params: &AddressParams::ELEMENTS,
        usdt: AssetId::from_str(&usdt_id).unwrap(),
        lbtc: AssetId::from_str(&lbtc_id).unwrap(),
    });
    let chain = Esplora::Node(node.rpc.clone());
    let genesis = BlockHash::from_str(&chain.genesis().unwrap()).unwrap();

    // The maker's funded devnet key becomes the signer's key.
    let keys_src = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../cli/.keys"));
    let keys = root.join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::copy(keys_src.join("maker-solana.json"), keys.join("solana.json")).unwrap();

    let socket = private.join("signer.sock");
    let solana_rpc = std::env::var("SOLANA_RPC").unwrap_or_else(|_| "https://api.devnet.solana.com".into());
    let limits = Limits { extend_after_stall_secs: 3, ..Limits::from(&EngineConfig::default()) };
    let config = Config {
        socket: socket.clone(),
        ledger: root.join("signer.db"),
        keys_dir: keys.clone(),
        liquid: LiquidSource::Node { cli: node.rpc.cli.clone(), args: node.rpc.args.clone() },
        solana_rpc: solana_rpc.clone(),
        limits,
        regtest_assets: Some((usdt_id.clone(), lbtc_id.clone())),
        vault_owner: None,
    };
    let config_path = root.join("signer.json");
    std::fs::write(&config_path, serde_json::to_string(&config).unwrap()).unwrap();

    println!("\n==> Startup checks");
    // A shared directory must be refused, because the socket would be briefly open to everyone.
    let shared = root.join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut open_cfg = config.clone();
    open_cfg.socket = shared.join("signer.sock");
    let open_path = root.join("open.json");
    std::fs::write(&open_path, serde_json::to_string(&open_cfg).unwrap()).unwrap();
    let status =
        Command::new(env!("CARGO_BIN_EXE_liq2sol-signer")).arg(&open_path).stderr(Stdio::null()).status().unwrap();
    assert!(!status.success(), "a socket in a shared directory is refused");
    println!("    ok: refuses a socket in a shared directory");

    let mut signer = SignerProcess::start(&config_path, &socket);
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&socket) & 0o777 & 0o077, 0, "socket private");
    assert_eq!(mode(&keys.join("swap-seed.key")), 0o600);
    assert_eq!(mode(&keys.join("liquid-wallet.key")), 0o600);
    assert_eq!(mode(&keys.join("solana.json")), 0o600);
    println!("    ok: socket and key files are private");

    let mut client = Client::connect(&socket).unwrap();
    let Reply::Identity { liquid_address, liquid_script, solana, .. } = client.call(&Call::Identity).unwrap() else {
        panic!()
    };
    node.fund(&liquid_address, 500_000, "bitcoin");
    node.fund(&liquid_address, 20 * LIQ_AMOUNT, &usdt_id);

    let taker = keypair_from(&random32().unwrap()).unwrap();
    node.fund(&address_of(&taker).to_string(), 100_000, "bitcoin");
    node.fund(&address_of(&taker).to_string(), 20 * LIQ_AMOUNT, &usdt_id);
    node.mine(1);

    let world = World {
        chain,
        sol: Sol::new(&solana_rpc),
        genesis,
        taker_sol: read_keypair_file(keys_src.join("taker-solana.json")).unwrap(),
        maker_pubkey: solana.clone(),
        maker_script: liquid_script.clone(),
        taker,
        node,
    };

    bad_input(&mut client, &socket);
    l2s_attacks(&world, &mut client);
    s2l_attacks(&world, &mut client);

    println!("\n==> Memory survives a restart");
    let (hash, preimage) = s2l_collected(&world, &mut client);
    signer.restart();
    let mut client = Client::connect(&socket).unwrap();
    let t = s2l_terms_for(&world, &hash, &mut client);
    assert_refused(client.call(&Call::RefundLiquid { terms: t }).unwrap(), Refusal::RefundAfterClaim);
    let _ = preimage;
    println!("    ok: the refund is still refused after the signer restarts");

    drop(client);
    drop(signer);
    vault_cap(&world, &config, &root, &private);

    // Stop the node before deleting its directory.
    drop(world);
    let _ = std::fs::remove_dir_all(&root);
    println!("\nALL SIGNER ATTACKS REFUSED");
}

fn bad_input(client: &mut Client, socket: &Path) {
    println!("\n==> Bad input");
    assert!(client.call_raw(b"not json\n").unwrap().contains("malformed"));
    assert!(client.call_raw(b"{\"LockLiquid\":{}}\n").unwrap().contains("malformed"));
    assert!(client.call_raw(b"\xff\xfe\n").unwrap().contains("malformed"));
    println!("    ok: malformed requests get an error");
    let mut big = Client::connect(socket).unwrap();
    let mut huge = vec![b'x'; MAX_REQUEST_BYTES as usize + 10];
    huge.push(b'\n');
    assert!(big.call_raw(&huge).unwrap().contains("too large"));
    println!("    ok: an oversized request is refused");
    // A client that sends half a request and stalls must not block anyone else.
    let mut stalled = std::os::unix::net::UnixStream::connect(socket).unwrap();
    std::io::Write::write_all(&mut stalled, b"{\"LockLiquid\":").unwrap();
    let mut other = Client::connect(socket).unwrap();
    assert!(matches!(other.call(&Call::Identity).unwrap(), Reply::Identity { .. }), "not blocked by a stalled client");
    println!("    ok: a stalled client blocks nobody");
    // Nor can a flood of idle connections take the working one down.
    let flood: Vec<_> = (0..liq2sol_signer::MAX_CONNECTIONS * 2)
        .map(|_| std::os::unix::net::UnixStream::connect(socket).unwrap())
        .collect();
    assert!(
        matches!(client.call(&Call::Identity).unwrap(), Reply::Identity { .. }),
        "an existing connection still works"
    );
    drop(flood);
    println!("    ok: a connection flood doesn't affect an existing connection");
    assert!(matches!(client.call(&Call::Identity).unwrap(), Reply::Identity { .. }), "still serving");
    println!("    ok: the signer is still serving");
}

fn l2s_params(
    world: &World,
    client: &mut Client,
    hash: [u8; 32],
    lock_height: u32,
    liquid_amount: u64,
) -> (L2sTerms, HtlcParams) {
    let Reply::SwapKey { x_only } = client.call(&Call::SwapKey { payment_hash: hash }).unwrap() else { panic!() };
    let taker_pk = world.taker.x_only_public_key().0.serialize();
    let terms = L2sTerms {
        payment_hash: hash,
        taker_pk,
        lock_height,
        liquid_amount,
        solana_amount: SOL_AMOUNT,
        taker_solana: world.taker_sol.pubkey().to_string(),
    };
    let params = HtlcParams {
        payment_hash: hash,
        recipient_pk: x_only,
        sender_pk: taker_pk,
        lock_height,
        usdt_asset: asset_param(&usdt()),
        amount: liquid_amount,
        recipient_script_hash: script_hash(&Script::from(world.maker_script.clone())),
    };
    (terms, params)
}

fn l2s_attacks(world: &World, client: &mut Client) {
    println!("\n==> L to S: locking our funds");
    let tip = world.chain.tip().unwrap();
    let lock_height = tip + 300;
    let (hash, preimage) = hash_and_preimage();
    let (terms, params) = l2s_params(world, client, hash, lock_height, LIQ_AMOUNT);
    let deadline = world.sol.slot().unwrap() + 5_000;
    let lock =
        |c: &mut Client, deadline_slot| c.call(&Call::LockSolana { terms: terms.clone(), deadline_slot }).unwrap();

    // No coin at all: a colluding taker who never paid.
    assert_refused(lock(client, deadline), Refusal::NoTakerCoin);

    // The coin exists, with too few confirmations.
    let htlc = Htlc::new(&params).unwrap().address(&AddressParams::ELEMENTS);
    pay(&world.chain, &world.taker, vec![txout(usdt(), LIQ_AMOUNT, htlc.script_pubkey())], world.genesis).unwrap();
    assert_refused(lock(client, deadline), Refusal::NoTakerCoin);
    world.node.mine(1);
    assert_refused(lock(client, deadline), Refusal::NoTakerCoin);
    world.node.mine(1);

    // The engine lies that the coin covers more than it does, at a loss: the HTLC for those terms holds nothing.
    let (hash2, _) = hash_and_preimage();
    let (cheap, cheap_params) = l2s_params(world, client, hash2, lock_height, LIQ_AMOUNT - 100);
    let cheap_htlc = Htlc::new(&cheap_params).unwrap().address(&AddressParams::ELEMENTS);
    pay(&world.chain, &world.taker, vec![txout(usdt(), LIQ_AMOUNT - 100, cheap_htlc.script_pubkey())], world.genesis)
        .unwrap();
    world.node.mine(2);
    assert_refused(
        client.call(&Call::LockSolana { terms: cheap, deadline_slot: deadline }).unwrap(),
        Refusal::BadPrice,
    );

    // Bad timing.
    assert_bad_timeout(lock(client, deadline + 9_000));

    // Now everything is right. The signer signs, and doesn't send.
    let escrow = escrow_pda(&Pubkey58::parse(&world.maker_pubkey), &hash);
    let tx = solana_tx(lock(client, deadline));
    assert!(world.sol.escrow(&escrow).unwrap().is_none(), "the signer never broadcasts");
    println!("    ok: signed, and not sent");

    // An identical retry is signed. Different terms aren't.
    let _ = solana_tx(lock(client, deadline));
    assert_refused(lock(client, deadline - 1), Refusal::AlreadyLocked);

    world.send_solana(&tx).unwrap();
    let e = world.sol.escrow(&escrow).unwrap().unwrap();
    assert!(e.close_on_restart, "the short leg always closes on restart");
    assert_eq!(e.amount, SOL_AMOUNT);
    assert_eq!(e.recipient, world.taker_sol.pubkey());
    println!("    ok: the real escrow has the expected terms");

    // The claim only takes the right preimage, and always pays our own wallet.
    assert_refused(
        client.call(&Call::ClaimLiquid { terms: terms.clone(), preimage: [9; 32] }).unwrap(),
        Refusal::WrongPreimage,
    );
    world.sol.claim(&world.taker_sol, &escrow, &e, preimage).unwrap();
    let stored = world.sol.escrow(&escrow).unwrap().unwrap().preimage;
    let claim = liquid_tx(client.call(&Call::ClaimLiquid { terms: terms.clone(), preimage: stored }).unwrap());
    assert_eq!(claim.output[0].script_pubkey.to_bytes(), world.maker_script, "the claim pays our wallet");
    world.chain.broadcast(&claim).unwrap();
    world.node.mine(1);
    println!("    ok: the claim paid our wallet, and confirmed");

    // Requests in the wrong direction for this swap.
    assert_refused(
        client
            .call(&Call::ClaimSolana { taker_solana: terms.taker_solana.clone(), payment_hash: hash, preimage })
            .unwrap(),
        Refusal::UnknownSwap,
    );
    assert_refused(
        client.call(&Call::Release { taker_solana: terms.taker_solana.clone(), payment_hash: hash }).unwrap(),
        Refusal::ReleaseAfterLock,
    );
}

/// Base58 parsing, kept in one place.
struct Pubkey58;
impl Pubkey58 {
    fn parse(s: &str) -> anchor_lang::prelude::Pubkey {
        anchor_lang::prelude::Pubkey::from_str(s).unwrap()
    }
}

fn s2l_terms(world: &World, hash: [u8; 32], lock_height: u32, liquid_amount: u64) -> S2lTerms {
    S2lTerms {
        payment_hash: hash,
        taker_pk: world.taker.x_only_public_key().0.serialize(),
        taker_script: address_of(&world.taker).script_pubkey().to_bytes(),
        taker_fee_script: address_of(&world.taker).script_pubkey().to_bytes(),
        lock_height,
        liquid_amount,
        taker_solana: world.taker_sol.pubkey().to_string(),
    }
}

/// Lock terms remembered per hash, so the restart check can rebuild them.
static LAST_LOCK_HEIGHT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn s2l_terms_for(world: &World, hash: &[u8; 32], _client: &mut Client) -> S2lTerms {
    s2l_terms(world, *hash, LAST_LOCK_HEIGHT.load(std::sync::atomic::Ordering::SeqCst), LIQ_AMOUNT)
}

fn taker_lock(
    world: &World,
    hash: [u8; 32],
    recipient: anchor_lang::prelude::Pubkey,
    slots: u64,
    close_on_restart: bool,
    amount: u64,
) -> anchor_lang::prelude::Pubkey {
    let deadline = world.sol.slot().unwrap() + slots;
    world.sol.lock(&world.taker_sol, hash, recipient, amount, deadline, close_on_restart).unwrap()
}

fn s2l_attacks(world: &World, client: &mut Client) {
    println!("\n==> S to L: locking our funds");
    let maker = Pubkey58::parse(&world.maker_pubkey);
    let taker_solana = world.taker_sol.pubkey().to_string();
    let lock_height = || world.chain.tip().unwrap() + 80;

    // No escrow at all.
    let (hash, _) = hash_and_preimage();
    assert_refused(
        client.call(&Call::LockLiquid { terms: s2l_terms(world, hash, lock_height(), LIQ_AMOUNT) }).unwrap(),
        Refusal::NoTakerEscrow,
    );

    // An escrow paying someone else.
    let (hash, _) = hash_and_preimage();
    let other = Keypair::new().pubkey();
    taker_lock(world, hash, other, 40, false, SOL_AMOUNT);
    assert_refused(
        client.call(&Call::LockLiquid { terms: s2l_terms(world, hash, lock_height(), LIQ_AMOUNT) }).unwrap(),
        Refusal::NoTakerEscrow,
    );

    // Too close to its deadline, which the maker can return at once with Release.
    let (hash, _) = hash_and_preimage();
    let short = taker_lock(world, hash, maker, 50_000, false, SOL_AMOUNT);
    assert_refused(
        client.call(&Call::LockLiquid { terms: s2l_terms(world, hash, lock_height(), LIQ_AMOUNT) }).unwrap(),
        Refusal::NoTakerEscrow,
    );
    let release =
        solana_tx(client.call(&Call::Release { taker_solana: taker_solana.clone(), payment_hash: hash }).unwrap());
    world.send_solana(&release).unwrap();
    assert_eq!(world.sol.escrow(&short).unwrap().unwrap().state, EscrowState::Refunded);
    println!("    ok: Release returned an unfilled escrow");

    // A short-leg escrow, which closes on restart, could strand us after a reveal.
    let (hash, _) = hash_and_preimage();
    let restart = taker_lock(world, hash, maker, 101_400, true, SOL_AMOUNT);
    assert_refused(
        client.call(&Call::LockLiquid { terms: s2l_terms(world, hash, lock_height(), LIQ_AMOUNT) }).unwrap(),
        Refusal::NoTakerEscrow,
    );
    let release =
        solana_tx(client.call(&Call::Release { taker_solana: taker_solana.clone(), payment_hash: hash }).unwrap());
    world.send_solana(&release).unwrap();
    assert_eq!(world.sol.escrow(&restart).unwrap().unwrap().state, EscrowState::Refunded);

    // Paying out more than the escrow holds.
    let (hash, _) = hash_and_preimage();
    let under = taker_lock(world, hash, maker, 101_400, false, SOL_AMOUNT - 1);
    assert_refused(
        client.call(&Call::LockLiquid { terms: s2l_terms(world, hash, lock_height(), LIQ_AMOUNT) }).unwrap(),
        Refusal::BadPrice,
    );
    let release =
        solana_tx(client.call(&Call::Release { taker_solana: taker_solana.clone(), payment_hash: hash }).unwrap());
    world.send_solana(&release).unwrap();
    assert_eq!(world.sol.escrow(&under).unwrap().unwrap().state, EscrowState::Refunded);

    // A good one, with the double-payment guard.
    let (hash, preimage) = hash_and_preimage();
    let escrow = taker_lock(world, hash, maker, 101_400, false, SOL_AMOUNT);
    let height = lock_height();
    let terms = s2l_terms(world, hash, height, LIQ_AMOUNT);
    let first = client.call(&Call::LockLiquid { terms: terms.clone() }).unwrap();
    assert_eq!(
        client.call(&Call::LockLiquid { terms: terms.clone() }).unwrap(),
        first,
        "a retry gets the exact same bytes"
    );
    println!("    ok: a retried Liquid lock is byte-identical");
    let different = s2l_terms(world, hash, height + 1, LIQ_AMOUNT);
    assert_refused(client.call(&Call::LockLiquid { terms: different }).unwrap(), Refusal::AlreadyLocked);

    let lock = liquid_tx(first);
    assert_eq!(
        lock.output.iter().filter(|o| o.value.explicit() == Some(liquid_chain_fee())).count(),
        2,
        "one fee coin, one fee"
    );
    world.chain.broadcast(&lock).unwrap();
    world.node.mine(2);

    // Refund timing, and giving back an escrow we've locked against.
    assert_bad_timeout(client.call(&Call::RefundLiquid { terms: terms.clone() }).unwrap());
    assert_refused(
        client.call(&Call::Release { taker_solana: taker_solana.clone(), payment_hash: hash }).unwrap(),
        Refusal::ReleaseAfterLock,
    );

    // Extend: the tip is fresh, so refused, then the chain stalls, so allowed.
    let deadline = world.sol.escrow(&escrow).unwrap().unwrap().deadline_slot;
    let extend = |c: &mut Client, new_deadline| {
        c.call(&Call::Extend { taker_solana: taker_solana.clone(), payment_hash: hash, new_deadline }).unwrap()
    };
    world.node.mine(1);
    assert_refused(extend(client, deadline + 1_000), Refusal::ExtendNotJustified);
    std::thread::sleep(Duration::from_secs(5));
    assert_bad_timeout(extend(client, deadline + 3_000_001));
    let tx = solana_tx(extend(client, deadline + 1_000));
    world.send_solana(&tx).unwrap();
    assert_eq!(world.sol.escrow(&escrow).unwrap().unwrap().deadline_slot, deadline + 1_000);
    println!("    ok: Extend only works during a real stall, and the extension landed");

    // The wrong preimage can't be used to collect.
    assert_refused(
        client
            .call(&Call::ClaimSolana { taker_solana: taker_solana.clone(), payment_hash: hash, preimage: [9; 32] })
            .unwrap(),
        Refusal::WrongPreimage,
    );
    let _ = (preimage, vault_pda(&escrow), ata(&maker));
}

/// A hot key limited by the vault: it locks from the vault under the cap, and a stolen key can't get past it.
fn vault_cap(world: &World, config: &Config, root: &Path, private: &Path) {
    println!("\n==> The maker vault caps a stolen hot key");
    let operator = read_keypair_file(config.keys_dir.join("solana.json")).unwrap();
    // The cold owner is a separate key, which the signer never sees.
    let owner = Keypair::new();
    let mut transfer = vec![2, 0, 0, 0];
    transfer.extend_from_slice(&50_000_000u64.to_le_bytes());
    let fund = solana_sdk::instruction::Instruction {
        program_id: anchor_lang::system_program::ID,
        accounts: vec![
            solana_sdk::instruction::AccountMeta::new(operator.pubkey(), true),
            solana_sdk::instruction::AccountMeta::new(owner.pubkey(), true),
        ],
        data: transfer,
    };
    world.sol.send(&[fund], &operator, &[&owner]).unwrap();
    let cap = 2 * SOL_AMOUNT;
    world.sol.send(&[sol::vault_init_ix(&owner.pubkey(), operator.pubkey(), 1_000_000, cap)], &owner, &[]).unwrap();
    let vault = sol::maker_vault_pda(&owner.pubkey());
    let vault_token = sol::maker_vault_token_pda(&vault);

    let cfg = Config {
        socket: private.join("vault.sock"),
        ledger: root.join("vault-signer.db"),
        vault_owner: Some(owner.pubkey().to_string()),
        ..config.clone()
    };
    let path = root.join("vault.json");
    std::fs::write(&path, serde_json::to_string(&cfg).unwrap()).unwrap();
    let _signer = SignerProcess::start(&path, &cfg.socket);
    let mut client = Client::connect(&cfg.socket).unwrap();
    let Reply::Identity { escrow_initiator, .. } = client.call(&Call::Identity).unwrap() else { panic!() };
    assert_eq!(escrow_initiator, vault.to_string(), "escrows are seeded with the vault");

    // Tokens the hot key collected go back under the cap.
    let deposit = solana_tx(client.call(&Call::DepositToVault { amount: 5 * SOL_AMOUNT }).unwrap());
    world.send_solana(&deposit).unwrap();
    assert_eq!(world.sol.token_balance(&vault_token).unwrap(), 5 * SOL_AMOUNT);
    println!("    ok: collected tokens moved into the vault");

    let lock_once = |client: &mut Client, amount: u64, deadline: u64| {
        let (hash, _) = hash_and_preimage();
        let liquid = amount * 100;
        let (terms, params) = l2s_params(world, client, hash, world.chain.tip().unwrap() + 300, liquid);
        let terms = L2sTerms { solana_amount: amount, ..terms };
        let htlc = Htlc::new(&params).unwrap().address(&AddressParams::ELEMENTS);
        pay(&world.chain, &world.taker, vec![txout(usdt(), liquid, htlc.script_pubkey())], world.genesis).unwrap();
        world.node.mine(2);
        (client.call(&Call::LockSolana { terms, deadline_slot: deadline }).unwrap(), hash)
    };

    let deadline = world.sol.slot().unwrap() + 2_000;
    let (reply, hash) = lock_once(&mut client, SOL_AMOUNT, deadline);
    world.send_solana(&solana_tx(reply)).unwrap();
    let e = world.sol.escrow(&sol::escrow_pda(&vault, &hash)).unwrap().unwrap();
    assert_eq!((e.initiator, e.refund_token, e.rent_payer), (vault, vault_token, operator.pubkey()));
    assert_eq!(world.sol.token_balance(&vault_token).unwrap(), 4 * SOL_AMOUNT);
    println!("    ok: the lock drew from the vault, and a refund would return there");

    // The signer's own limits would allow this. The chain doesn't: this is the protection against a stolen key.
    let (reply, _) = lock_once(&mut client, cap, deadline);
    let err = world.send_solana(&solana_tx(reply)).unwrap_err();
    // 0x177c is 6012, the program's OverWindowCap. Any other failure wouldn't prove the cap did this.
    assert!(err.contains("0x177c"), "refused for the wrong reason: {err}");
    println!("    ok: a lock over the window cap is refused on-chain (OverWindowCap), even with a valid signature");
    assert_eq!(world.sol.token_balance(&vault_token).unwrap(), 4 * SOL_AMOUNT, "nothing moved");

    // The hot key can't withdraw: it isn't the owner, so the vault's address doesn't even derive from it.
    let withdraw = solana_sdk::instruction::Instruction {
        program_id: solana_escrow::id(),
        accounts: anchor_lang::ToAccountMetas::to_account_metas(
            &solana_escrow::accounts::VaultWithdraw {
                owner: operator.pubkey(),
                maker_vault: vault,
                vault_token,
                to: sol::ata(&operator.pubkey()),
                token_program: Pubkey58::parse("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
            },
            None,
        ),
        data: anchor_lang::InstructionData::data(&solana_escrow::instruction::VaultWithdraw { amount: 1 }),
    };
    assert!(world.sol.send(&[withdraw], &operator, &[]).is_err(), "the hot key can't withdraw");
    println!("    ok: the hot key can't withdraw from the vault");
}

fn liquid_chain_fee() -> u64 {
    liq2sol_chain::liquid::FEE
}

/// Collect on Solana, then try to take the Liquid coin back too.
fn s2l_collected(world: &World, client: &mut Client) -> ([u8; 32], [u8; 32]) {
    let maker = Pubkey58::parse(&world.maker_pubkey);
    let taker_solana = world.taker_sol.pubkey().to_string();
    let (hash, preimage) = hash_and_preimage();
    let escrow = taker_lock(world, hash, maker, 101_400, false, SOL_AMOUNT);
    let height = world.chain.tip().unwrap() + 40;
    LAST_LOCK_HEIGHT.store(height, std::sync::atomic::Ordering::SeqCst);
    let terms = s2l_terms(world, hash, height, LIQ_AMOUNT);
    let lock = liquid_tx(client.call(&Call::LockLiquid { terms: terms.clone() }).unwrap());
    world.chain.broadcast(&lock).unwrap();
    world.node.mine(2);

    let claim = solana_tx(client.call(&Call::ClaimSolana { taker_solana, payment_hash: hash, preimage }).unwrap());
    world.send_solana(&claim).unwrap();
    assert_eq!(world.sol.escrow(&escrow).unwrap().unwrap().state, EscrowState::Claimed);

    world.node.mine(45);
    assert!(world.chain.tip().unwrap() >= height, "past the lock height");
    assert_refused(client.call(&Call::RefundLiquid { terms }).unwrap(), Refusal::RefundAfterClaim);
    println!("    ok: after collecting on Solana, the refund is refused, even past the lock height");
    (hash, preimage)
}
