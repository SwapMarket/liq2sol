// Drives the real page in a real browser, against whatever maker the caller started. Everything is real except the Solana wallet,
// which is a test wallet that registers through the Wallet Standard as an extension would, and lives only in this file.
//
// Run through the daemon's suite:  CLIENT_TEST=1 SCENARIO="browser" cargo test -p liq2sol-maker --test e2e -- --ignored --nocapture
import { createServer } from 'node:http';
import { readFileSync, existsSync, mkdtempSync } from 'node:fs';
import { createPrivateKey, sign as edSign } from 'node:crypto';
import { execFile, execSync } from 'node:child_process';
import { promisify } from 'node:util';
import { tmpdir, homedir } from 'node:os';
import path from 'node:path';
import { chromium } from 'playwright-core';

const env = process.env;
const run = promisify(execFile);
const SITE = path.resolve(import.meta.dirname, '../site');
const rpcArgs = JSON.parse(env.ELEMENTS_ARGS);
const cli = (...args) => run(env.ELEMENTS_CLI, [...rpcArgs, ...args], { maxBuffer: 1 << 26 }).then((r) => r.stdout.trim());
const walletCli = (...args) => cli('-rpcwallet=w', ...args);

// ---- An Esplora API over the node, with the CORS headers the real ones send ----
async function scan(address) {
  for (let i = 0; ; i++) {
    try {
      return JSON.parse(await cli('scantxoutset', 'start', JSON.stringify([`addr(${address})`])));
    } catch (e) {
      if (i > 100 || !String(e.stderr ?? e).includes('already in progress')) throw e;
      await new Promise((r) => setTimeout(r, 100));
    }
  }
}

async function esplora(pathname, method, body) {
  const m = (re) => re.exec(pathname);
  let x;
  if (pathname === '/blocks/tip/height') return cli('getblockcount');
  if (pathname === '/block-height/0') return cli('getblockhash', '0');
  if ((x = m(/^\/address\/([^/]+)\/utxo$/))) {
    const found = await scan(x[1]);
    return JSON.stringify(found.unspents.map((u) => ({ txid: u.txid, vout: u.vout, status: { confirmed: true, block_height: u.height } })));
  }
  if ((x = m(/^\/tx\/([0-9a-f]+)\/hex$/))) return cli('getrawtransaction', x[1]);
  if ((x = m(/^\/tx\/([0-9a-f]+)\/status$/))) {
    const info = JSON.parse(await cli('getrawtransaction', x[1], '1'));
    return JSON.stringify({ confirmed: (info.confirmations ?? 0) > 0 });
  }
  if (pathname === '/tx' && method === 'POST') return cli('sendrawtransaction', body);
  throw new Error(`no such path ${pathname}`);
}

function serve(handler) {
  return new Promise((resolve) => {
    const server = createServer(handler);
    server.listen(0, '127.0.0.1', () => resolve({ server, port: server.address().port }));
  });
}

const shim = await serve(async (req, res) => {
  let body = '';
  for await (const chunk of req) body += chunk;
  res.setHeader('access-control-allow-origin', '*');
  try {
    res.end(await esplora(new URL(req.url, 'http://x').pathname.replace(/^\/[ab]/, ''), req.method, body));
  } catch (e) {
    res.statusCode = 400;
    res.end(String(e.stderr ?? e.message));
  }
});

const TYPES = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.wasm': 'application/wasm' };
const config = {
  name: env.NETWORK_NAME, regtest: true, genesis: await cli('getblockhash', '0'), usdt: env.USDT, lbtc: env.LBTC,
  esplora: [`http://127.0.0.1:${shim.port}/a`, `http://127.0.0.1:${shim.port}/b`], solana: env.SOLANA_RPC, relays: [env.RELAY_URL],
  pollMs: 1000,
  // The suite's maker locks for only 150 slots, so that failures play out in minutes.
  claimCutoffSlots: 20,
};
// The makers this page treats as vetted. The suite's maker has a new key each run, so the list is set once it's known.
let vetted = [];
const site = await serve((req, res) => {
  const rel = new URL(req.url, 'http://x').pathname.replace(/^\/$/, '/index.html');
  const file = path.join(SITE, rel);
  if (!file.startsWith(SITE) || !existsSync(file)) {
    res.statusCode = 404;
    res.end();
    return;
  }
  let data = readFileSync(file);
  if (rel === '/js/config.js') data = `${data}\nObject.assign(NETWORK, ${JSON.stringify(config)});\n`;
  if (rel === '/js/vetted.js') data = `${data}\nVETTED_MAKERS[${JSON.stringify(env.NETWORK_NAME)}] = ${JSON.stringify(vetted.map((key) => ({ key, name: 'test' })))};\n`;
  // The real page allows https and wss only. These endpoints are local, so only this test relaxes it.
  if (rel === '/index.html') data = String(data).replace("connect-src 'self' https: wss:;", 'connect-src * ws: http:;');
  res.setHeader('content-type', TYPES[path.extname(file)] ?? 'application/octet-stream');
  res.end(data);
});

// ---- The test wallet ----
const keyBytes = Uint8Array.from(JSON.parse(readFileSync(env.TAKER_SOLANA_KEY)));
const PKCS8 = Buffer.from('302e020100300506032b657004220420', 'hex');
const privateKey = createPrivateKey({ key: Buffer.concat([PKCS8, Buffer.from(keyBytes.slice(0, 32))]), format: 'der', type: 'pkcs8' });
const ALPHABET = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz';
function base58(bytes) {
  let n = BigInt(`0x${Buffer.from(bytes).toString('hex')}`);
  let out = '';
  while (n > 0n) {
    out = ALPHABET[Number(n % 58n)] + out;
    n /= 58n;
  }
  for (const b of bytes) {
    if (b !== 0) break;
    out = `1${out}`;
  }
  return out;
}
const ownerAddress = base58(keyBytes.slice(32));

const executable = env.CHROME
  ?? String(execSync(`ls ${path.join(homedir(), '.cache/ms-playwright')}/chromium_headless_shell-*/*/chrome-headless-shell | head -1`)).trim();

const browser = await chromium.launch({ executablePath: executable, args: ['--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage', '--no-proxy-server'] });
const shots = mkdtempSync(path.join(env.SHOTS_DIR ?? tmpdir(), 'client-'));
console.log(`    screenshots in ${shots}`);

async function newPage(viewport = { width: 1200, height: 1000 }) {
  const context = await browser.newContext({ acceptDownloads: true, viewport });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
  page.on('console', (m) => { if (m.type() === 'warning') console.log(`    page: ${m.text().slice(0, 300)}`); });
  page.errors = errors;
  await page.exposeFunction('testWalletSign', (hexTx) => {
    const tx = Buffer.from(hexTx, 'hex');
    const signature = edSign(null, tx.subarray(65), privateKey);
    signature.copy(tx, 1);
    return tx.toString('hex');
  });
  await page.addInitScript((address) => {
    const hex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
    const bytes = (h) => Uint8Array.from(h.match(/../g), (x) => parseInt(x, 16));
    const account = { address, publicKey: new Uint8Array(32), chains: ['solana:devnet'], features: [] };
    const wallet = {
      version: '1.0.0', name: 'Test Wallet', icon: 'data:image/svg+xml,<svg xmlns="http://www.w3.org/2000/svg"/>', chains: ['solana:devnet'], accounts: [account],
      features: {
        'standard:connect': { version: '1.0.0', connect: async () => ({ accounts: [account] }) },
        'solana:signTransaction': {
          version: '1.0.0',
          signTransaction: async ({ transaction }) => [{ signedTransaction: bytes(await window.testWalletSign(hex(transaction))) }],
        },
      },
    };
    const register = ({ register: r }) => r(wallet);
    window.addEventListener('wallet-standard:app-ready', ({ detail }) => register(detail));
    window.dispatchEvent(new CustomEvent('wallet-standard:register-wallet', { detail: register }));
  }, ownerAddress);
  await page.goto(`http://127.0.0.1:${site.port}/`);
  return page;
}

const step = (t) => console.log(`    ${t}`);
const state = (page) => page.locator('.swap-card').first().getAttribute('data-state');
const waitState = (page, want, ms = 240000) => page.locator(`.swap-card[data-state="${want}"]`).first().waitFor({ timeout: ms });

// Logs get pasted into chats. After a real swap, none of its secrets may be in them, in any form the page knows.
async function checkLogs(page) {
  const { text, swaps } = await page.evaluate(() => ({
    text: localStorage.getItem('liq2sol.logs') ?? '',
    swaps: Object.values(JSON.parse(localStorage.getItem('liq2sol.swaps') ?? '{}')),
  }));
  if (!text.includes('->')) throw new Error('the log has no swap states in it');
  for (const s of swaps) {
    for (const v of [s.secrets.lock_secret, s.secrets.taker_sk, s.secrets.blinding_key, s.nostr.secret, s.claim_tx]) {
      if (v && text.includes(v)) throw new Error('a secret reached the log');
    }
  }
}

async function checkSettingsLogs(page) {
  await page.locator('.gear').click();
  await page.locator('#logs-download').waitFor();
  const [file] = await Promise.all([page.waitForEvent('download'), page.locator('#logs-download').click()]);
  const saved = readFileSync(await file.path(), 'utf8');
  if (!saved.includes('started, build')) throw new Error(`the downloaded log is wrong: ${saved.slice(0, 200)}`);
  await page.screenshot({ path: path.join(shots, 'settings.png') });
  await page.locator('#logs-delete').click();
  if (!(await page.locator('#logs-note').innerText()).includes('deleted')) throw new Error('the logs were not deleted');
  await page.getByRole('button', { name: 'Cancel' }).click();
}

async function checkNoErrors(page) {
  const real = page.errors.filter((e) => !/Failed to load resource|WebSocket/.test(e));
  if (real.length) throw new Error(`the page logged errors: ${real.join(' | ').slice(0, 500)}`);
}

async function startSwap(page, flip, to) {
  await page.getByRole('button', { name: 'Test Wallet' }).click();
  if (flip) await page.locator('.flip').click();
  await page.locator('tbody tr').first().waitFor({ timeout: 30000 });
  await page.screenshot({ path: path.join(shots, `book-${flip ? 's2l' : 'l2s'}.png`) });
  await page.locator('#amount').fill(env.AMOUNT);
  if (to) await page.locator('#to').fill(to);
  await page.locator('#go:not(:disabled)').click();
  await page.locator('#accept').waitFor({ timeout: 30000 });
  await page.screenshot({ path: path.join(shots, `quote-${flip ? 's2l' : 'l2s'}.png`) });
  await page.locator('#accept').click();
  await page.locator('#download').waitFor({ timeout: 30000 });
  step('quote accepted and terms checked');

  if (await page.locator('.pay-address').count() || await page.locator('.lock').count()) throw new Error('the next step showed before the recovery file');
  const [download] = await Promise.all([page.waitForEvent('download'), page.locator('#download').click()]);
  const file = path.join(shots, `recovery-${Date.now()}.json`);
  await download.saveAs(file);
  await page.locator('#recovery-file').setInputFiles({ name: 'other.json', mimeType: 'application/json', buffer: Buffer.from('{"format":"nope"}') });
  await page.locator('#banner:not([hidden])').waitFor({ timeout: 5000 });
  if (await page.locator('.pay-address').count() || await page.locator('.lock').count()) throw new Error('a bad recovery file unlocked the next step');
  await page.locator('#recovery-file').setInputFiles(file);
  step('a bad file is refused, and the funding step only appears after the real one is read back');
  return file;
}

const pay = (address, sats, label) => {
  const amount = `${Math.floor(sats / 1e8)}.${String(sats % 1e8).padStart(8, '0')}`;
  return walletCli('sendtoaddress', address, amount, '', '', 'false', 'false', '1', 'UNSET', 'false', label);
};
const amountSats = (t) => Math.round(Number(t) * 1e6) * 100;
// What the node's own wallet reads at this address, of the swap's asset, in base units.
async function receivedAt(address) {
  const coins = JSON.parse(await walletCli('listunspent', '0', '9999999', JSON.stringify([address])));
  return coins.filter((c) => c.asset === env.USDT).reduce((sum, c) => sum + Math.round(c.amount * 1e8), 0);
}

async function reportSwaps(page) {
  const points = await page.evaluate(() => Object.values(JSON.parse(localStorage.getItem('liq2sol.swaps'))).map((s) => s.secrets.lock_point));
  for (const point of points) console.log(`SWAP ${point}`);
}

// More than one offer can take: the maker's per-swap limit splits it into two real swaps, with one recovery file.
async function split() {
  const page = await newPage();
  await page.locator('tbody tr').first().waitFor({ timeout: 30000 });
  await page.locator('#amount').fill(env.SPLIT_AMOUNT);
  await page.getByRole('button', { name: 'Test Wallet' }).click();
  await page.locator('#go:not(:disabled)').click();
  await page.locator('#not-atomic').waitFor({ timeout: 30000 });
  await page.screenshot({ path: path.join(shots, 'split-quote.png') });
  const review = await page.locator('.card .facts').innerText();
  // 120 units crosses into the maker's dearer step at 150 bps, and 50 stays at 100 bps.
  if (!review.includes('0.00012 → 0.000118') || !review.includes('0.00005 → 0.000049')) throw new Error(`wrong prices in the review: ${review}`);
  step('each fill has the price for its size');
  step('the review says these are separate swaps, and that some may refund');
  await page.locator('#accept').click();
  const [download] = await Promise.all([page.waitForEvent('download'), page.locator('#download').click()]);
  await page.locator('#recovery-file').setInputFiles(await download.path());
  await page.locator('.pay-address').nth(1).waitFor({ timeout: 10000 });
  await page.screenshot({ path: path.join(shots, 'split-pay.png'), fullPage: true });
  const count = await page.locator('.pay-address').count();
  if (count !== 2) throw new Error(`${count} addresses for a split order`);
  const addresses = await page.locator('.pay-address').allInnerTexts();
  const amounts = await page.locator('.pay-amount').allInnerTexts();
  if (addresses[0] === addresses[1]) throw new Error('both swaps share an address');
  const sum = amounts.reduce((s, a) => s + Math.round(parseFloat(a) * 1e8), 0);
  if (sum !== amountSats(env.SPLIT_AMOUNT)) throw new Error(`the payments add up to ${sum}`);
  for (let i = 0; i < 2; i++) {
    await pay(addresses[i], Math.round(parseFloat(amounts[i]) * 1e8), env.USDT);
    await walletCli('generatetoaddress', '1', await walletCli('getnewaddress'));
  }
  step('one recovery file covers both, and two exact payments to two addresses add up to the order');
  await page.locator('.one-endpoint').first().waitFor({ timeout: 240000 });
  await page.locator('.one-endpoint').first().check();
  for (let i = 0; i < 2; i++) {
    await page.locator('.swap-card[data-state="ready_claim"] .claim').first().waitFor({ timeout: 240000 });
    await page.locator('.swap-card[data-state="ready_claim"] .claim').first().click();
    await page.locator('.swap-card[data-state="done"]').nth(i).waitFor({ timeout: 120000 });
  }
  await page.screenshot({ path: path.join(shots, 'split-done.png'), fullPage: true });
  step('both swaps claimed');
  await reportSwaps(page);
  await checkNoErrors(page);
  await page.context().close();
}

async function l2s() {
  const page = await newPage();
  await startSwap(page, false);
  await page.locator('.pay-address').waitFor();
  await page.screenshot({ path: path.join(shots, 'pay.png') });
  const address = await page.locator('.pay-address').innerText();
  await pay(address, amountSats(env.AMOUNT), env.USDT);
  step(`paid ${env.AMOUNT} from the node's wallet to the address the page derived`);
  await waitState(page, 'ready_claim');
  await page.screenshot({ path: path.join(shots, 'claim.png') });
  step('the page signed the maker’s claim, and sees its lock');
  await page.locator('.claim').click();
  await page.locator('#banner:not([hidden])').waitFor({ timeout: 8000 });
  if (await state(page) !== 'ready_claim') throw new Error('claimed with one endpoint and no consent');
  step('with one Solana endpoint and no consent, the secret stays hidden');
  await page.locator('.one-endpoint').check();
  await page.locator('.claim').click();
  await waitState(page, 'done');
  await page.screenshot({ path: path.join(shots, 'done.png') });
  step('claimed on Solana');
  await checkLogs(page);
  step('the log has the swap’s steps, and none of its secrets');
  await reportSwaps(page);
  await checkNoErrors(page);
  await page.context().close();
}

async function s2l() {
  const page = await newPage();
  const to = await walletCli('getnewaddress');
  await startSwap(page, true, to);
  await page.locator('.lock').waitFor();
  await page.screenshot({ path: path.join(shots, 'lock.png') });
  await page.locator('.lock').click();
  step('locked on Solana');
  await waitState(page, 'done');
  await page.screenshot({ path: path.join(shots, 's2l-done.png') });
  step('the page signed with the maker, and its claim confirmed on Liquid');
  const swap = await page.evaluate(() => Object.values(JSON.parse(localStorage.getItem('liq2sol.swaps')))[0]);
  const tx = JSON.parse(await cli('getrawtransaction', swap.claim_txid, '1'));
  const coinInput = tx.vin[0].txinwitness;
  if (coinInput.length !== 1) throw new Error(`a claim's witness has ${coinInput.length} items`);
  if (tx.vout.some((o) => o.scriptPubKey?.type !== 'fee' && o.value !== undefined)) throw new Error('an output is visible');
  step('on-chain it is one signature, and everything but the fee is hidden');
  await checkLogs(page);
  const got = await receivedAt(to);
  if (got !== Number(swap.terms.liquid_amount)) throw new Error(`received ${got} units, expected ${swap.terms.liquid_amount}`);
  step('the wallet unblinded exactly the agreed amount at the address it gave');
  await reportSwaps(page);
  await checkNoErrors(page);
  await page.context().close();
}

// A taker who pays, sees the maker lock, and never claims. The maker refunds, and this browser loses everything but the file.
async function rescue() {
  const page = await newPage();
  const file = await startSwap(page, false);
  await page.locator('.pay-address').waitFor();
  await pay(await page.locator('.pay-address').innerText(), amountSats(env.AMOUNT), env.USDT);
  await waitState(page, 'ready_claim');
  step('the maker locked, and this taker never claims');
  await page.context().close();

  const fresh = await newPage();
  await fresh.goto(`http://127.0.0.1:${site.port}/#rescue`);
  await fresh.locator('#rescue-file').setInputFiles(file);
  await fresh.locator('.swap-card').waitFor({ timeout: 10000 });
  step('a browser with no stored data picked the swap up from the recovery file');
  await fresh.locator('.swap-card[data-state="refund_wait"], .swap-card[data-state="refund"]').waitFor({ timeout: 240000 });
  step('the maker refunded on Solana, so the page moved to a refund');
  const swap = await fresh.evaluate(() => Object.values(JSON.parse(localStorage.getItem('liq2sol.swaps')))[0]);
  await walletCli('generatetoaddress', String(Math.max(0, swap.terms.lock_height - Number(await cli('getblockcount')) + 1)), await walletCli('getnewaddress'));
  await waitState(fresh, 'refund');
  await fresh.screenshot({ path: path.join(shots, 'refund.png') });
  await pay(await fresh.locator('.fee-address').innerText(), 30, env.LBTC);
  await walletCli('generatetoaddress', '1', await walletCli('getnewaddress'));
  await new Promise((r) => setTimeout(r, 3000));
  const back = await walletCli('getnewaddress');
  await fresh.locator('.refund-to').fill(back);
  await fresh.locator('.refund').click();
  await waitState(fresh, 'refunded');
  const got = await receivedAt(back);
  if (got !== Number(swap.terms.liquid_amount)) throw new Error(`the refund paid ${got} units`);
  step('the refund went through the Simplicity leaf and paid the exact amount back');
  await checkNoErrors(fresh);
  await fresh.context().close();
}

// Runs first: it finds the suite's maker, and every other scenario then trades with that maker as a vetted one.
async function vetting() {
  const page = await newPage();
  await page.locator('.empty', { hasText: 'hidden by the switches' }).waitFor({ timeout: 30000 });
  if (!(await page.locator('#only-vetted').isChecked())) throw new Error('"only vetted" should start on');
  if (await page.locator('#unvetted-warning').count()) throw new Error('a warning with the switch on');
  await page.locator('#amount').fill(env.AMOUNT);
  if (await page.locator('#go:not(:disabled)').count()) throw new Error('an order could go to a hidden maker');
  step('a new visitor sees no unvetted maker, and can\'t trade with one');

  await page.locator('#only-vetted').uncheck();
  const row = page.locator('tbody tr').first();
  await row.waitFor({ timeout: 3000 });
  await page.locator('#unvetted-warning').waitFor({ timeout: 3000 });
  if (await page.locator('.vetted').count()) throw new Error('a checkmark on a maker that isn\'t listed');
  const key = await row.locator('td.mono').getAttribute('title');
  if (!/^[0-9a-f]{64}$/.test(key)) throw new Error(`the row's key: ${key}`);
  await page.reload();
  await page.locator('tbody tr').first().waitFor({ timeout: 30000 });
  if (await page.locator('#only-vetted').isChecked()) throw new Error('turning it off did not survive a reload');
  step('with the switch off, the maker shows with a warning and no checkmark, and that survives a reload');
  await checkNoErrors(page);
  await page.context().close();

  // Same start and end, so the same on screen.
  vetted = [`${key.slice(0, 30)}${key[30] === '0' ? '1' : '0'}${key.slice(31)}`];
  const lookalike = await newPage();
  await lookalike.locator('.empty', { hasText: 'hidden by the switches' }).waitFor({ timeout: 30000 });
  await lookalike.context().close();
  step('a listed key that only looks like the maker\'s doesn\'t count');

  vetted = [key];
  const listed = await newPage();
  await listed.locator('tbody tr .vetted').first().waitFor({ timeout: 30000 });
  await listed.context().close();
  step('once listed, the maker has its checkmark with the switch on');
}

// The library is held back here, so this shows the dialog doesn't wait for it, and needs no relay.
async function walletConnectWait() {
  const page = await newPage();
  let release;
  const held = new Promise((r) => { release = r; });
  await page.route('**/vendor/walletconnect.js', async (route) => { await held; await route.abort(); });
  await page.getByRole('button', { name: 'WalletConnect' }).click();
  await page.locator('dialog .spinner').waitFor({ timeout: 2000 });
  const turning = await page.locator('dialog .spinner').evaluate((e) => getComputedStyle(e).animationName);
  if (turning !== 'spin') throw new Error(`the spinner isn't animated: ${turning}`);
  release();
  await page.locator('dialog').waitFor({ state: 'detached', timeout: 10000 });
  await page.locator('#banner').waitFor({ timeout: 5000 });
  await page.context().close();
  step('WalletConnect shows a moving spinner at once, and a failure closes it with a message');
}

async function look() {
  await walletConnectWait();
  const phone = await newPage({ width: 400, height: 800 });
  await phone.locator('tbody tr').first().waitFor({ timeout: 30000 });
  await phone.locator('#amount').fill(env.AMOUNT);
  await checkSettingsLogs(phone);
  await phone.locator('.star').first().click();
  await phone.locator('.star.on').first().waitFor({ timeout: 3000 });
  await phone.locator('#only-favorites').check();
  await phone.locator('.star.on').first().waitFor({ timeout: 3000 });
  await phone.reload();
  await phone.locator('.star.on').first().waitFor({ timeout: 30000 });
  if (!(await phone.locator('#only-favorites').isChecked())) throw new Error('"only favorites" did not survive a reload');
  await phone.locator('.star.on').first().click();
  await phone.locator('tbody tr').waitFor({ state: 'detached', timeout: 3000 });
  await phone.locator('#only-favorites').uncheck();
  await phone.locator('tbody tr').first().waitFor({ timeout: 3000 });
  step('a favorite and the switch survive a reload, and the switch hides everyone else');
  step('Settings can download and delete the logs');
  await phone.screenshot({ path: path.join(shots, 'phone.png'), fullPage: true });
  const wide = await phone.evaluate(() => document.documentElement.scrollWidth > window.innerWidth);
  if (wide) throw new Error('the page scrolls sideways on a phone');
  await checkNoErrors(phone);
  await phone.context().close();
}

let code = 0;
try {
  for (const name of ['vetting', ...(env.BROWSER_SCENARIOS ?? 'look,l2s,s2l,split,rescue').split(',')]) {
    console.log(`  == browser: ${name}`);
    await { vetting, look, l2s, s2l, rescue, split }[name]();
  }
  console.log('BROWSER OK');
} catch (e) {
  console.log(`BROWSER FAILED: ${e.message}`);
  for (const context of browser.contexts()) {
    for (const page of context.pages()) {
      await page.screenshot({ path: path.join(shots, 'failed.png') }).catch(() => {});
      const seen = await page.locator('#banner').innerText({ timeout: 1000 }).catch(() => '');
      const card = await page.locator('.swap-card').first().getAttribute('data-state', { timeout: 1000 }).catch(() => '');
      console.log(`    the page shows state "${card}" and message "${seen.slice(0, 300)}"`);
      const swaps = await page.evaluate(() => Object.values(JSON.parse(localStorage.getItem('liq2sol.swaps') ?? '{}'))).catch(() => []);
      for (const x of swaps) {
        console.log(`    swap ${x.secrets.lock_point.slice(0, 12)}: state ${x.state}, status "${x.status}", tip ${x.tip}, lock height ${x.terms.lock_height}, accepted at ${x.terms.h_accept}, coin ${JSON.stringify(x.coin ?? null)}, signed ${!!x.signed}, escrow ${x.escrow_address}`);
      }
    }
  }
  code = 1;
} finally {
  await browser.close();
  shim.server.close();
  site.server.close();
}
process.exit(code);
