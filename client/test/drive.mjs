// Drives the real page in a real browser, against whatever maker the caller started. Everything is real except the Solana wallet,
// which is a test wallet that registers through the Wallet Standard as an extension would, and lives only in this file.
//
// Run through the daemon's suite:  CLIENT_TEST=1 SCENARIO="browser" cargo test -p liq2sol-maker --test e2e -- --ignored --nocapture
import { createServer } from 'node:http';
import { readFileSync, existsSync, mkdtempSync } from 'node:fs';
import { createPrivateKey, generateKeyPairSync, sign as edSign } from 'node:crypto';
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
  if ((x = m(/^\/address\/([^/]+)\/txs$/))) {
    const tip = Number(await cli('getblockcount'));
    for (; scannedTo <= tip; scannedTo++) {
      const block = JSON.parse(await cli('getblock', await cli('getblockhash', String(scannedTo)), '2'));
      for (const t of block.tx) {
        for (const o of t.vout) {
          const a = o.scriptPubKey?.address;
          if (a) txsByAddress.set(a, [...(txsByAddress.get(a) ?? []), { txid: t.txid }]);
        }
      }
    }
    return JSON.stringify(txsByAddress.get(x[1]) ?? []);
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

let loseBroadcastReply = false;
const txsByAddress = new Map();
let scannedTo = 0;
const shim = await serve(async (req, res) => {
  let body = '';
  for await (const chunk of req) body += chunk;
  res.setHeader('access-control-allow-origin', '*');
  try {
    const reply = await esplora(new URL(req.url, 'http://x').pathname.replace(/^\/[ab]/, ''), req.method, body);
    // The transaction went out, and the page never hears so.
    if (req.method === 'POST' && loseBroadcastReply) {
      loseBroadcastReply = false;
      req.socket.destroy();
      return;
    }
    res.end(reply);
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

async function newPage(viewport = { width: 1200, height: 1000 }, beforeLoad = null, phone = false, who = { address: ownerAddress, key: privateKey }) {
  const context = await browser.newContext({ acceptDownloads: true, viewport, permissions: ['clipboard-read', 'clipboard-write'], ...(phone ? { hasTouch: true, isMobile: true } : {}) });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
  page.on('console', (m) => { if (m.type() === 'warning') console.log(`    page: ${m.text().slice(0, 300)}`); });
  page.errors = errors;
  await page.exposeFunction('testWalletSign', (hexTx) => {
    const tx = Buffer.from(hexTx, 'hex');
    const signature = edSign(null, tx.subarray(65), who.key);
    signature.copy(tx, 1);
    return tx.toString('hex');
  });
  if (!phone) await page.addInitScript((address) => {
    const hex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
    const bytes = (h) => Uint8Array.from(h.match(/../g), (x) => parseInt(x, 16));
    const account = { address, publicKey: new Uint8Array(32), chains: ['solana:devnet'], features: [] };
    const wallet = {
      version: '1.0.0', name: 'Test Wallet', icon: 'data:image/svg+xml,<svg xmlns="http://www.w3.org/2000/svg"/>', chains: ['solana:devnet'], accounts: [account],
      features: {
        'standard:connect': { version: '1.0.0', connect: async (input) => {
          window.testWalletSilent = !!input?.silent;
          if (input?.silent && localStorage.getItem('test.refuseSilent')) throw new Error('not trusted');
          const refuse = localStorage.getItem('test.refuseConnect');
          if (!input?.silent && refuse === 'person') throw Object.assign(new Error('User rejected the request'), { code: 4001 });
          if (!input?.silent && refuse === 'broken') throw new Error('Unexpected wallet failure');
          return { accounts: [account] };
        } },
        'solana:signTransaction': {
          version: '1.0.0',
          signTransaction: async ({ transaction }) => [{ signedTransaction: bytes(await window.testWalletSign(hex(transaction))) }],
        },
      },
    };
    const register = ({ register: r }) => r(wallet);
    window.addEventListener('wallet-standard:app-ready', ({ detail }) => register(detail));
    window.dispatchEvent(new CustomEvent('wallet-standard:register-wallet', { detail: register }));
  }, who.address);
  if (beforeLoad) await beforeLoad(page);
  await page.goto(`http://127.0.0.1:${site.port}/`, { waitUntil: 'commit' });
  return page;
}

const step = (t) => console.log(`    ${t}`);
async function connectTestWallet(page) {
  const go = page.locator('#go', { hasText: 'Connect a Solana wallet' });
  await go.waitFor({ timeout: 15000 });
  await go.click();
  await page.getByRole('link', { name: 'disconnect' }).waitFor({ timeout: 5000 });
}
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
  await connectTestWallet(page);
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
  await page.locator('#recovery-file').focus();
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
  await connectTestWallet(page);
  await page.locator('#go:not(:disabled)').click();
  await page.locator('#not-atomic').waitFor({ timeout: 30000 });
  await page.screenshot({ path: path.join(shots, 'split-quote.png') });
  const review = await page.locator('.card .facts').innerText();
  // 120 units goes to the first maker's dearer step, at 150 bps, and the 50 left to the dearer second maker, at 180.
  if (!review.includes('0.00012 → 0.000118') || !review.includes('0.00005 → 0.000049')) throw new Error(`wrong prices in the review: ${review}`);
  if (new Set((await page.locator('.card .facts dt.mono').allInnerTexts())).size !== 2) throw new Error(`one maker twice: ${review}`);
  step('each fill has the price for its size, from a different maker');
  step('the review says these are separate swaps, and that some may refund');
  await page.locator('#accept').click();
  const [download] = await Promise.all([page.waitForEvent('download'), page.locator('#download').click()]);
  await page.locator('#recovery-file').focus();
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

// The history shows what the swap really cost and who it was with, from the agreed terms.
async function checkHistory(page) {
  const swap = await page.evaluate(() => Object.values(JSON.parse(localStorage.getItem('liq2sol.swaps')))[0]);
  const liquid = Number(swap.terms.liquid_amount);
  const solana = Number(swap.terms.solana_amount) * 100;
  const want = (swap.direction === 'L2S' ? liquid / solana : solana / liquid).toFixed(4);
  await page.locator('.tab[data-tab="history"]').click();
  const row = page.locator('.swap-row').first();
  await row.waitFor({ timeout: 5000 });
  const price = await row.locator('.history-price').innerText();
  if (price !== `Price ${want}`) throw new Error(`the history says ${price}, but the terms come to ${want}`);
  if ((await row.locator('.history-maker').getAttribute('title')) !== swap.maker) throw new Error('the history has the wrong maker');
  if (!(await row.locator('.history-maker .vetted').count())) throw new Error('a vetted maker without its mark in the history');
  const coinLink = await row.locator('.explorer-coin').getAttribute('href', { timeout: 10000 });
  const blinded = /\/tx\/([0-9a-f]{64})#blinded=(\d+),([0-9a-f]{64}),([0-9a-f]{64}),([0-9a-f]{64})$/.exec(coinLink);
  if (!blinded || blinded[1] !== swap.coin.txid || blinded[2] !== String(swap.terms.liquid_amount) || blinded[3] !== env.USDT) throw new Error(`the explorer link: ${coinLink}`);
  if (!(await row.locator('.explorer-solana').getAttribute('href')).includes(swap.escrow_address)) throw new Error('the Solana link');
  if (!(await page.evaluate(() => localStorage.getItem('liq2sol.logs'))).includes('blinded=') === false) throw new Error('blinders reached the log');
  await page.locator('.tab[data-tab="swap"]').click();
  step('the history shows the price, the maker, and explorer links with the right amount and asset');
}

// A browser with only the file, on a swap that is over. It must never offer to pay or lock, whatever the chains are slow to say.
async function restoredPage(file, beforeLoad = null) {
  const page = await newPage(undefined, beforeLoad);
  await page.goto(`http://127.0.0.1:${site.port}/#rescue`);
  await page.locator('#rescue-file').focus();
  await page.locator('#rescue-file').setInputFiles(file);
  await page.locator('.swap-card').waitFor({ timeout: 10000 });
  return page;
}

const noSpendStep = async (page) => {
  if (await page.locator('.pay-address').count() || await page.locator('.lock').count()) throw new Error('a finished swap offered to pay or lock again');
};

async function checkFinishedRestore(file) {
  const page = await restoredPage(file);
  await page.locator('.swap-card[data-state="done"]').waitFor({ timeout: 90000 });
  await noSpendStep(page);
  await page.context().close();
}

// The lock's account can be closed once settled, so the page has to go by its history. Solana's answers are replaced to show that.
async function checkClosedLock(file) {
  const answering = (results) => (p) => p.route((url) => url.href.startsWith(env.SOLANA_RPC), async (route) => {
    const body = route.request().postDataJSON();
    const isEscrow = body?.method === 'getAccountInfo' && body.params[1]?.encoding === 'base64';
    const answer = isEscrow ? { context: { slot: 1 }, value: null } : results[body?.method];
    if (answer === undefined) return route.fallback();
    if (answer === 'down') return route.fulfill({ status: 500, body: 'down' });
    return route.fulfill({ json: { jsonrpc: '2.0', id: body.id, result: answer } });
  });

  const closed = await restoredPage(file, answering({ getSignaturesForAddress: [{ signature: 'x' }] }));
  await closed.locator('.swap-card[data-state="done"]').waitFor({ timeout: 30000 });
  await noSpendStep(closed);
  await closed.context().close();

  const blind = await restoredPage(file, answering({ getSignaturesForAddress: 'down' }));
  await blind.locator('.swap-card[data-state="restored"] .status', { hasText: "Can't read" }).waitFor({ timeout: 30000 });
  await noSpendStep(blind);
  await blind.context().close();

  const never = await restoredPage(file, answering({ getSignaturesForAddress: [] }));
  await never.locator('.swap-card[data-state="lock"], .swap-card[data-state="expired"]').waitFor({ timeout: 30000 });
  await never.context().close();
  step('with its lock closed, the page goes by the lock\'s history, and offers nothing while it can\'t read it');
}

async function l2s() {
  const page = await newPage();
  const file = await startSwap(page, false);
  await page.locator('.pay-address').waitFor();
  await page.screenshot({ path: path.join(shots, 'pay.png') });
  const address = await page.locator('.pay-address').innerText();
  for (const [cls, name] of [['.pay-amount', 'Amount'], ['.pay-address', 'Address']]) {
    await page.locator(cls).click();
    await page.locator('#toast', { hasText: `${name} copied` }).waitFor({ timeout: 2000 });
    const copied = await page.evaluate(() => navigator.clipboard.readText());
    const shown = await page.locator(cls).innerText();
    if (cls === '.pay-address' ? copied !== shown : !(shown.startsWith(`${copied} `) && /^\d+\.\d{8}$/.test(copied))) throw new Error(`${shown} copied ${copied}`);
  }
  step('clicking the amount or the address copies it, with a notification');
  await pay(address, amountSats(env.AMOUNT), env.USDT);
  step(`paid ${env.AMOUNT} from the node's wallet to the address the page derived`);
  await waitState(page, 'ready_claim');
  await page.screenshot({ path: path.join(shots, 'claim.png') });
  step('the page signed the maker’s claim, and sees its lock');
  const setHelius = async (key) => {
    await page.locator('.swap-card .gear').click();
    await page.locator('#s-helius').fill(key);
    await page.locator('dialog .btn', { hasText: 'Save' }).click();
  };
  await setHelius('test-key');
  await page.locator('.one-endpoint').waitFor({ state: 'detached', timeout: 5000 });
  await setHelius('');
  await page.locator('.one-endpoint').waitFor({ timeout: 5000 });
  step('Settings opens from the claim page, and a second endpoint set there takes effect at once');
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
  await checkHistory(page);
  await checkFinishedRestore(file);
  step('restoring a finished swap from its file shows it finished, and never offers to pay again');
  await checkNoErrors(page);
  await page.context().close();
}

async function s2l() {
  const page = await newPage();
  const to = await walletCli('getnewaddress');
  const file = await startSwap(page, true, to);
  await page.locator('.lock').waitFor();
  await page.screenshot({ path: path.join(shots, 'lock.png') });
  await page.locator('.lock').click();
  step('locked on Solana');
  loseBroadcastReply = true;
  await waitState(page, 'done');
  if (loseBroadcastReply) throw new Error('the claim was never published through the page');
  await page.screenshot({ path: path.join(shots, 's2l-done.png') });
  step('the page signed with the maker, and its claim confirmed on Liquid, even with the reply to its broadcast lost');
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
  await checkFinishedRestore(file);
  step('restoring a finished swap from its file shows it finished, and never offers to lock again');
  await checkClosedLock(file);
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
  await fresh.locator('#rescue-file').focus();
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
  loseBroadcastReply = true;
  await fresh.locator('.refund').click();
  await fresh.locator('.swap-card[data-state="refunding"], .swap-card[data-state="refunded"]').waitFor({ timeout: 30000 });
  if (loseBroadcastReply) throw new Error('the refund was never published through the page');
  step('a refund still goes through with the reply to its broadcast lost');
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
  if (await page.locator('#unvetted-warning').isVisible()) throw new Error('a warning with the switch on');
  await connectTestWallet(page);
  await page.locator('#amount').fill(env.AMOUNT);
  if (await page.locator('#go:not(:disabled)').count()) throw new Error('an order could go to a hidden maker');
  if (await page.locator('#send-max').isVisible()) throw new Error('send limits from makers that are hidden');
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

  // Now the other maker too, which the later scenarios split orders across.
  const everyone = await newPage();
  await everyone.locator('#only-vetted').uncheck();
  await everyone.locator('tbody tr').nth(1).waitFor({ timeout: 30000 });
  const keys = await everyone.locator('tbody td.mono').evaluateAll((all) => all.map((td) => td.title));
  if (!keys.includes(key) || keys.length !== 2) throw new Error(`the makers: ${keys}`);
  await everyone.context().close();
  vetted = keys;
  const listed = await newPage();
  await listed.locator('tbody tr .vetted').nth(1).waitFor({ timeout: 30000 });
  await listed.context().close();
  step('once listed, the maker has its checkmark with the switch on');
}

// The library is held back here, so this shows the dialog doesn't wait for it, and needs no relay.
async function walletConnectWait() {
  const page = await newPage();
  let release;
  const held = new Promise((r) => { release = r; });
  await page.route('**/vendor/walletconnect.js', async (route) => { await held; await route.abort(); });
  await page.evaluate(() => localStorage.setItem('test.refuseConnect', 'broken'));
  await page.locator('#go', { hasText: 'Connect a Solana wallet' }).click();
  await page.locator('dialog .spinner').waitFor({ timeout: 2000 });
  const turning = await page.locator('dialog .spinner').evaluate((e) => getComputedStyle(e).animationName);
  if (turning !== 'spin') throw new Error(`the spinner isn't animated: ${turning}`);
  release();
  await page.locator('dialog').waitFor({ state: 'detached', timeout: 10000 });
  await page.locator('#banner').waitFor({ timeout: 5000 });
  await page.context().close();
  step('a wallet that fails falls back to WalletConnect, whose spinner shows at once, and a failure closes it with a message');
}

// Nothing is rebuilt: marks put on the elements must outlast typing, a flip, new offers and the page's own timers.
async function liveBook() {
  const page = await newPage();
  const row = page.locator('tbody tr').first();
  await row.waitFor({ timeout: 30000 });
  const headers = async () => (await page.locator('thead th').allInnerTexts()).map((t) => t.toLowerCase());
  const cells = async () => row.locator('td').allInnerTexts();
  if ((await headers()).join() !== 'maker,price,per swap') throw new Error(`columns: ${await headers()}`);
  const quoted = await cells();
  const steps = quoted[1].split('\n');
  if (!quoted[2].includes('–') || steps.length < 2 || !steps.every((t) => /^≤[\d,.]+: \d\.\d{4}$/.test(t))) throw new Error(`before an order: ${quoted}`);
  const mark = () => page.evaluate(() => {
    for (const s of ['.card', '.book', 'tbody tr', 'tbody tr td:nth-child(2)', '#amount', '#go']) document.querySelector(s).dataset.mark = '1';
  });
  const marked = () => page.evaluate(() => ['.card', '.book', 'tbody tr', 'tbody tr td:nth-child(2)', '#amount', '#go'].filter((s) => !document.querySelector(s).dataset.mark));
  await mark();

  await page.locator('#amount').pressSequentially(env.AMOUNT);
  if ((await page.locator('#amount').inputValue()) !== env.AMOUNT) throw new Error('typing was lost');
  if (!(await page.locator('#amount').evaluate((e) => e === document.activeElement))) throw new Error('typing lost focus');
  if ((await headers()).join() !== 'maker,price,your fill') throw new Error(`columns with an order: ${await headers()}`);
  const ordered = await cells();
  const average = await page.locator('.facts dd').nth(1).innerText();
  if (ordered[1] !== average) throw new Error(`the row's price ${ordered[1]} isn't the order's ${average}`);
  if (!/^\d+\.\d{2}$/.test(ordered[2]) || ordered[2] === '0.00') throw new Error(`the fill: ${ordered[2]}`);

  await new Promise((r) => setTimeout(r, 6000));
  await page.locator('.flip').click();
  await page.locator('.flip').click();
  let lost = await marked();
  if (lost.length) throw new Error(`rebuilt: ${lost}`);

  await page.locator('#amount').fill('');
  if ((await headers())[2] !== 'per swap') throw new Error('the caption did not come back');
  if ((await cells())[1] !== quoted[1]) throw new Error('the quoted price did not come back');
  lost = await marked();
  if (lost.length) throw new Error(`rebuilt: ${lost}`);

  for (const [link, name] of [['#send-min', 'Send min'], ['#send-max', 'Send max']]) {
    const shown = await page.locator(link).innerText();
    await page.locator(link).click();
    const amount = await page.locator('#amount').inputValue();
    if (!shown || Number(shown.replaceAll(',', '')) !== Number(amount)) throw new Error(`${name} shows ${shown} but put ${amount} in the form`);
    if (await page.locator('#unfilled').isVisible()) throw new Error(`${name} left part of the order unfilled`);
    if (!(await page.locator('.facts').isVisible())) throw new Error(`no maker takes ${name}`);
    if ((await headers())[2] !== 'your fill') throw new Error(`${name} did not count as an order`);
  }
  await page.locator('#amount').fill('');
  lost = await marked();
  if (lost.length) throw new Error(`rebuilt: ${lost}`);
  await checkNoErrors(page);
  await page.context().close();
  step('send min and max put an order in the form that the book fills completely');
  step('the book updates in place, and shows an order\'s own price and size under "your fill"');
}

// The WebAssembly is the slow part of loading, so it is held back here.
async function pageLoading() {
  let release;
  const held = new Promise((r) => { release = r; });
  const page = await newPage(undefined, (p) => p.route('**/*.wasm', async (route) => { await held; await route.fallback(); }));
  await page.locator('.page-loading .spinner').waitFor({ timeout: 5000 });
  if ((await page.locator('.page-loading .spinner').evaluate((e) => getComputedStyle(e).animationName)) !== 'spin') throw new Error('the spinner isn\'t animated');
  if (await page.locator('#amount').count()) throw new Error('the form appeared before it could work');
  const link = page.locator('footer a').first();
  if ((await link.getAttribute('target')) !== '_blank') throw new Error('the build link stays in this tab');
  if (!(await link.getAttribute('href')).includes('/commit/')) throw new Error('the build link isn\'t to a commit');
  release();
  await page.locator('#amount').waitFor({ timeout: 15000 });
  if (await page.locator('.page-loading').count()) throw new Error('the placeholder stayed');
  await page.context().close();

  const broken = await newPage(undefined, (p) => p.route('**/*.wasm', (route) => route.abort()));
  await broken.locator('#banner').waitFor({ timeout: 15000 });
  await broken.locator('.page-loading').waitFor({ state: 'detached', timeout: 5000 });
  await broken.context().close();
  step('a spinner shows while the page loads, and goes away if loading fails');
}

// Reloading mid-swap shouldn't mean connecting again, or a prompt, and disconnecting has to stick.
async function walletStays() {
  const page = await newPage();
  const connected = page.getByRole('link', { name: 'disconnect' });
  const button = page.locator('#go', { hasText: 'Connect a Solana wallet' });
  await button.waitFor({ timeout: 15000 });
  if (await button.isDisabled()) throw new Error('the connect button should be clickable before any amount');
  await button.click();
  await connected.waitFor({ timeout: 5000 });
  await page.reload();
  await connected.waitFor({ timeout: 15000 });
  if (!(await page.evaluate(() => window.testWalletSilent))) throw new Error('the reconnect could have shown the wallet\'s prompt');

  await page.evaluate(() => localStorage.setItem('test.refuseSilent', '1'));
  await page.reload();
  await button.waitFor({ timeout: 15000 });
  await new Promise((r) => setTimeout(r, 1000));
  if (await connected.isVisible()) throw new Error('connected though the wallet refused');
  if (await page.locator('#banner').isVisible()) throw new Error('a refused reconnect should be quiet');
  await page.evaluate(() => localStorage.removeItem('test.refuseSilent'));

  await button.click();
  await connected.click();
  await page.reload();
  await button.waitFor({ timeout: 15000 });
  await new Promise((r) => setTimeout(r, 4000));
  if (await connected.isVisible()) throw new Error('disconnecting did not stick');
  await page.context().close();
  step('the wallet reconnects silently after a reload, unless refused or disconnected');
}

// The wallet comes up short, as the Solana endpoint reports it, so no quote should even be asked for.
async function s2lBalance() {
  for (const [method, empty, want] of [
    ['getAccountInfo', { value: null }, /holds 0 SOL-USDT, and this order needs/],
    ['getBalance', { value: 0 }, /needs a little SOL to pay for each lock/],
  ]) {
    const page = await newPage();
    await page.route((url) => url.href.startsWith(env.SOLANA_RPC), async (route) => {
      const body = route.request().postDataJSON();
      if (body?.method !== method) return route.fallback();
      return route.fulfill({ json: { jsonrpc: '2.0', id: body.id, result: { context: { slot: 1 }, ...empty } } });
    });
    await connectTestWallet(page);
    await page.locator('.flip').click();
    await page.locator('tbody tr').first().waitFor({ timeout: 30000 });
    await page.locator('#amount').fill(env.AMOUNT);
    await page.locator('#to').fill('tlq1x');
    await page.locator('#go:not(:disabled)').click();
    await page.locator('#banner:not([hidden])', { hasText: want }).waitFor({ timeout: 10000 });
    await page.locator('#go').waitFor({ timeout: 3000 });
    if (await page.locator('#accept').count()) throw new Error('quoted an order the wallet can\'t pay');
    await page.context().close();
  }
  step('S to L stops before a quote when the wallet is short of SOL-USDT or SOL');
}

async function phoneWallets() {
  const page = await newPage({ width: 400, height: 800 }, null, true);
  await page.locator('.connect-wallet, #go', { hasText: 'Connect a Solana wallet' }).first().click();
  const links = page.locator('.phone-wallets .open-wallet');
  await links.first().waitFor({ timeout: 15000 });
  const hrefs = await links.evaluateAll((all) => all.map((a) => a.href));
  const here = encodeURIComponent(page.url());
  if (hrefs.length !== 2 || !hrefs[0].startsWith(`https://phantom.app/ul/browse/${here}`) || !hrefs[1].startsWith(`https://solflare.com/ul/v1/browse/${here}`)) throw new Error(`the links: ${hrefs}`);
  await page.locator('.phone-wallets').getByRole('button', { name: 'WalletConnect' }).waitFor({ timeout: 2000 });
  await page.screenshot({ path: path.join(shots, 'phone-wallets.png') });
  await page.context().close();
  step('a phone with no wallet in its browser gets a list of wallets, and WalletConnect');
}

// The order to try, and when to stop. Phantom registers beside the test wallet here, which would otherwise be first.
async function connectOrder() {
  const page = await newPage(undefined, (p) => p.addInitScript(() => {
    const account = { address: '11111111111111111111111111111111', publicKey: new Uint8Array(32), chains: ['solana:devnet'], features: [] };
    const phantom = {
      version: '1.0.0', name: 'Phantom', icon: 'data:image/svg+xml,<svg xmlns="http://www.w3.org/2000/svg"/>', chains: ['solana:devnet'], accounts: [account],
      features: {
        'standard:connect': { version: '1.0.0', connect: async () => { if (localStorage.getItem('test.phantom') === 'no') throw Object.assign(new Error('no'), { code: 4001 }); return { accounts: [account] }; } },
        'solana:signTransaction': { version: '1.0.0', signTransaction: async () => { throw new Error('unused'); } },
      },
    };
    const register = ({ register: r }) => r(phantom);
    window.addEventListener('wallet-standard:app-ready', ({ detail }) => register(detail));
    window.dispatchEvent(new CustomEvent('wallet-standard:register-wallet', { detail: register }));
  }));
  const go = page.locator('#go', { hasText: 'Connect a Solana wallet' });
  await page.evaluate(() => localStorage.setItem('test.phantom', 'no'));
  await go.click();
  await page.locator('#banner:not([hidden])', { hasText: 'Phantom declined' }).waitFor({ timeout: 5000 });
  if (await page.getByRole('link', { name: 'disconnect' }).count()) throw new Error('moved on to another wallet after a person said no');
  await page.evaluate(() => localStorage.removeItem('test.phantom'));
  await go.click();
  await page.locator('.note', { hasText: 'Solana: Phantom' }).waitFor({ timeout: 5000 });
  await page.context().close();
  step('Phantom is tried first, and saying no to a wallet stops there');
}

// The maker locks as soon as it has the page's signature, so the page's messages are held back until it asks for its refund.
// A maker that signed never locks, and the coin goes back on the key path, looking like any claim.
async function early() {
  let holding = false;
  const page = await newPage(undefined, (p) => p.routeWebSocket(/.*/, (ws) => {
    const relay = ws.connectToServer();
    ws.onMessage((m) => { if (!(holding && String(m).startsWith('["EVENT"'))) relay.send(m); });
    relay.onMessage((m) => ws.send(m));
  }));
  await startSwap(page, false);
  await page.locator('.pay-address').waitFor();
  holding = true;
  await pay(await page.locator('.pay-address').innerText(), amountSats(env.AMOUNT), env.USDT);
  await page.locator('.early-refund').waitFor({ timeout: 120000 });
  await page.locator('.early-refund').click();
  const fee = await page.locator('.fee-address').innerText();
  await pay(fee, 30, env.LBTC);
  await walletCli('generatetoaddress', '1', await walletCli('getnewaddress'));
  const to = await walletCli('getnewaddress');
  await page.locator('.refund-to').fill(to);
  holding = false;
  // The fee coin can take a moment to show up.
  for (let i = 0; i < 20; i++) {
    await page.locator('.early-go').click();
    await page.locator('.swap-card:not([data-state="early_form"])').waitFor({ timeout: 2000 }).catch(() => {});
    if (await state(page) !== 'early_form') break;
    await page.locator('.refund-to').fill(to);
  }
  step('asked for a refund before the maker locked');
  await page.locator('.swap-card[data-state="refunded"]').waitFor({ timeout: 120000 });
  const swap = await page.evaluate(() => Object.values(JSON.parse(localStorage.getItem('liq2sol.swaps')))[0]);
  const tx = JSON.parse(await cli('getrawtransaction', swap.refund_txid, '1'));
  if (tx.vin[0].txinwitness.length !== 1) throw new Error(`the refund's witness has ${tx.vin[0].txinwitness.length} items, so it shows it was a refund`);
  const got = await receivedAt(to);
  if (got !== Number(swap.terms.liquid_amount)) throw new Error(`refunded ${got} units, expected ${swap.terms.liquid_amount}`);
  step('the maker agreed, and the refund paid it all back, as one signature, long before the lock height');
  await checkNoErrors(page);
  await page.context().close();
}

async function solanaRpc(method, params) {
  const res = await fetch(env.SOLANA_RPC, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) });
  return (await res.json()).result;
}

function freshWallet() {
  const { publicKey, privateKey: key } = generateKeyPairSync('ed25519');
  return { key, address: base58(publicKey.export({ format: 'der', type: 'spki' }).subarray(-32)) };
}

// The real faucet program on the validator, with a wallet that has never claimed, so it works on every run.
async function faucet() {
  const broke = freshWallet();
  const noSol = await newPage(undefined, null, false, broke);
  await noSol.goto(`http://127.0.0.1:${site.port}/#faucet`);
  await noSol.locator('#sideswap-note', { hasText: 'SideSwap' }).waitFor({ timeout: 15000 });
  if (!(await noSol.locator('.card.faucet .addr').innerText()).includes(env.USDT)) throw new Error('the page should name the Liquid asset');
  await noSol.locator('#faucet-connect').click();
  await noSol.locator('#drip').click();
  await noSol.locator('#banner:not([hidden])', { hasText: 'devnet SOL first' }).waitFor({ timeout: 10000 });
  await noSol.context().close();

  const who = freshWallet();
  await solanaRpc('requestAirdrop', [who.address, 1000000000]);
  for (let i = 0; i < 60 && !(await solanaRpc('getBalance', [who.address, { commitment: 'confirmed' }]))?.value; i++) await new Promise((r) => setTimeout(r, 500));
  const page = await newPage(undefined, null, false, who);
  await page.goto(`http://127.0.0.1:${site.port}/#faucet`);
  await page.locator('#faucet-connect').click();
  await page.locator('#drip').click();
  await page.locator('#faucet-balance', { hasText: '100 SOL-USDT' }).waitFor({ timeout: 60000 });
  step('a new wallet gets its test USDT from the faucet program');
  const spent = page.locator('#drip:disabled', { hasText: 'Already claimed' });
  await spent.waitFor({ timeout: 5000 });
  await page.reload();
  await page.locator('#sideswap-note').waitFor({ timeout: 15000 });
  await page.locator('#drip').first().waitFor({ timeout: 15000 });
  await spent.waitFor({ timeout: 15000 });
  await checkNoErrors(page);
  await page.context().close();
  step('it says when a wallet has no SOL, stays disabled after a claim, even after a reload, and names the asset SideSwap uses');
}

async function look() {
  await phoneWallets();
  await connectOrder();
  await s2lBalance();
  await walletStays();
  await pageLoading();
  await walletConnectWait();
  await liveBook();
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
  for (const name of ['vetting', ...(env.BROWSER_SCENARIOS ?? 'look,faucet,l2s,s2l,split,rescue,early').split(',')]) {
    console.log(`  == browser: ${name}`);
    await { vetting, look, faucet, l2s, s2l, rescue, split, early }[name]();
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
