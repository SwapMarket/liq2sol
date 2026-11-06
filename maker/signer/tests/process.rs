//! The real signer binary, attacked through its socket as a compromised engine would. Uses a regtest node for Liquid
//! and a real Solana cluster, so everything it reads is real.
//!
//!   ELEMENTSD=~/bin/elementsd ELEMENTS_CLI=~/bin/elements-cli SOLANA_RPC=https://api.devnet.solana.com \
//!     cargo test -p liq2sol-signer --test process -- --ignored --nocapture
//!
//! Needs `cli/.keys` from `liq2sol-cli setup`, holding SOL and stand-in tokens on that cluster. Never mainnet.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use liq2sol_chain::esplora::{build_confidential_pay, Esplora, Rpc};
use liq2sol_chain::liquid::{
    build_swap_spend, exact_secret_coin, is_key_path_spend, keypair_from, lbtc, published_signature, random32, usdt,
    use_network, NetworkConfig, Payment, SecretCoin, Wallet, FEE,
};
use liq2sol_chain::sol::{self, escrow_pda, Sol};
use liq2sol_chain::swap::{answer_claim, TakerClaim};
use liq2sol_maker_core::engine::{Config as EngineConfig, Direction};
use liq2sol_maker_core::policy::{Limits, Refusal};
use liq2sol_maker_core::LockPoint;
use liq2sol_signer::{unhex, Call, Client, Config, LiquidSource, Reply, SwapTerms, MAX_REQUEST_BYTES};
use liq2sol_swapkey::{
    nonce_bytes, nonce_from, point_of, secret_from_solana, share_bytes, share_from, solana_point_of, solana_value,
    Nonce,
};
use simplicityhl::elements::encode::{deserialize, serialize_hex};
use simplicityhl::elements::secp256k1_zkp::SecretKey;
use simplicityhl::elements::{
    Address, AddressParams, AssetId, BlockHash, OutPoint, Script, Sequence, Transaction as LiquidTx,
};
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
            .arg("-acceptdiscountct=1")
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
        fee_coin_sats: FEE,
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
    assert_eq!(mode(&keys.join("liquid-blinding.key")), 0o600);
    assert_eq!(mode(&keys.join("solana.json")), 0o600);
    println!("    ok: socket and key files are private");

    let mut client = Client::connect(&socket).unwrap();
    let Reply::Identity { liquid_address, solana, .. } = client.call(&Call::Identity).unwrap() else { panic!() };
    node.fund(&liquid_address, 500_000, "bitcoin");
    node.fund(&liquid_address, 30 * LIQ_AMOUNT, &usdt_id);

    let taker = Wallet {
        key: keypair_from(&random32().unwrap()).unwrap(),
        blinding: SecretKey::from_slice(&random32().unwrap()).unwrap(),
    };
    node.fund(&taker.address().to_string(), 500_000, "bitcoin");
    node.fund(&taker.address().to_string(), 30 * LIQ_AMOUNT, &usdt_id);
    node.mine(1);

    let world = World {
        chain,
        sol: Sol::new(&solana_rpc),
        genesis,
        taker_sol: read_keypair_file(keys_src.join("taker-solana.json")).unwrap(),
        maker_pubkey: solana.clone(),
        maker_script: Address::from_str(&liquid_address).unwrap().script_pubkey(),
        maker_address: liquid_address.clone(),
        taker,
        node,
    };

    bad_input(&mut client, &socket);
    let reserved = l2s_attacks(&world, &mut client);
    s2l_attacks(&world, &mut client, reserved);
    s2l_refund(&world, &mut client);
    assert!(
        world.chain.spender(&reserved).unwrap().is_none(),
        "after every payment above, the fee coin promised to a stored signature is still unspent"
    );
    println!("    ok: a reserved fee coin survives all of the wallet's payments");

    println!("\n==> Memory survives a restart");
    let terms = s2l_collected(&world, &mut client);
    signer.restart();
    let mut client = Client::connect(&socket).unwrap();
    assert_refused(client.call(&Call::RefundLiquid { terms }).unwrap(), Refusal::RefundAfterClaim);
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

struct World {
    node: Node,
    chain: Esplora,
    sol: Sol,
    genesis: BlockHash,
    taker_sol: Keypair,
    maker_pubkey: String,
    maker_script: Script,
    maker_address: String,
    taker: Wallet,
}

impl World {
    fn send_solana(&self, tx: &SolanaTx) -> Result<(), String> {
        self.sol.rpc.send_and_confirm_transaction(tx).map(|_| ()).map_err(|e| e.to_string())
    }

    fn terms(&self, direction: Direction, point: LockPoint, lock_height: u32, liquid: u64, solana: u64) -> SwapTerms {
        SwapTerms {
            direction,
            lock_point: point,
            taker_key: LockPoint(self.taker.key.public_key().serialize()),
            blinding_key: random32().unwrap(),
            lock_height,
            liquid_amount: liquid,
            solana_amount: solana,
            taker_solana: self.taker_sol.pubkey().to_string(),
        }
    }

    /// Pay a confidential address from the taker's wallet, and mine, so the next payment sees the change.
    fn pay(&self, address: &Address, asset: AssetId, amount: u64) {
        let payment = Payment { asset, amount, address: address.clone() };
        let tx = build_confidential_pay(&self.chain, &self.taker, vec![payment], self.genesis).unwrap();
        self.chain.broadcast(&tx).unwrap();
        self.node.mine(1);
    }

    fn top_up(&self, client: &mut Client) {
        if let Reply::Liquid(hex) = client.call(&Call::TopUpFeeCoins).unwrap() {
            let tx: LiquidTx = deserialize(&unhex(&hex).unwrap()).unwrap();
            self.chain.broadcast(&tx).unwrap();
            self.node.mine(1);
        }
    }
}

fn maker_key(client: &mut Client, id: [u8; 32]) -> LockPoint {
    let Reply::SwapKey { key } = client.call(&Call::SwapKey { swap_id: id }).unwrap() else { panic!() };
    key
}

fn fresh_point() -> ([u8; 32], LockPoint) {
    let secret = random32().unwrap();
    (secret, LockPoint(point_of(&secret).unwrap()))
}

fn point_64(point: &LockPoint) -> [u8; 64] {
    solana_point_of(&point.0).unwrap()
}

/// The same x with the other y. Its swap id is identical, and only the holder of the secret could use it.
fn flipped(point: &LockPoint) -> LockPoint {
    let mut other = point.0;
    other[0] = if other[0] == 2 { 3 } else { 2 };
    LockPoint(other)
}

fn template(reply: Reply) -> (LiquidTx, Nonce) {
    let Reply::ClaimTemplate { tx, nonce } = reply else { panic!("expected a claim template, got {reply:?}") };
    (deserialize(&unhex(&tx).unwrap()).unwrap(), nonce_from(&unhex(&nonce).unwrap()).unwrap())
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn swap_coin(world: &World, terms: &SwapTerms, key: &LockPoint) -> SecretCoin {
    let address = terms.address(key).unwrap();
    world.chain.secret_coins(&address, &terms.blinding().unwrap()).unwrap().remove(0)
}

/// The taker's whole L→S side: pay, confirm, and answer the maker's claim template. Leaves an adapter signature stored.
fn l2s_ready(world: &World, client: &mut Client, terms: &SwapTerms) -> LiquidTx {
    let key = maker_key(client, terms.swap_id());
    world.pay(&terms.address(&key).unwrap(), usdt(), terms.liquid_amount);
    world.node.mine(1);
    let coin = swap_coin(world, terms, &key);
    let (tx, nonce) = template(client.call(&Call::PrepareClaim { terms: terms.clone() }).unwrap());
    let (taker_nonce, share) = answer_claim(
        &world.chain,
        &world.taker.key,
        &terms.output(&key).unwrap(),
        &coin.coin.outpoint,
        &terms.lock_point.0,
        &nonce,
        &tx,
        world.genesis,
    )
    .unwrap();
    let reply = client
        .call(&Call::StoreAdaptor {
            terms: terms.clone(),
            taker_nonce: hex_of(&nonce_bytes(&taker_nonce)),
            taker_share: hex_of(&share_bytes(&share)),
        })
        .unwrap();
    assert_eq!(reply, Reply::Held(true));
    tx
}

/// An L→S swap holding an adapter signature that hasn't been used. Its fee coin must survive anything the wallet pays.
fn reserved_fee_coin(world: &World, client: &mut Client) -> OutPoint {
    let (_, point) = fresh_point();
    let terms = world.terms(Direction::L2S, point, world.chain.tip().unwrap() + 300, LIQ_AMOUNT, SOL_AMOUNT);
    l2s_ready(world, client, &terms).input[1].previous_output
}

fn l2s_attacks(world: &World, client: &mut Client) -> OutPoint {
    println!("\n==> L to S: asking for a signing session");
    let tip = world.chain.tip().unwrap();
    let lock_height = tip + 300;
    let (secret, point) = fresh_point();
    let id = point.id();
    let terms = world.terms(Direction::L2S, point, lock_height, LIQ_AMOUNT, SOL_AMOUNT);
    let key = maker_key(client, id);
    let address = terms.address(&key).unwrap();
    let prepare = |c: &mut Client, t: &SwapTerms| c.call(&Call::PrepareClaim { terms: t.clone() }).unwrap();

    // A colluding taker who never paid can't get a session, or a fee coin reserved.
    assert_refused(prepare(client, &terms), Refusal::NoTakerCoin);

    // A coin of the wrong asset at the right address. The chain has no opinion: only unblinding can tell.
    world.pay(&address, lbtc(), LIQ_AMOUNT);
    world.node.mine(1);
    assert_refused(prepare(client, &terms), Refusal::NoTakerCoin);
    println!("    ok: a coin of another asset at the swap address is refused");

    // A coin of the wrong amount.
    let (_, short_point) = fresh_point();
    let short = world.terms(Direction::L2S, short_point, lock_height, LIQ_AMOUNT, SOL_AMOUNT);
    let short_key = maker_key(client, short_point.id());
    world.pay(&short.address(&short_key).unwrap(), usdt(), LIQ_AMOUNT - 1);
    world.node.mine(1);
    assert_refused(prepare(client, &short), Refusal::NoTakerCoin);
    println!("    ok: a coin one unit short is refused");

    // The right coin, too new, and then read with the wrong blinding key.
    world.pay(&address, usdt(), LIQ_AMOUNT);
    assert_refused(prepare(client, &terms), Refusal::NoTakerCoin);
    println!("    ok: a coin with too few confirmations is refused");
    world.node.mine(1);
    let unreadable = SwapTerms { blinding_key: random32().unwrap(), ..terms.clone() };
    assert_refused(prepare(client, &unreadable), Refusal::NoTakerCoin);
    println!("    ok: a coin that can't be read counts as no coin");

    // Anyone can send the wallet an explicit coin of a fee coin's value. It must not be taken for one: spending it would show
    // an input's value, and bound the blinded output.
    let plain = Address::from_str(&world.maker_address).unwrap().to_unconfidential();
    world.node.fund(&plain.to_string(), FEE, "bitcoin");
    world.node.mine(1);

    // No fee coin confirmed yet, so no session: a claim needs one reserved first.
    assert!(matches!(prepare(client, &terms), Reply::Failed(e) if e.contains("fee coin")));
    println!("    ok: no session starts without a confirmed fee coin");
    let top_up = client.call(&Call::TopUpFeeCoins).unwrap();
    assert_eq!(
        client.call(&Call::TopUpFeeCoins).unwrap(),
        top_up,
        "a retry sends the same bytes, and doesn't pay twice"
    );
    println!("    ok: a fee coin top-up is idempotent");
    world.chain.broadcast(&liquid_tx(top_up)).unwrap();
    world.node.mine(1);
    assert_eq!(client.call(&Call::TopUpFeeCoins).unwrap(), Reply::Nothing, "the pool is full");

    println!("\n==> L to S: locking our funds");
    let deadline = world.sol.slot().unwrap() + 5_000;
    let lock =
        |c: &mut Client, deadline_slot| c.call(&Call::LockSolana { terms: terms.clone(), deadline_slot }).unwrap();

    // The coin is good, but with no adapter signature we'd have no way to collect it.
    assert_refused(lock(client, deadline), Refusal::NoAdaptor);
    println!("    ok: no lock without a stored adapter signature");

    let coins = world.chain.secret_coins(&address, &terms.blinding().unwrap()).unwrap();
    let coin = exact_secret_coin(&coins, usdt(), LIQ_AMOUNT, world.chain.tip().unwrap(), 2).unwrap();
    let output = terms.output(&key).unwrap();
    let answer = |tx: &LiquidTx, nonce: &Nonce, wrong_point: Option<[u8; 33]>| {
        let at = wrong_point.unwrap_or(terms.lock_point.0);
        answer_claim(&world.chain, &world.taker.key, &output, &coin.coin.outpoint, &at, nonce, tx, world.genesis)
            .unwrap()
    };

    // A share locked to another point would leave the maker unable to finish, after it has locked.
    let (tx, nonce) = template(prepare(client, &terms));
    let (other_nonce, other_share) = answer(&tx, &nonce, Some(point_of(&random32().unwrap()).unwrap()));
    let store = |nonce: &Nonce, share| Call::StoreAdaptor {
        terms: terms.clone(),
        taker_nonce: hex_of(&nonce_bytes(nonce)),
        taker_share: hex_of(&share_bytes(&share)),
    };
    assert!(matches!(client.call(&store(&other_nonce, other_share)).unwrap(), Reply::Failed(_)));
    assert_eq!(client.call(&Call::AdaptorHeld { swap_id: id }).unwrap(), Reply::Held(false));
    assert_refused(lock(client, deadline), Refusal::NoAdaptor);
    println!("    ok: a share for another point is refused, and nothing is stored");

    // That session is over, so its nonce can't sign again.
    assert!(
        matches!(client.call(&store(&other_nonce, other_share)).unwrap(), Reply::Failed(e) if e.contains("session"))
    );
    println!("    ok: a used session can't be used again");

    // Malformed answers.
    let _ = template(prepare(client, &terms));
    let junk = Call::StoreAdaptor { terms: terms.clone(), taker_nonce: "00".repeat(66), taker_share: "ff".repeat(32) };
    assert!(matches!(client.call(&junk).unwrap(), Reply::Failed(_)));
    println!("    ok: malformed nonces and shares are refused");

    // Now a real one.
    let (tx, nonce) = template(prepare(client, &terms));
    let fee_outpoint = tx.input[1].previous_output;
    let (good_nonce, good_share) = answer(&tx, &nonce, None);
    assert_eq!(client.call(&store(&good_nonce, good_share)).unwrap(), Reply::Held(true));
    assert_eq!(client.call(&Call::AdaptorHeld { swap_id: id }).unwrap(), Reply::Held(true));
    println!("    ok: a real share is checked and stored");
    assert_eq!(prepare(client, &terms), Reply::Nothing, "no second session once one is stored");
    assert!(matches!(client.call(&store(&good_nonce, good_share)).unwrap(), Reply::Refused(_)));
    println!("    ok: a stored adapter signature can't be replaced");

    // The engine lies that the coin covers more than it does, at a loss.
    let (_, cheap_point) = fresh_point();
    let cheap = world.terms(Direction::L2S, cheap_point, lock_height, LIQ_AMOUNT - 100, SOL_AMOUNT);
    l2s_ready(world, client, &cheap);
    assert_refused(
        client.call(&Call::LockSolana { terms: cheap, deadline_slot: deadline }).unwrap(),
        Refusal::BadPrice,
    );

    // Bad timing.
    assert_bad_timeout(lock(client, deadline + 9_000));

    // Now everything is right. The signer signs, and doesn't send.
    let escrow = escrow_pda(&Pubkey58::parse(&world.maker_pubkey), &point_64(&point));
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
    assert_eq!(e.lock_point, point_64(&point));
    println!("    ok: the real escrow has the expected terms");

    // The claim only takes the right secret, and always pays our own wallet.
    assert_refused(
        client.call(&Call::ClaimLiquid { terms: terms.clone(), secret: [9; 32] }).unwrap(),
        Refusal::WrongSecret,
    );
    world.sol.claim(&world.taker_sol, &escrow, &e, solana_value(&secret).unwrap()).unwrap();
    let revealed = world.sol.escrow(&escrow).unwrap().unwrap().revealed;
    let learned = secret_from_solana(&revealed).unwrap();
    assert_eq!(learned, secret, "the secret comes out of what the claim revealed");
    let claim = liquid_tx(client.call(&Call::ClaimLiquid { terms: terms.clone(), secret: learned }).unwrap());
    assert_eq!(claim.output[0].script_pubkey, world.maker_script, "the claim pays our wallet");
    assert!(!claim.output[0].asset.is_explicit() && !claim.output[0].value.is_explicit(), "and hides what it pays");
    assert_eq!(claim.input[1].previous_output, fee_outpoint, "using the fee coin it reserved");
    world.chain.broadcast(&claim).unwrap();
    world.node.mine(1);
    let mined = world.chain.tx(&claim.txid()).unwrap();
    assert_eq!(is_key_path_spend(&mined, &coin.coin.outpoint), Some(true), "one signature on-chain, with no script");
    println!("    ok: the claim paid our wallet, confirmed, and looks like a plain key-path spend");

    // Requests in the wrong direction for this swap.
    assert_refused(
        client
            .call(&Call::ClaimSolana { taker_solana: terms.taker_solana.clone(), lock_point: point, secret })
            .unwrap(),
        Refusal::UnknownSwap,
    );
    assert_refused(
        client.call(&Call::Release { taker_solana: terms.taker_solana.clone(), lock_point: point }).unwrap(),
        Refusal::ReleaseAfterLock,
    );

    world.top_up(client);
    reserved_fee_coin(world, client)
}

/// Base58 parsing, kept in one place.
struct Pubkey58;
impl Pubkey58 {
    fn parse(s: &str) -> anchor_lang::prelude::Pubkey {
        anchor_lang::prelude::Pubkey::from_str(s).unwrap()
    }
}

fn taker_lock(
    world: &World,
    point: &LockPoint,
    recipient: anchor_lang::prelude::Pubkey,
    slots: u64,
    close_on_restart: bool,
    amount: u64,
) -> anchor_lang::prelude::Pubkey {
    let deadline = world.sol.slot().unwrap() + slots;
    world.sol.lock(&world.taker_sol, point_64(point), recipient, amount, deadline, close_on_restart).unwrap()
}

fn s2l_attacks(world: &World, client: &mut Client, reserved: OutPoint) {
    println!("\n==> S to L: locking our funds");
    let maker = Pubkey58::parse(&world.maker_pubkey);
    let taker_solana = world.taker_sol.pubkey().to_string();
    let lock_height = || world.chain.tip().unwrap() + 80;
    let terms_for = |point, height| world.terms(Direction::S2L, point, height, LIQ_AMOUNT, SOL_AMOUNT);
    let lock_call = |t: &SwapTerms| Call::LockLiquid { terms: t.clone() };
    let release = |c: &mut Client, lock_point| {
        solana_tx(c.call(&Call::Release { taker_solana: taker_solana.clone(), lock_point }).unwrap())
    };

    // No escrow at all.
    let (_, point) = fresh_point();
    assert_refused(client.call(&lock_call(&terms_for(point, lock_height()))).unwrap(), Refusal::NoTakerEscrow);

    // An escrow paying someone else.
    let (_, point) = fresh_point();
    let other = Keypair::new().pubkey();
    taker_lock(world, &point, other, 40, false, SOL_AMOUNT);
    assert_refused(client.call(&lock_call(&terms_for(point, lock_height()))).unwrap(), Refusal::NoTakerEscrow);

    // Too close to its deadline, which the maker can return at once with Release.
    let (_, point) = fresh_point();
    let short = taker_lock(world, &point, maker, 50_000, false, SOL_AMOUNT);
    assert_refused(client.call(&lock_call(&terms_for(point, lock_height()))).unwrap(), Refusal::NoTakerEscrow);
    world.send_solana(&release(client, point)).unwrap();
    assert_eq!(world.sol.escrow(&short).unwrap().unwrap().state, EscrowState::Refunded);
    println!("    ok: Release returned an unfilled escrow");

    // A short-leg escrow, which closes on restart, could strand us after a reveal.
    let (_, point) = fresh_point();
    let restart = taker_lock(world, &point, maker, 101_400, true, SOL_AMOUNT);
    assert_refused(client.call(&lock_call(&terms_for(point, lock_height()))).unwrap(), Refusal::NoTakerEscrow);
    world.send_solana(&release(client, point)).unwrap();
    assert_eq!(world.sol.escrow(&restart).unwrap().unwrap().state, EscrowState::Refunded);

    // Paying out more than the escrow holds.
    let (_, point) = fresh_point();
    let under = taker_lock(world, &point, maker, 101_400, false, SOL_AMOUNT - 1);
    assert_refused(client.call(&lock_call(&terms_for(point, lock_height()))).unwrap(), Refusal::BadPrice);
    world.send_solana(&release(client, point)).unwrap();
    assert_eq!(world.sol.escrow(&under).unwrap().unwrap().state, EscrowState::Refunded);

    // A good one, with the double-payment guard.
    let (secret, point) = fresh_point();
    let id = point.id();
    let escrow = taker_lock(world, &point, maker, 101_400, false, SOL_AMOUNT);
    let height = lock_height();
    let terms = terms_for(point, height);
    let key = maker_key(client, id);

    // Nothing to sign for yet: an empty ledger and no coin of ours.
    let early = Call::OpenSession { terms: terms.clone(), claim_tx: "00".into(), taker_nonce: "00".into() };
    assert_refused(client.call(&early).unwrap(), Refusal::UnknownSwap);
    println!("    ok: no signing session before we've locked");

    let first = client.call(&lock_call(&terms)).unwrap();
    assert_eq!(client.call(&lock_call(&terms)).unwrap(), first, "a retry gets the exact same bytes");
    println!("    ok: a retried Liquid lock is byte-identical");
    let different = SwapTerms { lock_height: height + 1, ..terms.clone() };
    assert_refused(client.call(&lock_call(&different)).unwrap(), Refusal::AlreadyLocked);

    let lock = liquid_tx(first);
    assert!(lock.output.iter().filter(|o| !o.is_fee()).all(|o| !o.asset.is_explicit() && !o.value.is_explicit()));
    assert!(lock.input.iter().all(|i| i.previous_output != reserved), "a coin promised to a signature stays out");
    println!("    ok: everything but the fee is hidden, and a reserved fee coin isn't touched");
    world.chain.broadcast(&lock).unwrap();
    world.node.mine(2);
    let coin = swap_coin(world, &terms, &key);
    assert_eq!((coin.secrets.asset, coin.secrets.value), (usdt(), LIQ_AMOUNT));
    let fee_address = terms.taker_fee_address().unwrap();
    let fee_coin = world.chain.secret_coins(&fee_address, &terms.blinding().unwrap()).unwrap().remove(0);
    assert_eq!((fee_coin.secrets.asset, fee_coin.secrets.value), (lbtc(), FEE));
    println!("    ok: the taker can read its coin and its fee coin, with exact amounts");

    // Refund timing, and giving back an escrow we've locked against.
    coins_are_never_reused(world, client);
    assert_bad_timeout(client.call(&Call::RefundLiquid { terms: terms.clone() }).unwrap());
    assert_refused(
        client.call(&Call::Release { taker_solana: taker_solana.clone(), lock_point: point }).unwrap(),
        Refusal::ReleaseAfterLock,
    );

    println!("\n==> S to L: the signing session");
    let output = terms.output(&key).unwrap();
    let to = world.taker.address();
    let open = |c: &mut Client, tx: &LiquidTx, nonce: &Nonce| {
        c.call(&Call::OpenSession {
            terms: terms.clone(),
            claim_tx: serialize_hex(tx),
            taker_nonce: hex_of(&nonce_bytes(nonce)),
        })
        .unwrap()
    };
    let start = || {
        let (tx, _) = build_swap_spend(&coin, &fee_coin, &to, 0, Sequence::MAX).unwrap();
        TakerClaim::start(&world.chain, &world.taker.key, &output, tx, world.genesis).unwrap()
    };
    let got_nonce = |reply: Reply| {
        let Reply::Nonce(hex) = reply else { panic!("expected a nonce, got {reply:?}") };
        nonce_from(&unhex(&hex).unwrap()).unwrap()
    };

    // A claim that doesn't spend this swap's coin.
    let (mut wrong, _) = build_swap_spend(&coin, &fee_coin, &to, 0, Sequence::MAX).unwrap();
    wrong.input.swap(0, 1);
    let nonce = start().nonce();
    assert!(matches!(open(client, &wrong, &nonce), Reply::Failed(e) if e.contains("input 0")));
    println!("    ok: a claim that doesn't spend the swap coin first is refused");

    // A share for another point: the maker couldn't read the secret from the published signature.
    let claim = start();
    let maker_nonce = got_nonce(open(client, &claim.tx.clone(), &claim.nonce()));
    let (bad, _) =
        claim.sign(&world.taker.key, &output, &point_of(&random32().unwrap()).unwrap(), &maker_nonce).unwrap();
    let finish = |c: &mut Client, t: &SwapTerms, share: &str| {
        c.call(&Call::FinishSession { terms: t.clone(), taker_share: share.to_string() }).unwrap()
    };
    assert!(matches!(finish(client, &terms, &hex_of(&share_bytes(&bad))), Reply::Failed(_)));
    println!("    ok: a share for another point gets no share back");
    assert!(
        matches!(finish(client, &terms, &hex_of(&share_bytes(&bad))), Reply::Failed(e) if e.contains("session")),
        "and that session is gone"
    );

    // Other terms for the same swap id would mean signing with this nonce under another key context.
    let claim = start();
    let maker_nonce = got_nonce(open(client, &claim.tx.clone(), &claim.nonce()));
    let (share, _) = claim.sign(&world.taker.key, &output, &point.0, &maker_nonce).unwrap();
    let moved = SwapTerms { lock_height: terms.lock_height + 1, ..terms.clone() };
    assert!(
        matches!(finish(client, &moved, &hex_of(&share_bytes(&share))), Reply::Failed(e) if e.contains("differ")),
        "a session finishes under the terms it was opened with, or not at all"
    );
    println!("    ok: a session can't be finished under different terms");

    // The same x with the other y: same id, but it isn't the escrow's point.
    let claim = start();
    let maker_nonce = got_nonce(open(client, &claim.tx.clone(), &claim.nonce()));
    let (share, signed) = claim.sign(&world.taker.key, &output, &point.0, &maker_nonce).unwrap();
    let flip = SwapTerms { lock_point: flipped(&point), ..terms.clone() };
    assert_refused(finish(client, &flip, &hex_of(&share_bytes(&share))), Refusal::NoTakerEscrow);
    println!("    ok: a point with the same x and the other y is refused");

    // Now a real one. Our share only comes after the taker's has been checked and stored.
    let Reply::Share(given) = finish(client, &terms, &hex_of(&share_bytes(&share))) else { panic!() };
    assert_eq!(finish(client, &terms, &hex_of(&share_bytes(&share))), Reply::Share(given.clone()), "resent unchanged");
    println!("    ok: our share is given once, and resent unchanged if the reply was lost");
    let again = start();
    assert_refused(open(client, &again.tx.clone(), &again.nonce()), Refusal::AdaptorAlreadyHeld);
    println!("    ok: no second session, so the only signature the taker can get is the one we can read");

    let maker_share = share_from(&unhex(&given).unwrap()).unwrap();
    let claim_tx =
        signed.finish(&world.taker.key, &output, &key.0, maker_share, &point.0, &secret, world.genesis).unwrap();
    let published = published_signature(&claim_tx, &coin.coin.outpoint).unwrap();

    // Extend: the tip is fresh, so refused, then the chain stalls, so allowed.
    let deadline = world.sol.escrow(&escrow).unwrap().unwrap().deadline_slot;
    let extend = |c: &mut Client, new_deadline| {
        c.call(&Call::Extend { taker_solana: taker_solana.clone(), lock_point: point, new_deadline }).unwrap()
    };
    world.node.mine(1);
    assert_refused(extend(client, deadline + 1_000), Refusal::ExtendNotJustified);
    std::thread::sleep(Duration::from_secs(5));
    assert_bad_timeout(extend(client, deadline + 3_000_001));
    let tx = solana_tx(extend(client, deadline + 1_000));
    world.send_solana(&tx).unwrap();
    assert_eq!(world.sol.escrow(&escrow).unwrap().unwrap().deadline_slot, deadline + 1_000);
    println!("    ok: Extend only works during a real stall, and the extension landed");

    // The taker claims, and we read the secret out of its signature.
    world.chain.broadcast(&claim_tx).unwrap();
    world.node.mine(1);
    let mined = world.chain.tx(&claim_tx.txid()).unwrap();
    assert_eq!(is_key_path_spend(&mined, &coin.coin.outpoint), Some(true));
    let extract = |c: &mut Client, sig: &[u8]| {
        c.call(&Call::ExtractSecret { lock_point: point, signature: hex_of(sig) }).unwrap()
    };
    assert_eq!(extract(client, &published), Reply::Secret(secret));
    println!("    ok: the secret comes out of the published signature");
    assert!(matches!(extract(client, &[7; 64]), Reply::Failed(_)), "a made-up signature gives nothing");

    // The wrong secret can't be used to collect, and the right one can.
    assert_refused(
        client
            .call(&Call::ClaimSolana { taker_solana: taker_solana.clone(), lock_point: point, secret: [9; 32] })
            .unwrap(),
        Refusal::WrongSecret,
    );
    let collect = solana_tx(
        client.call(&Call::ClaimSolana { taker_solana: taker_solana.clone(), lock_point: point, secret }).unwrap(),
    );
    world.send_solana(&collect).unwrap();
    assert_eq!(world.sol.escrow(&escrow).unwrap().unwrap().state, EscrowState::Claimed);
    println!("    ok: collected on Solana with the secret from Liquid");
}

/// Two locks with no block between them. The node's coin list still shows the first one's coins as unspent, and a second lock
/// signed over them could never land, because retries must send the same bytes.
fn coins_are_never_reused(world: &World, client: &mut Client) {
    let maker = Pubkey58::parse(&world.maker_pubkey);
    let mut seen: Vec<OutPoint> = vec![];
    for _ in 0..2 {
        let (_, point) = fresh_point();
        taker_lock(world, &point, maker, 101_400, false, SOL_AMOUNT);
        let height = world.chain.tip().unwrap() + 80;
        let terms = world.terms(Direction::S2L, point, height, LIQ_AMOUNT, SOL_AMOUNT);
        let call = Call::LockLiquid { terms };
        let first = client.call(&call).unwrap();
        // Short of unused coins until a block confirms the change. Then it is signed, and it must still not overlap.
        let reply = match first {
            Reply::Failed(e) if e.contains("wallet") => {
                world.node.mine(1);
                client.call(&call).unwrap()
            }
            other => other,
        };
        let tx = liquid_tx(reply);
        for input in &tx.input {
            assert!(!seen.contains(&input.previous_output), "a signed payment's coin was used again");
            seen.push(input.previous_output);
        }
        world.chain.broadcast(&tx).expect("each lock can land");
    }
    world.node.mine(1);
    println!("    ok: locks signed in one block never share a coin, and both land");
}

/// A taker who never claims: the maker's own coin comes back through the Simplicity leaf.
fn s2l_refund(world: &World, client: &mut Client) {
    println!("\n==> S to L: refund");
    world.top_up(client);
    let maker = Pubkey58::parse(&world.maker_pubkey);
    let (_, point) = fresh_point();
    taker_lock(world, &point, maker, 101_400, false, SOL_AMOUNT);
    let height = world.chain.tip().unwrap() + 30;
    let terms = world.terms(Direction::S2L, point, height, LIQ_AMOUNT, SOL_AMOUNT);
    let key = maker_key(client, point.id());
    let lock = liquid_tx(client.call(&Call::LockLiquid { terms: terms.clone() }).unwrap());
    world.chain.broadcast(&lock).unwrap();
    world.node.mine(2);
    let coin = swap_coin(world, &terms, &key);
    assert_bad_timeout(client.call(&Call::RefundLiquid { terms: terms.clone() }).unwrap());
    world.node.mine(30);
    let refund = liquid_tx(client.call(&Call::RefundLiquid { terms: terms.clone() }).unwrap());
    assert_eq!(refund.output[0].script_pubkey, world.maker_script, "the refund pays our wallet");
    let info: serde_json::Value =
        serde_json::from_str(&world.node.rpc.call(&["decoderawtransaction", &serialize_hex(&refund)]).unwrap())
            .unwrap();
    println!("    refund: vsize {}, discountvsize {}", info["vsize"], info["discountvsize"]);
    world.chain.broadcast(&refund).unwrap();
    world.node.mine(1);
    let mined = world.chain.tx(&refund.txid()).unwrap();
    assert_eq!(is_key_path_spend(&mined, &coin.coin.outpoint), Some(false), "a refund is recognisably not a claim");
    println!("    ok: the refund went through the Simplicity leaf and paid our wallet");
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
    world.top_up(&mut client);

    // Tokens the hot key collected go back under the cap.
    let deposit = solana_tx(client.call(&Call::DepositToVault { amount: 5 * SOL_AMOUNT }).unwrap());
    world.send_solana(&deposit).unwrap();
    assert_eq!(world.sol.token_balance(&vault_token).unwrap(), 5 * SOL_AMOUNT);
    println!("    ok: collected tokens moved into the vault");

    let lock_once = |client: &mut Client, amount: u64, deadline: u64| {
        let (_, point) = fresh_point();
        let height = world.chain.tip().unwrap() + 300;
        let terms = world.terms(Direction::L2S, point, height, amount * 100, amount);
        l2s_ready(world, client, &terms);
        (client.call(&Call::LockSolana { terms, deadline_slot: deadline }).unwrap(), point)
    };

    let deadline = world.sol.slot().unwrap() + 2_000;
    let (reply, point) = lock_once(&mut client, SOL_AMOUNT, deadline);
    world.send_solana(&solana_tx(reply)).unwrap();
    let e = world.sol.escrow(&sol::escrow_pda(&vault, &point_64(&point))).unwrap().unwrap();
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

/// Collect on Solana, then try to take the Liquid coin back too.
fn s2l_collected(world: &World, client: &mut Client) -> SwapTerms {
    let maker = Pubkey58::parse(&world.maker_pubkey);
    let (secret, point) = fresh_point();
    let escrow = taker_lock(world, &point, maker, 101_400, false, SOL_AMOUNT);
    let height = world.chain.tip().unwrap() + 40;
    let terms = world.terms(Direction::S2L, point, height, LIQ_AMOUNT, SOL_AMOUNT);
    let lock = liquid_tx(client.call(&Call::LockLiquid { terms: terms.clone() }).unwrap());
    world.chain.broadcast(&lock).unwrap();
    world.node.mine(2);

    let taker_solana = terms.taker_solana.clone();
    let claim = solana_tx(client.call(&Call::ClaimSolana { taker_solana, lock_point: point, secret }).unwrap());
    world.send_solana(&claim).unwrap();
    assert_eq!(world.sol.escrow(&escrow).unwrap().unwrap().state, EscrowState::Claimed);

    world.node.mine(45);
    assert!(world.chain.tip().unwrap() >= height, "past the lock height");
    assert_refused(client.call(&Call::RefundLiquid { terms: terms.clone() }).unwrap(), Refusal::RefundAfterClaim);
    println!("    ok: after collecting on Solana, the refund is refused, even past the lock height");
    terms
}
