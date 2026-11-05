#!/usr/bin/env python3
"""Stage 1 of the confidential redesign: a real browser builds and signs a whole confidential claim, and a real node judges it.

Needs a Chromium browser (`CHROME`, or Playwright's headless shell) and elementsd:  ELEMENTSD=~/bin/elementsd ELEMENTS_CLI=~/bin/elements-cli scripts/check-browser-claim.py
First: (cd contracts/swapkey-wasm && RUSTFLAGS="" wasm-pack build --target web --out-dir pkg)
"""
import glob, http.server, json, os, secrets, shutil, socketserver, subprocess, sys, tempfile, threading, time, urllib.parse

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PKG = os.path.join(ROOT, "contracts/swapkey-wasm/pkg")
# Playwright's headless shell. Full Chrome stalls loading local pages on the development VM, for reasons outside this project.
CHROME = os.environ.get("CHROME") or next(iter(glob.glob(os.path.expanduser(
    "~/.cache/ms-playwright/chromium_headless_shell-*/*/chrome-headless-shell"))), "google-chrome")
PORT = 21000 + os.getpid() % 5000
AMOUNT = "100.00000000"

work = tempfile.mkdtemp(prefix="l2s-browser-")
node_dir = os.path.join(work, "node")
os.makedirs(node_dir)
base = ["-chain=liquidregtest", "-rpcuser=t", "-rpcpassword=t", f"-rpcport={PORT}"]
node = subprocess.Popen([os.environ.get("ELEMENTSD", "elementsd"), f"-datadir={node_dir}", "-listen=0", "-txindex=1", "-validatepegin=0",
                         "-fallbackfee=0.00001", "-initialfreecoins=2100000000", "-anyonecanspendaremine=1",
                         "-evbparams=simplicity:-1:::", "-printtoconsole=0", *base], stdout=subprocess.DEVNULL)


def cli(*args):
    out = subprocess.run([os.environ.get("ELEMENTS_CLI", "elements-cli"), *base, *map(str, args)], capture_output=True, text=True)
    if out.returncode:
        raise RuntimeError(f"{args[0]}: {out.stderr.strip()}")
    return out.stdout.strip()


PAGE = """<!doctype html><title>claim</title><pre id="out">running</pre><script type="module">
import init, * as w from "./liq2sol_swapkey_wasm.js";
const q = new URLSearchParams(location.search), out = document.getElementById("out");
const done = (o) => { out.textContent = JSON.stringify(o); fetch("/result", { method: "POST", body: JSON.stringify(o) }); };
try {
  const t0 = performance.now();
  await init();
  const loaded = performance.now() - t0;
  const a = (k) => q.get(k);
  if (a("mode") === "address") {
    done({ address: w.swap_address(a("maker"), a("taker"), a("blinding"), Number(a("height"))), fee: w.fee_address(a("feesk")),
           random: [w.random_key(), w.random_key()], ua: navigator.userAgent, loaded });
  } else {
    const t1 = performance.now();
    const r = w.build_claim(a("maker"), a("taker"), a("blinding"), Number(a("height")), a("coin"), a("feetx"), a("feesk"),
                            a("genesis"), a("lbtc"), a("t")).split("\\n");
    done({ tx: r[0], secret: r[1], ms: performance.now() - t1, loaded });
  }
} catch (e) { done({ error: String(e) }); }
</script>"""


class Handler(http.server.SimpleHTTPRequestHandler):
    last = None

    def __init__(self, *a, **k):
        super().__init__(*a, directory=PKG, **k)

    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        query = self.rfile.read(length).decode()
        Handler.last = query
        self.send_response(200)
        self.end_headers()

    def do_GET(self):
        if self.path.startswith("/run.html"):
            body = PAGE.encode()
            self.send_response(200)
            self.send_header("content-type", "text/html")
            self.end_headers()
            self.wfile.write(body)
            return
        super().do_GET()

    def log_message(self, *a):
        pass


Handler.extensions_map = {**http.server.SimpleHTTPRequestHandler.extensions_map, ".wasm": "application/wasm", ".js": "text/javascript"}
server = socketserver.ThreadingTCPServer(("127.0.0.1", PORT + 1), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()


def browser(**params):
    url = f"http://127.0.0.1:{PORT + 1}/run.html?" + urllib.parse.urlencode(params)
    profile = tempfile.mkdtemp(dir=work)
    Handler.last = None
    chrome = subprocess.Popen([CHROME, "--no-sandbox", "--disable-gpu", "--no-proxy-server", "--disable-dev-shm-usage", "--remote-debugging-port=0", f"--user-data-dir={profile}", url],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        start = time.time()
        while Handler.last is None:
            if time.time() - start > 100:
                raise RuntimeError("the browser never reported back")
            if chrome.poll() is not None:
                raise RuntimeError(f"the browser exited with {chrome.returncode}")
            time.sleep(0.2)
    finally:
        chrome.terminate()
        try:
            chrome.wait(timeout=10)
        except subprocess.TimeoutExpired:
            chrome.kill()
    result = json.loads(Handler.last)
    if "error" in result:
        raise RuntimeError(f"the browser reported: {result['error']}")
    return result


def mine(n=1):
    cli("generatetoaddress", n, cli("getnewaddress"))


def ok(what):
    print(f"    ok: {what}")


status = 1
try:
    for _ in range(200):
        try:
            cli("getblockcount")
            break
        except RuntimeError:
            time.sleep(0.3)
    cli("createwallet", "w")
    cli("rescanblockchain")
    mine()
    usdt = json.loads(cli("issueasset", "1000", "0", "false"))["asset"]
    mine()
    lbtc = json.loads(cli("getsidechaininfo"))["pegged_asset"]
    genesis = cli("getblockhash", 0)

    keys = {k: secrets.token_hex(32) for k in ["maker", "taker", "blinding", "feesk", "t"]}
    height = int(cli("getblockcount")) + 500
    print(cli("getnetworkinfo").split('"subversion": ')[1].split(",")[0])

    print("\n==> The browser derives the address")
    first = browser(mode="address", height=height, **{k: v for k, v in keys.items() if k != "t"})
    print(f"    {first['ua'][:90]}")
    assert first["random"][0] != first["random"][1]
    ok(f"the browser supplied randomness, and the module loaded in {first['loaded']:.0f} ms")

    print("\n==> A wallet pays it")
    coin_id = cli("sendtoaddress", first["address"], AMOUNT, "", "", "false", "false", 1, "UNSET", "false", usdt)
    fee_id = cli("sendtoaddress", first["fee"], "0.00000300", "", "", "false", "false", 1, "UNSET", "false", "bitcoin")
    mine()
    ok("funded a blinded coin and a 300-sat fee coin")

    print("\n==> The browser builds and signs the whole claim")
    coin = cli("getrawtransaction", coin_id)
    feetx = cli("getrawtransaction", fee_id)
    # Everything goes through the URL, which is plenty for two transactions.
    if len(coin) + len(feetx) > 1_500_000:
        raise RuntimeError("funding transactions are too large for a URL")
    result = browser(mode="claim", height=height, coin=coin, feetx=feetx, genesis=genesis, lbtc=lbtc, **keys)
    ok(f"unblinding, range and surjection proofs, and adapter signing took {result['ms']:.0f} ms")
    assert result["secret"] == keys["t"], "the secret must come out of the signature"
    ok("the secret came out of the finished signature")

    print("\n==> The node judges it")
    accepted = json.loads(cli("testmempoolaccept", json.dumps([result["tx"]])))[0]
    assert accepted["allowed"], accepted
    ok(f"the node accepts it at the 300-sat fee: {accepted['vsize']} vB")
    txid = cli("sendrawtransaction", result["tx"])
    mine()
    decoded = json.loads(cli("getrawtransaction", txid, 1))
    assert decoded["confirmations"] == 1
    hidden = decoded["vout"][0]
    assert "value" not in hidden and "asset" not in hidden, hidden
    ok("it mined, and the paid output's value and asset are hidden")
    witness = decoded["vin"][0]["txinwitness"]
    assert len(witness) == 1 and len(witness[0]) == 130, witness
    ok("its witness is one 65-byte signature")
    print("\nBROWSER CLAIM OK")
    status = 0
finally:
    server.shutdown()
    try:
        cli("stop")
        node.wait(timeout=30)
    except Exception:
        node.kill()
    shutil.rmtree(work, ignore_errors=True)
sys.exit(status)
