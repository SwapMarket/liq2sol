// The page. It shows state and takes decisions from the user. js/swap.js and the WASM do the protocol.
import init, * as W from '../wasm/liq2sol_client_wasm.js';
import { NETWORK, refuseMainnet } from './config.js';
import { Esplora, Solana } from './chain.js';
import { Relays } from './relay.js';
import { Engine, FINISHED, deriveSwap, rulesJson } from './swap.js';
import { loadSettings, loadSwaps, removeSwap, saveSettings, saveSwap } from './store.js';
import { makeRecovery, offerDownload, readRecovery } from './recovery.js';
import { connectExtension, connectWalletConnect, extensionWallets } from './wallet.js';

const OFFER_KIND = 38401;
const WRAP_KIND = 21059;
const $ = (id) => document.getElementById(id);
const now = () => Math.floor(Date.now() / 1000);

let settings = loadSettings();
let wallet = null;
let relays = null;
let engine = null;
let esplora = [];
let solana = null;
const offers = new Map();
const negotiation = { current: null };
const ui = { direction: 'L2S', amount: '', to: '', selected: null, error: '', openSwap: null, quote: null };

// ---- Small DOM helper. Text always goes in as text, so nothing from a relay can become markup. ----
function h(tag, props = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props ?? {})) {
    if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k === 'class') el.className = v;
    else if (k in el && k !== 'list') el[k] = v;
    else el.setAttribute(k, v);
  }
  for (const c of children.flat()) if (c !== null && c !== undefined && c !== false) el.append(c);
  return el;
}

const units = (text) => {
  const m = /^(\d*)(?:\.(\d{0,6}))?$/.exec(text.trim());
  if (!m || (!m[1] && !m[2])) return null;
  return BigInt(m[1] || '0') * 1000000n + BigInt((m[2] || '').padEnd(6, '0'));
};
const show = (u) => {
  const v = BigInt(u);
  const frac = (v % 1000000n).toString().padStart(6, '0').replace(/0+$/, '');
  return `${(v / 1000000n).toLocaleString('en-US')}${frac ? `.${frac}` : ''}`;
};
const liquidUnits = (u) => {
  const v = BigInt(u);
  return `${v / 100000000n}.${(v % 100000000n).toString().padStart(8, '0')}`;
};
const short = (s) => `${s.slice(0, 8)}…${s.slice(-6)}`;
const names = { L2S: ['L-USDt', 'SOL-USDT'], S2L: ['SOL-USDT', 'L-USDt'] };

function coinPill(name) {
  const liquid = name.startsWith('L');
  return h('span', { class: 'pill' }, h('span', { class: `coin ${liquid ? 'lq' : 'sol'}` }, liquid ? 'L' : 'S'), name);
}

function fail(e) {
  const banner = $('banner');
  banner.textContent = (e?.message ?? String(e)).split('api-key=')[0];
  banner.hidden = false;
  clearTimeout(fail.timer);
  fail.timer = setTimeout(() => { banner.hidden = true; }, 15000);
}

// ---- Connections ----
function endpoints() {
  esplora = (settings.esplora ?? NETWORK.esplora).filter(Boolean).map((u) => new Esplora(u));
  solana = new Solana(settings.solana || NETWORK.solana);
}

// The second Solana source is Helius, with a key only the user holds, and never one shipped in this page.
function secondSolana() {
  if (settings.solana2) return new Solana(settings.solana2);
  return settings.heliusKey ? new Solana(NETWORK.heliusTemplate + encodeURIComponent(settings.heliusKey)) : null;
}

function startRelays() {
  relays?.close();
  relays = new Relays(settings.relays ?? NETWORK.relays, onEvent);
  relays.subscribe('offers', { kinds: [OFFER_KIND], since: now() - 3600 });
  for (const s of Object.values(loadSwaps())) watch(s);
  engine.relays = relays;
}

function watch(s) {
  if (FINISHED.includes(s.state)) return;
  relays.subscribe(`swap-${s.id.slice(0, 12)}`, { kinds: [WRAP_KIND], '#p': [s.nostr.public], since: now() - 300 });
}

function onEvent(event) {
  const text = JSON.stringify(event);
  if (event.kind === OFFER_KIND) {
    const got = JSON.parse(W.read_offer(text, NETWORK.name, BigInt(now())));
    if (!got) return;
    const known = offers.get(got.maker);
    if (!known || known.created_at <= got.created_at) offers.set(got.maker, got);
    // Messages carry the time, so a clock far from a maker's would get them refused.
    if (Math.abs(now() - got.created_at) > 3600 * 24) return;
    render();
    return;
  }
  if (event.kind !== WRAP_KIND) return;
  const to = event.tags.find((t) => t[0] === 'p')?.[1];
  if (negotiation.current?.nostr.public === to) {
    const got = JSON.parse(W.unwrap_message(negotiation.current.nostr.secret, text, NETWORK.name, BigInt(now())));
    if (got) negotiation.current.onMessage(got);
    return;
  }
  const s = Object.values(loadSwaps()).find((x) => x.nostr.public === to && !FINISHED.includes(x.state));
  if (!s) return;
  const got = JSON.parse(W.unwrap_message(s.nostr.secret, text, NETWORK.name, BigInt(now())));
  if (got) engine.onMessage(s, got).catch(fail);
}

// ---- The makers book ----
const liveOffers = (direction) => [...offers.values()]
  .filter((o) => o.offer.directions.includes(direction))
  .sort((a, b) => bps(a, direction) - bps(b, direction));
const bps = (o, direction) => (direction === 'L2S' ? o.offer.l2s_bps : o.offer.s2l_bps);
const available = (o, direction) => BigInt(direction === 'L2S' ? o.offer.available_l2s : o.offer.available_s2l);
const receiveAt = (o, direction, send) => (send * BigInt(10000 - bps(o, direction))) / 10000n;

function fits(o, direction, send) {
  if (send === null || send <= 0n) return true;
  const r = receiveAt(o, direction, send);
  return send >= BigInt(o.offer.min_sol_units) && send <= BigInt(o.offer.max_sol_units)
    && r >= BigInt(o.offer.min_sol_units) && r <= available(o, direction);
}

function chosenOffer(send) {
  const eligible = liveOffers(ui.direction).filter((o) => fits(o, ui.direction, send));
  return eligible.find((o) => o.maker === ui.selected) ?? eligible[0] ?? null;
}

function bookView(send) {
  const list = liveOffers(ui.direction);
  const chosen = chosenOffer(send);
  const rows = list.map((o) => {
    const ok = fits(o, ui.direction, send);
    return h('tr', {
      class: `${!ok ? 'off' : ''} ${chosen?.maker === o.maker ? 'on' : ''}`,
      onclick: () => { if (ok) { ui.selected = o.maker; render(); } },
    },
    h('td', { class: 'mono' }, short(o.maker)),
    h('td', {}, `${(bps(o, ui.direction) / 100).toFixed(2)}%`),
    h('td', {}, `${show(o.offer.min_sol_units)} – ${show(o.offer.max_sol_units)}`),
    h('td', {}, show(available(o, ui.direction))),
    h('td', {}, send && send > 0n && ok ? show(receiveAt(o, ui.direction, send)) : '–'));
  });
  return h('div', { class: 'book' },
    h('h3', {}, `Makers book: ${names[ui.direction].join(' → ')}`),
    h('div', { class: 'scroll' }, rows.length
      ? h('table', {},
        h('thead', {}, h('tr', {}, ['Maker', 'Spread', 'Per swap', 'Available', 'You receive'].map((t) => h('th', {}, t)))),
        h('tbody', {}, rows))
      : h('div', { class: 'empty' }, relays?.connected() ? 'No maker is offering this direction right now.' : 'Connecting to Nostr relays…')));
}

// ---- Wallet ----
function walletView() {
  if (wallet) {
    return h('p', { class: 'note' }, `Solana: ${wallet.name} ${short(wallet.address)} `,
      h('a', { href: '#', onclick: (e) => { e.preventDefault(); wallet = null; render(); } }, 'disconnect'));
  }
  const choose = async (connect) => {
    try {
      wallet = await connect();
      render();
    } catch (e) {
      fail(e);
    }
  };
  return h('div', { class: 'wallets' },
    extensionWallets().map((w) => h('button', { class: 'btn quiet', onclick: () => choose(() => connectExtension(w)) }, w.name)),
    h('button', { class: 'btn quiet', onclick: () => choose(walletConnect) }, 'WalletConnect'));
}

async function walletConnect() {
  const { qrcode } = await import('../vendor/qrcode.js');
  const box = h('div', { class: 'qr' });
  const dialog = h('dialog', {}, h('h3', {}, 'Scan with your Solana wallet'), box,
    h('button', { class: 'btn quiet', onclick: () => dialog.close() }, 'Close'));
  document.body.append(dialog);
  try {
    return await connectWalletConnect(settings.walletConnectProject || NETWORK.walletConnectProject, (uri) => {
      const qr = qrcode(0, 'L');
      qr.addData(uri);
      qr.make();
      box.innerHTML = qr.createSvgTag({ cellSize: 3, margin: 2, scalable: true });
      dialog.showModal();
    });
  } finally {
    dialog.close();
    dialog.remove();
  }
}

// ---- Starting a swap ----
function formView() {
  const send = units(ui.amount);
  const [from, to] = names[ui.direction];
  const offer = chosenOffer(send);
  const received = offer && send ? receiveAt(offer, ui.direction, send) : null;
  const needsAddress = ui.direction === 'S2L';
  const ready = send && send > 0n && offer && wallet && (!needsAddress || ui.to.trim());
  const label = !send || send <= 0n ? 'Enter an amount' : !offer ? 'No maker fits this amount' : !wallet ? 'Connect a Solana wallet' : needsAddress && !ui.to.trim() ? 'Enter a Liquid address' : 'Get a firm quote';
  const receive = h('input', { class: 'amount', readOnly: true, value: received === null ? '' : show(received), placeholder: '0', 'aria-label': `You receive, in ${to}` });
  return h('div', { class: 'card' },
    h('button', { class: 'gear', title: 'Settings', 'aria-label': 'Settings', onclick: settingsDialog }, '⚙'),
    h('h2', {}, 'Create Atomic Swap'),
    h('p', { class: 'sub' }, 'Trustless and confidential. No server holds anything.'),
    h('div', { class: 'row' }, coinPill(from),
      h('input', {
        class: 'amount', inputMode: 'decimal', placeholder: '0', value: ui.amount, id: 'amount', 'aria-label': `You send, in ${from}`,
        oninput: (e) => { ui.amount = e.target.value; refreshForm(); },
      })),
    h('button', { class: 'flip', title: 'Swap direction', 'aria-label': 'Swap direction', onclick: () => { ui.direction = ui.direction === 'L2S' ? 'S2L' : 'L2S'; ui.selected = null; render(); } }, '⇅'),
    h('div', { class: 'row to' }, coinPill(to), receive),
    needsAddress && h('label', { class: 'field' }, h('span', {}, 'Your Liquid address, confidential'),
      h('input', { class: 'input', id: 'to', value: ui.to, placeholder: 'tlq1…', oninput: (e) => { ui.to = e.target.value; refreshForm(); } })),
    walletView(),
    offer && send && h('dl', { class: 'facts' },
      h('div', {}, h('dt', {}, 'Maker'), h('dd', { class: 'mono' }, short(offer.maker))),
      h('div', {}, h('dt', {}, 'Spread'), h('dd', {}, `${(bps(offer, ui.direction) / 100).toFixed(2)}%`))),
    h('button', { class: 'btn', id: 'go', disabled: !ready, onclick: startQuote }, label));
}

// Typing shouldn't rebuild the input under the cursor.
function refreshForm() {
  const focus = document.activeElement?.id;
  const at = document.activeElement?.selectionStart;
  render();
  if (focus && $(focus)) {
    $(focus).focus();
    if (at !== undefined && at !== null) $(focus).setSelectionRange?.(at, at);
  }
}

function waitFor(pred, ms) {
  return new Promise((resolve, reject) => {
    const n = negotiation.current;
    const timer = setTimeout(() => reject(new Error('The maker did not answer in time. Nothing has been committed.')), ms);
    n.onMessage = (got) => {
      if (got.sender !== n.maker || got.id !== n.id) return;
      if (got.body.type === 'rfq-reject') {
        clearTimeout(timer);
        reject(new Error(`The maker declined: ${String(got.body.body.reason).slice(0, 200)}`));
      } else if (pred(got)) {
        clearTimeout(timer);
        resolve(got);
      }
    };
  });
}

async function startQuote() {
  const send = units(ui.amount);
  const offer = chosenOffer(send);
  try {
    if (ui.direction === 'L2S') {
      if (await solana.lamports(wallet.address) < NETWORK.minSolLamports) throw new Error('This wallet needs a little SOL to pay for the claim, so a swap could not finish. Nothing has been sent.');
    }
    const nostr = JSON.parse(W.new_nostr_key());
    const id = JSON.parse(W.new_nostr_key()).secret;
    negotiation.current = { id, nostr, maker: offer.maker, onMessage: () => {} };
    relays.subscribe('negotiation', { kinds: [WRAP_KIND], '#p': [nostr.public], since: now() - 300 });
    await new Promise((r) => setTimeout(r, 300));
    const request = { direction: ui.direction, amount_side: 'send', amount: send.toString() };
    const answered = waitFor((g) => g.body.type === 'rfq-quote', 20000);
    const { event } = JSON.parse(W.wrap_message(nostr.secret, offer.maker, id, NETWORK.name, JSON.stringify({ type: 'rfq-request', body: request }), BigInt(now())));
    relays.publish(event);
    ui.quote = { status: 'Asking the maker for a firm price…' };
    render();
    const got = await answered;
    const quote = got.body.body;
    const problem = W.quote_problem(rulesJson(settings.maxSpreadBps ?? NETWORK.maxSpreadBps), JSON.stringify(request), JSON.stringify(quote), BigInt(now()));
    if (problem) throw new Error(`This quote isn't acceptable: ${problem}.`);
    ui.quote = { quote, rumor: got.rumor_id, request, offer, direction: ui.direction, to: ui.to.trim(), owner: wallet.address };
    render();
  } catch (e) {
    ui.quote = null;
    negotiation.current = null;
    render();
    fail(e);
  }
}

function quoteView() {
  const q = ui.quote;
  if (q.status) return h('div', { class: 'card' }, h('h2', {}, 'Getting a quote'), h('p', { class: 'status spin' }, q.status));
  const [from, to] = names[q.direction];
  const left = Math.max(0, q.quote.quote_expiry - now());
  return h('div', { class: 'card' },
    h('h2', {}, 'Review the quote'),
    h('dl', { class: 'facts' },
      h('div', {}, h('dt', {}, 'You send'), h('dd', {}, `${show(q.quote.send)} ${from}`)),
      h('div', {}, h('dt', {}, 'You receive'), h('dd', {}, `${show(q.quote.receive)} ${to}`)),
      h('div', {}, h('dt', {}, 'Maker'), h('dd', { class: 'mono' }, short(q.offer.maker))),
      h('div', {}, h('dt', {}, 'If the maker never locks'), h('dd', {}, q.direction === 'L2S' ? 'Refund after about 5 hours' : 'Refund after about 6 hours')),
      h('div', {}, h('dt', {}, 'Price holds for'), h('dd', {}, `${left} s`))),
    q.direction === 'L2S' && h('p', { class: 'note' }, 'You will need a little L-BTC only if a refund is ever needed.'),
    h('button', { class: 'btn', id: 'accept', onclick: acceptQuote }, 'Accept'),
    h('button', { class: 'btn quiet', onclick: () => { ui.quote = null; negotiation.current = null; render(); } }, 'Cancel'));
}

async function acceptQuote() {
  const q = ui.quote;
  try {
    const n = negotiation.current;
    const secrets = JSON.parse(W.new_swap_secrets());
    const accept = {
      quote_event: q.rumor, lock_point: secrets.lock_point, taker_key: secrets.taker_key,
      blinding_key: secrets.blinding_key, taker_solana: q.owner,
    };
    const answered = waitFor((g) => g.body.type === 'rfq-terms', 20000);
    const { event, rumor_id: rumor } = JSON.parse(W.wrap_message(n.nostr.secret, n.maker, n.id, NETWORK.name, JSON.stringify({ type: 'rfq-accept', body: accept }), BigInt(now())));
    relays.publish(event);
    ui.quote = { status: 'Fixing the terms with the maker…' };
    render();
    const got = await answered;
    const terms = got.body.body;
    const tip = await esplora[0].tip();
    const problem = W.terms_problem(rulesJson(settings.maxSpreadBps ?? NETWORK.maxSpreadBps), q.direction, JSON.stringify(q.quote), rumor, JSON.stringify(terms), tip);
    if (problem) throw new Error(`The maker's terms aren't acceptable, so nothing has been sent: ${problem}.`);
    const swap = deriveSwap({
      id: n.id, created_at: now(), direction: q.direction, maker: n.maker, nostr: n.nostr, secrets, terms: {
        ...terms, liquid_amount: terms.liquid_amount, solana_amount: terms.solana_amount,
      },
      fee_coin_sats: q.offer.offer.fee_coin_sats, solana_owner: q.owner, to_address: q.to, state: 'recovery', status: '',
    });
    saveSwap(swap);
    watch(swap);
    negotiation.current = null;
    ui.quote = null;
    ui.openSwap = swap.id;
    render();
  } catch (e) {
    ui.quote = null;
    negotiation.current = null;
    render();
    fail(e);
  }
}

// ---- A swap under way ----
const FLOW = {
  L2S: [['recovery', 'Save the recovery file'], ['fund', 'Pay the swap address'], ['await_maker', 'The maker locks on Solana'], ['ready_claim', 'Claim on Solana'], ['done', 'Done']],
  S2L: [['recovery', 'Save the recovery file'], ['lock', 'Lock on Solana'], ['await_maker', 'The maker locks on Liquid'], ['signing', 'Sign the claim together'], ['claiming', 'Publish the claim'], ['done', 'Done']],
};

function stepsView(s) {
  const flow = FLOW[s.direction];
  const at = flow.findIndex(([k]) => k === s.state);
  return h('ol', { class: 'steps' }, flow.map(([k, text], i) => h('li', { class: i < at || s.state === 'done' ? 'done' : i === at ? 'now' : '' }, text)));
}

function recoveryView(s) {
  const pick = h('input', {
    type: 'file', accept: '.json,application/json', class: 'input', id: 'recovery-file',
    onchange: async (e) => {
      try {
        const read = await readRecovery(await e.target.files[0].text());
        if (read.id !== s.id || read.secrets.lock_secret !== s.secrets.lock_secret) throw new Error('That is a recovery file for a different swap.');
        engine.set(s, { state: s.direction === 'L2S' ? 'fund' : 'lock', recovery_saved: true });
        render();
      } catch (err) {
        fail(err);
      }
    },
  });
  return [
    h('p', { class: 'warn' }, 'This file can move your funds, so treat it like money. It is the only way back if this browser loses its data.'),
    h('button', { class: 'btn', id: 'download', onclick: async () => offerDownload(await makeRecovery(s), `liq2sol-recovery-${s.id.slice(0, 8)}.json`) }, 'Download recovery file'),
    h('label', { class: 'field' }, h('span', {}, 'Then choose the file you saved, to confirm it is intact. The next step stays hidden until you do.'), pick),
  ];
}

async function qrView(text) {
  const { qrcode } = await import('../vendor/qrcode.js');
  const qr = qrcode(0, 'M');
  qr.addData(text);
  qr.make();
  const box = h('div', { class: 'qr' });
  box.innerHTML = qr.createSvgTag({ cellSize: 3, margin: 2, scalable: true });
  return box;
}

function act(fn) {
  return async () => {
    try {
      await fn();
    } catch (e) {
      fail(e);
    }
    render();
  };
}

function refundLiquidView(s) {
  const input = h('input', { class: 'input', id: 'refund-to', placeholder: 'tlq1… confidential address' });
  return [
    h('p', { class: 'note' }, `The refund needs a fee coin. From a Liquid wallet, send ${s.fee_coin_sats ?? 30} sats of L-BTC or a little more to this address. All of it pays the network fee.`),
    h('div', { class: 'addr', id: 'fee-address' }, s.info.fee_address),
    h('label', { class: 'field' }, h('span', {}, 'Where to send your refund'), input),
    h('button', { class: 'btn', id: 'refund', onclick: act(() => engine.refundOnLiquid(s, input.value.trim())) }, 'Refund'),
  ];
}

function swapBody(s) {
  const [from, to] = names[s.direction];
  switch (s.state) {
    case 'recovery':
      return recoveryView(s);
    case 'fund': {
      const box = h('div', {});
      qrView(s.info.address).then((qr) => box.append(qr)).catch(() => {});
      return [
        h('p', { class: 'note' }, `Send exactly this amount of ${from}, once, from any Liquid wallet:`),
        h('div', { class: 'addr', id: 'pay-amount' }, `${liquidUnits(s.terms.liquid_amount)} ${from}`),
        h('div', { class: 'addr', id: 'pay-address' }, s.info.address),
        box,
        s.wrong_coin && h('p', { class: 'warn' }, 'A coin of the wrong asset or amount reached this address. Nobody will act on it, and you can get it back with a refund after the lock height.'),
        h('p', { class: 'note' }, 'The amount stays hidden on-chain only if your wallet pays from blinded coins.'),
        h('p', { class: 'status spin' }, 'Watching for your payment…'),
      ];
    }
    case 'lock':
      return [
        h('p', { class: 'note' }, `Lock ${show(s.terms.solana_amount)} ${from}. The maker then locks ${show(BigInt(s.terms.liquid_amount) / 100n)} ${to} for you.`),
        walletView(),
        h('button', { class: 'btn', id: 'lock', disabled: !wallet, onclick: act(() => engine.lockOnSolana(s)) }, 'Lock on Solana'),
      ];
    case 'await_maker':
      return h('p', { class: 'status spin' }, s.direction === 'L2S'
        ? `Your coin has ${s.coin?.confirmations ?? 0} confirmations. Waiting for the maker to sign and lock on Solana.`
        : (s.status || 'Waiting for the maker to lock on Liquid.'));
    case 'ready_claim':
      return [
        h('p', { class: 'good' }, `The maker locked ${show(s.terms.solana_amount)} ${to} for you.`),
        !secondSolana() && h('label', { class: 'check' },
          h('input', { type: 'checkbox', id: 'one-endpoint', checked: !!settings.allowOneEndpoint, onchange: (e) => { settings.allowOneEndpoint = e.target.checked; saveSettings(settings); } }),
          'Only one Solana endpoint is set, so a lying endpoint could fool this page into revealing the secret. Claim anyway. Add a free Helius key in Settings to check two.'),
        walletView(),
        h('button', { class: 'btn', id: 'claim', disabled: !wallet, onclick: act(() => engine.claimOnSolana(s)) }, 'Claim on Solana'),
        s.status && h('p', { class: 'status' }, s.status),
      ];
    case 'signing':
      return h('p', { class: 'status spin' }, s.status || 'Signing the claim together with the maker…');
    case 'claiming':
      return h('p', { class: 'status spin' }, s.status || 'Publishing the claim…');
    case 'done':
      return h('p', { class: 'good', id: 'done' }, s.status || 'Swap complete.');
    case 'refund_wait':
      return h('p', { class: 'status spin' }, s.direction === 'L2S'
        ? `This swap won't finish. Your refund opens at block ${s.terms.lock_height.toLocaleString('en-US')} (now ${(s.tip ?? 0).toLocaleString('en-US')}).`
        : 'This swap won’t finish. Your refund opens at the Solana deadline.');
    case 'refund':
      return refundLiquidView(s);
    case 'refund_ready':
      return [
        h('p', { class: 'note' }, 'The deadline has passed, so your Solana funds can come back.'),
        walletView(),
        h('button', { class: 'btn', id: 'refund', disabled: !wallet, onclick: act(() => engine.refundOnSolana(s)) }, 'Refund on Solana'),
      ];
    case 'refunding':
      return h('p', { class: 'status spin' }, s.status || 'Refund sent, waiting for a confirmation.');
    case 'refunded':
      return h('p', { class: 'good', id: 'refunded' }, 'Refunded.');
    case 'expired':
      return h('p', { class: 'note' }, 'This swap expired before anything was paid.');
    default:
      return h('p', { class: 'status' }, s.status);
  }
}

function swapView(s) {
  const [from, to] = names[s.direction];
  return h('div', { class: 'card', id: 'swap-card', 'data-state': s.state },
    h('h2', {}, `${from} → ${to}`),
    h('dl', { class: 'facts' },
      h('div', {}, h('dt', {}, 'You send'), h('dd', {}, s.direction === 'L2S' ? `${show(BigInt(s.terms.liquid_amount) / 100n)} ${from}` : `${show(s.terms.solana_amount)} ${from}`)),
      h('div', {}, h('dt', {}, 'You receive'), h('dd', {}, s.direction === 'L2S' ? `${show(s.terms.solana_amount)} ${to}` : `${show(BigInt(s.terms.liquid_amount) / 100n)} ${to}`))),
    stepsView(s),
    swapBody(s),
    h('button', { class: 'btn quiet', onclick: () => { ui.openSwap = null; render(); } }, 'Back'));
}

// ---- Rescue and history ----
function rescueView() {
  const pick = h('input', {
    type: 'file', accept: '.json,application/json', class: 'input', id: 'rescue-file',
    onchange: async (e) => {
      try {
        const s = deriveSwap(await readRecovery(await e.target.files[0].text()));
        if (s.state === 'recovery') s.state = s.direction === 'L2S' ? 'fund' : 'lock';
        saveSwap(s);
        watch(s);
        ui.openSwap = s.id;
        location.hash = '#swap';
        render();
      } catch (err) {
        fail(err);
      }
    },
  });
  return h('div', { class: 'card' },
    h('h2', {}, 'Rescue'),
    h('p', { class: 'sub' }, 'Pick up a swap on any device, with its recovery file. No maker, relay or server is needed for a refund.'),
    h('label', { class: 'field' }, h('span', {}, 'Your recovery file'), pick));
}

function historyView() {
  const all = Object.values(loadSwaps()).sort((a, b) => b.created_at - a.created_at);
  if (!all.length) return h('div', { class: 'card' }, h('h2', {}, 'History'), h('p', { class: 'sub' }, 'Swaps from this browser appear here.'));
  return h('div', { class: 'list' }, all.map((s) => {
    const [from, to] = names[s.direction];
    return h('div', { class: 'swap-row' },
      h('span', {}, `${show(s.direction === 'L2S' ? BigInt(s.terms.liquid_amount) / 100n : s.terms.solana_amount)} ${from} → ${to}`),
      h('span', { class: 'mono' }, s.state),
      h('span', {}, new Date(s.created_at * 1000).toLocaleString()),
      h('button', { class: 'btn quiet', onclick: () => { ui.openSwap = s.id; location.hash = '#swap'; render(); } }, 'Open'),
      FINISHED.includes(s.state) && h('button', { class: 'btn quiet', onclick: () => { removeSwap(s.id); render(); } }, 'Forget'));
  }));
}

function settingsDialog() {
  const field = (label, id, value, placeholder = '') => h('label', { class: 'field' }, h('span', {}, label), h('input', { class: 'input', id, value: value ?? '', placeholder }));
  const dialog = h('dialog', {},
    h('h3', {}, 'Settings'),
    field('First Liquid Esplora', 's-e1', settings.esplora?.[0], NETWORK.esplora[0]),
    field('Second Liquid Esplora', 's-e2', settings.esplora?.[1], NETWORK.esplora[1]),
    field('Solana RPC', 's-sol', settings.solana, NETWORK.solana),
    field('Helius key, for a second Solana source. Stored only in this browser, and sent only to Helius.', 's-helius', settings.heliusKey),
    field('Nostr relays, separated by spaces', 's-relays', settings.relays?.join(' '), NETWORK.relays.join(' ')),
    field('Largest spread to accept, in basis points', 's-spread', settings.maxSpreadBps, String(NETWORK.maxSpreadBps)),
    h('button', {
      class: 'btn',
      onclick: () => {
        const v = (id) => $(id).value.trim();
        settings.esplora = v('s-e1') || v('s-e2') ? [v('s-e1') || NETWORK.esplora[0], v('s-e2') || NETWORK.esplora[1]] : undefined;
        settings.solana = v('s-sol') || undefined;
        settings.heliusKey = v('s-helius') || undefined;
        settings.relays = v('s-relays') ? v('s-relays').split(/\s+/) : undefined;
        settings.maxSpreadBps = v('s-spread') ? Number(v('s-spread')) : undefined;
        saveSettings(settings);
        dialog.close();
        dialog.remove();
        endpoints();
        engine.esplora = esplora;
        engine.solana = solana;
        startRelays();
        render();
      },
    }, 'Save'),
    h('button', { class: 'btn quiet', onclick: () => { dialog.close(); dialog.remove(); } }, 'Cancel'));
  document.body.append(dialog);
  dialog.showModal();
}

// ---- Render and run ----
function render() {
  const tab = (location.hash.replace('#', '') || 'swap');
  document.querySelectorAll('.tab[data-tab]').forEach((a) => a.classList.toggle('on', a.dataset.tab === tab));
  const view = $('view');
  const below = $('below');
  // A file picker or a half-typed address shouldn't vanish when the chain is polled.
  const active = document.activeElement;
  if (active && active.id && (active.id === 'refund-to' || active.type === 'file') && view.contains(active)) return;
  view.replaceChildren();
  below.replaceChildren();
  if (tab === 'rescue') {
    view.append(rescueView());
  } else if (tab === 'history') {
    view.append(historyView());
  } else if (ui.openSwap && loadSwaps()[ui.openSwap]) {
    view.append(swapView(loadSwaps()[ui.openSwap]));
  } else if (ui.quote) {
    view.append(quoteView());
  } else {
    view.append(formView());
    below.append(bookView(units(ui.amount)));
    below.append(h('div', { class: 'hero' }, h('h1', {}, 'Liquid ⇄ Solana Atomic Swaps'), h('p', {}, 'Swap USDt across chains while staying in full control. Nobody can link the two sides.')));
  }
}

async function tickAll() {
  for (const s of Object.values(loadSwaps())) {
    if (FINISHED.includes(s.state)) continue;
    try {
      await engine.tick(s);
    } catch (e) {
      console.warn('tick', e.message);
    }
  }
}

async function main() {
  await init();
  if (NETWORK.regtest) W.use_regtest(NETWORK.usdt, NETWORK.lbtc);
  endpoints();
  $('net').textContent = NETWORK.label;
  engine = new Engine({
    esplora, solana, secondSolana, getWallet: () => wallet, relays: null,
    onChange: () => { if (ui.openSwap) render(); },
    allowOneEndpoint: () => !!settings.allowOneEndpoint,
  });
  startRelays();
  const open = Object.values(loadSwaps()).filter((s) => !FINISHED.includes(s.state)).sort((a, b) => b.created_at - a.created_at)[0];
  if (open) ui.openSwap = open.id;
  window.addEventListener('hashchange', render);
  render();
  try {
    refuseMainnet(await esplora[0].genesis(), await solana.genesis());
  } catch (e) {
    relays.close();
    fail(e);
    engine = null;
    return;
  }
  setInterval(() => tickAll(), NETWORK.pollMs);
  tickAll();
  // Wallet extensions can register after load, and offers can expire.
  setInterval(() => { if (!ui.openSwap && !ui.quote && document.activeElement?.tagName !== 'INPUT') render(); }, 5000);
}

main().catch(fail);
