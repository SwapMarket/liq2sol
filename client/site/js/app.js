// The page. It shows state and takes decisions from the user. js/swap.js and the WASM do the protocol.
import init, * as W from '../wasm/liq2sol_client_wasm.js';
import { NETWORK, refuseMainnet } from './config.js';
import { Esplora, Solana } from './chain.js';
import { bpsAt, planOrder } from './plan.js';
import { Relays } from './relay.js';
import { Engine, FINISHED, deriveSwap, rulesJson } from './swap.js';
import { loadSettings, loadSwaps, removeSwap, saveSettings, saveSwap, withSwap } from './store.js';
import { captureConsole, clearLogs, log, logsCount, logsText } from './logs.js';
import { makeRecovery, offerDownload, readRecovery } from './recovery.js';
import { vettedMaker } from './vetted.js';
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
// Fills being negotiated, by the throwaway Nostr key each one uses.
const negotiations = new Map();
const ui = { direction: 'L2S', amount: '', to: '', selected: null, openOrder: null, quote: null };

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
// The book's sizes, to two places, rounded the cautious way: a minimum up, and a maximum or what's available down.
const STEP = 10000n;
const sizeUp = (u) => (((BigInt(u) + STEP - 1n) / STEP) * STEP);
const sizeDown = (u) => ((BigInt(u) / STEP) * STEP);
const size = (u) => {
  const v = BigInt(u);
  return `${(v / 1000000n).toLocaleString('en-US')}.${((v % 1000000n) / STEP).toString().padStart(2, '0')}`;
};
// What the taker pays per unit received, to four places, so a worse price is a higher number. Quotes and payments stay exact: a coin
// must match to the last unit.
const price = (bpsValue) => (10000 / (10000 - bpsValue)).toFixed(4);
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
  log('error', e);
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
    refreshForm();
    return;
  }
  if (event.kind !== WRAP_KIND) return;
  const to = event.tags.find((t) => t[0] === 'p')?.[1];
  const talking = negotiations.get(to);
  if (talking) {
    const got = JSON.parse(W.unwrap_message(talking.nostr.secret, text, NETWORK.name, BigInt(now())));
    if (got) talking.onMessage(got);
    return;
  }
  const s = Object.values(loadSwaps()).find((x) => x.nostr.public === to && !FINISHED.includes(x.state));
  if (!s) return;
  const got = JSON.parse(W.unwrap_message(s.nostr.secret, text, NETWORK.name, BigInt(now())));
  if (got) withSwap(s.id, (fresh) => engine.onMessage(fresh, got)).catch(fail);
}

// ---- The makers book ----
const liveOffers = (direction) => [...offers.values()]
  .filter((o) => o.offer.directions.includes(direction))
  .sort((a, b) => tiersOf(a, direction)[0].bps - tiersOf(b, direction)[0].bps);
const tiersOf = (o, direction) => (direction === 'L2S' ? o.offer.l2s_prices : o.offer.s2l_prices).map((t) => ({ up_to: BigInt(t.up_to), bps: t.bps }));
const available = (o, direction) => BigInt(direction === 'L2S' ? o.offer.available_l2s : o.offer.available_s2l);

// Saved with the settings, so they outlast this visit.
const favorites = () => new Set(settings.favorites ?? []);

function toggleFavorite(maker) {
  const set = favorites();
  if (!set.delete(maker)) set.add(maker);
  settings.favorites = [...set];
  saveSettings(settings);
  render();
}

function setOnlyFavorites(on) {
  settings.onlyFavorites = on;
  saveSettings(settings);
  render();
}

// On unless the taker turned it off, because a new visitor has no favorites and fake offers cost nothing to publish.
const onlyVetted = () => settings.onlyVetted !== false;

function setOnlyVetted(on) {
  settings.onlyVetted = on;
  saveSettings(settings);
  render();
}

// The makers an order can use. Hidden ones aren't offered anything.
const shownOffers = (direction) => {
  const fav = favorites();
  const list = liveOffers(direction);
  return list
    .filter((o) => (!settings.onlyFavorites || fav.has(o.maker)) && (!onlyVetted() || vettedMaker(NETWORK.name, o.maker)))
    .sort((a, b) => Number(fav.has(b.maker)) - Number(fav.has(a.maker)));
};

function currentPlan(send) {
  const makers = shownOffers(ui.direction).map((o) => ({
    maker: o.maker, tiers: tiersOf(o, ui.direction), min: BigInt(o.offer.min_sol_units), max: BigInt(o.offer.max_sol_units), available: available(o, ui.direction),
  }));
  return planOrder(send ?? 0n, makers, ui.selected, settings.maxSpreadBps ?? NETWORK.maxSpreadBps, favorites());
}

// The price at this order's size, or the best step if there is no order yet. Hovering shows the whole ladder.
function priceCell(o, direction, send, fills) {
  const tiers = tiersOf(o, direction);
  const ladder = tiers.map((t) => `up to ${size(sizeDown(t.up_to))}: ${price(t.bps)}`).join('\n');
  if (fills.length) {
    const rates = [...new Set(fills.map((f) => price(f.bps)))];
    return h('td', { title: ladder }, rates.join(' / '));
  }
  const at = send && send > 0n ? bpsAt(tiers, send) : null;
  if (at !== null) return h('td', { title: ladder }, price(at));
  return h('td', { title: ladder }, `${tiers.length > 1 ? 'from ' : ''}${price(tiers[0].bps)}`);
}

function bookView(send) {
  const list = shownOffers(ui.direction);
  const plan = currentPlan(send);
  const fav = favorites();
  const rows = list.map((o) => {
    const mine = plan.fills.filter((f) => f.maker === o.maker);
    return h('tr', {
      class: mine.length ? 'on' : '',
      onclick: () => { ui.selected = ui.selected === o.maker ? null : o.maker; render(); },
    },
    h('td', { class: 'mono', title: o.maker },
      h('button', {
        class: `star${fav.has(o.maker) ? ' on' : ''}`,
        title: fav.has(o.maker) ? 'Remove from favorites' : 'Add to favorites',
        'aria-label': fav.has(o.maker) ? `Remove ${short(o.maker)} from favorites` : `Add ${short(o.maker)} to favorites`,
        'aria-pressed': fav.has(o.maker) ? 'true' : 'false',
        onclick: (e) => { e.stopPropagation(); toggleFavorite(o.maker); },
      }, fav.has(o.maker) ? '★' : '☆'),
      ` ${short(o.maker)}`,
      vettedMaker(NETWORK.name, o.maker)
        ? h('span', { class: 'vetted', title: `Vetted by SwapMarket: ${vettedMaker(NETWORK.name, o.maker).name}`, 'aria-label': 'Vetted by SwapMarket' }, ' ✓')
        : ''),
    priceCell(o, ui.direction, send, mine),
    h('td', {}, `${size(sizeUp(o.offer.min_sol_units))} – ${size(sizeDown(o.offer.max_sol_units))}`),
    h('td', {}, size(sizeDown(available(o, ui.direction)))),
    h('td', {}, mine.length ? size(sizeUp(mine.reduce((sum, f) => sum + f.send, 0n) - 1n)) : '–'));
  });
  return h('div', { class: 'book' },
    h('h3', {}, `Makers book: ${names[ui.direction].join(' → ')}`),
    h('div', { class: 'scroll' }, rows.length
      ? h('table', {},
        h('thead', {}, h('tr', {}, [
          ['Maker', 'The maker\'s Nostr key'],
          ['Price', `${names[ui.direction][0]} you pay for each ${names[ui.direction][1]} you receive`],
          ['Per swap', `The smallest and largest single swap, in ${names[ui.direction][0]} and ${names[ui.direction][1]} alike`],
          ['Maker can pay', `How much ${names[ui.direction][1]} this maker can pay you right now`],
          ['Your fill', `How much of your order goes to this maker, in ${names[ui.direction][0]}`],
        ].map(([t, why]) => h('th', { title: why }, t)))),
        h('tbody', {}, rows))
      : h('div', { class: 'empty' }, liveOffers(ui.direction).length
        ? `No maker you chose to see is offering this direction right now. ${liveOffers(ui.direction).length} hidden by the switches below.`
        : relays?.connected() ? 'No maker is offering this direction right now.' : 'Connecting to Nostr relays…')),
    h('div', { class: 'book-filters' },
      h('label', { class: 'check' },
        h('input', { type: 'checkbox', id: 'only-vetted', checked: onlyVetted(), onchange: (e) => setOnlyVetted(e.target.checked) }),
        'Only vetted by SwapMarket'),
      h('label', { class: 'check' },
        h('input', { type: 'checkbox', id: 'only-favorites', checked: !!settings.onlyFavorites, onchange: (e) => setOnlyFavorites(e.target.checked) }),
        'Only my favorites')),
    onlyVetted() ? '' : h('p', { class: 'note', id: 'unvetted-warning' },
      'Anyone can publish an offer. A maker who never follows through can\'t take your funds, but it can hold them until the refund, for about 6 hours or more.'));
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
    extensionWallets().map((w) => h('button', {
      class: 'btn quiet',
      onclick: () => choose(() => connectExtension(w).catch((e) => {
        console.warn('wallet connect', w.name, e);
        throw new Error(`${w.name} refused to connect: ${e?.message ?? e}. Check that it is unlocked and has a Solana account, and that no other wallet is taking over this page.`);
      })),
    }, w.name)),
    h('button', { class: 'btn quiet', onclick: () => choose(walletConnect) }, 'WalletConnect'));
}

async function walletConnect() {
  // Open at once: the library is large and the relay takes a few seconds, so the code comes later.
  const title = h('h3', {}, 'Connecting to WalletConnect…');
  const box = h('div', { class: 'qr-wait', role: 'status', 'aria-label': 'Connecting to WalletConnect' }, h('div', { class: 'spinner' }));
  const dialog = h('dialog', {}, title, box,
    h('button', { class: 'btn quiet', onclick: () => dialog.close() }, 'Close'));
  document.body.append(dialog);
  dialog.showModal();
  try {
    const { qrcode } = await import('../vendor/qrcode.js');
    return await connectWalletConnect(settings.walletConnectProject || NETWORK.walletConnectProject, (uri) => {
      const qr = qrcode(0, 'L');
      qr.addData(uri);
      qr.make();
      title.textContent = 'Scan with your Solana wallet';
      box.className = 'qr';
      box.removeAttribute('role');
      box.removeAttribute('aria-label');
      box.innerHTML = qr.createSvgTag({ cellSize: 3, margin: 2, scalable: true });
    });
  } finally {
    dialog.close();
    dialog.remove();
  }
}

// ---- Starting a swap ----
// These two stay the same elements across updates. New offers arrive every few seconds, and rebuilding an input could lose what
// was being typed into it at that moment.
const amountInput = h('input', {
  class: 'amount', inputMode: 'decimal', placeholder: '0', id: 'amount',
  oninput: (e) => { ui.amount = e.target.value; refreshForm(); },
});
const toInput = h('input', {
  class: 'input', id: 'to', placeholder: 'tlq1…',
  oninput: (e) => { ui.to = e.target.value; refreshForm(); },
});

function formView() {
  const send = units(ui.amount);
  const [from, to] = names[ui.direction];
  const plan = currentPlan(send);
  const hasAmount = send && send > 0n;
  const received = hasAmount && plan.fills.length ? plan.receive : null;
  const needsAddress = ui.direction === 'S2L';
  const ready = hasAmount && plan.fills.length && wallet && (!needsAddress || ui.to.trim());
  const label = !hasAmount ? 'Enter an amount' : !plan.fills.length ? 'No maker fits this amount' : !wallet ? 'Connect a Solana wallet' : needsAddress && !ui.to.trim() ? 'Enter a Liquid address' : plan.fills.length > 1 ? `Get ${plan.fills.length} firm quotes` : 'Get a firm quote';
  amountInput.setAttribute('aria-label', `You send, in ${from}`);
  const receive = h('input', { class: 'amount', readOnly: true, value: received === null ? '' : show(received), placeholder: '0', 'aria-label': `You receive, in ${to}` });
  return h('div', { class: 'card' },
    h('button', { class: 'gear', title: 'Settings', 'aria-label': 'Settings', onclick: settingsDialog }, '⚙'),
    h('h2', {}, 'Create Atomic Swap'),
    h('p', { class: 'sub' }, 'Trustless and confidential. No server holds anything.'),
    h('div', { class: 'row' }, coinPill(from),
      amountInput),
    h('button', { class: 'flip', title: 'Swap direction', 'aria-label': 'Swap direction', onclick: () => { ui.direction = ui.direction === 'L2S' ? 'S2L' : 'L2S'; ui.selected = null; render(); } }, '⇅'),
    h('div', { class: 'row to' }, coinPill(to), receive),
    needsAddress && h('label', { class: 'field' }, h('span', {}, 'Your Liquid address, confidential'),
      toInput),
    walletView(),
    hasAmount && plan.fills.length > 0 && h('dl', { class: 'facts' },
      h('div', {}, h('dt', {}, 'Filled by'), h('dd', {}, plan.fills.length === 1 ? short(plan.fills[0].maker) : `${plan.fills.length} separate swaps`)),
      h('div', {}, h('dt', {}, 'Average price'), h('dd', {}, (Number(send - plan.unfilled) / Number(plan.receive)).toFixed(4)))),
    hasAmount && plan.unfilled > 0n && plan.fills.length > 0 && h('p', { class: 'warn', id: 'unfilled' }, `The book can fill ${show(send - plan.unfilled)} of this right now. ${show(plan.unfilled)} would stay unswapped.`),
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

function waitFor(n, pred, ms) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('The maker did not answer in time.')), ms);
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

function sendTo(n, type, body) {
  const wrapped = JSON.parse(W.wrap_message(n.nostr.secret, n.maker, n.id, NETWORK.name, JSON.stringify({ type, body }), BigInt(now())));
  relays.publish(wrapped.event);
  return wrapped.rumor_id;
}

const spreadLimit = () => rulesJson(settings.maxSpreadBps ?? NETWORK.maxSpreadBps);

async function quoteFill(fill, direction) {
  const nostr = JSON.parse(W.new_nostr_key());
  const n = { id: JSON.parse(W.new_nostr_key()).secret, nostr, maker: fill.maker, offer: offers.get(fill.maker), onMessage: () => {} };
  negotiations.set(nostr.public, n);
  relays.subscribe(`talk-${nostr.public.slice(0, 12)}`, { kinds: [WRAP_KIND], '#p': [nostr.public], since: now() - 300 });
  await new Promise((r) => setTimeout(r, 300));
  const request = { direction, amount_side: 'send', amount: fill.send.toString() };
  const answered = waitFor(n, (g) => g.body.type === 'rfq-quote', 30000);
  sendTo(n, 'rfq-request', request);
  const got = await answered;
  const problem = W.quote_problem(spreadLimit(), JSON.stringify(request), JSON.stringify(got.body.body), BigInt(now()));
  if (problem) throw new Error(`This quote isn't acceptable: ${problem}.`);
  return { n, quote: got.body.body, rumor: got.rumor_id };
}

function clearQuote() {
  ui.quote = null;
  negotiations.clear();
}

async function startQuote() {
  const send = units(ui.amount);
  const plan = currentPlan(send);
  const direction = ui.direction;
  try {
    if (direction === 'L2S' && await solana.lamports(wallet.address) < NETWORK.minSolLamports * BigInt(plan.fills.length)) {
      throw new Error('This wallet needs a little SOL to pay for each claim, or a swap could not finish. Nothing has been sent.');
    }
    ui.quote = { status: plan.fills.length > 1 ? `Asking ${plan.fills.length} times for a firm price…` : 'Asking the maker for a firm price…' };
    render();
    const settled = await Promise.allSettled(plan.fills.map((f) => quoteFill(f, direction)));
    const fills = settled.filter((r) => r.status === 'fulfilled').map((r) => r.value);
    const refused = settled.filter((r) => r.status === 'rejected');
    if (!fills.length) throw refused[0].reason;
    ui.quote = { fills, refused: refused.map((r) => r.reason.message), direction, to: ui.to.trim(), owner: wallet.address, wanted: send };
    render();
  } catch (e) {
    clearQuote();
    render();
    fail(e);
  }
}

function quoteView() {
  const q = ui.quote;
  if (q.status) return h('div', { class: 'card' }, h('h2', {}, 'Getting a quote'), h('p', { class: 'status spin' }, q.status));
  const [from, to] = names[q.direction];
  const sum = (key) => q.fills.reduce((total, f) => total + BigInt(f.quote[key]), 0n);
  const left = Math.max(0, Math.min(...q.fills.map((f) => f.quote.quote_expiry)) - now());
  const many = q.fills.length > 1;
  return h('div', { class: 'card' },
    h('h2', {}, many ? `Review ${q.fills.length} quotes` : 'Review the quote'),
    h('dl', { class: 'facts' },
      h('div', {}, h('dt', {}, 'You send'), h('dd', {}, `${show(sum('send'))} ${from}`)),
      h('div', {}, h('dt', {}, 'You receive'), h('dd', {}, `${show(sum('receive'))} ${to}`)),
      many ? q.fills.map((f) => h('div', {}, h('dt', { class: 'mono' }, short(f.n.maker)), h('dd', {}, `${show(f.quote.send)} → ${show(f.quote.receive)}`)))
        : h('div', {}, h('dt', {}, 'Maker'), h('dd', { class: 'mono' }, short(q.fills[0].n.maker))),
      h('div', {}, h('dt', {}, 'If a maker never locks'), h('dd', {}, q.direction === 'L2S' ? 'Refund after about 5 hours' : 'Refund after about 6 hours')),
      h('div', {}, h('dt', {}, 'Price holds for'), h('dd', {}, `${left} s`))),
    many && h('p', { class: 'warn', id: 'not-atomic' }, `These are ${q.fills.length} separate swaps. Each one either completes or refunds by itself, so some may complete while others refund. You ${q.direction === 'L2S' ? 'make one exact payment for each' : 'approve one lock for each'}.`),
    many && q.direction === 'S2L' && h('p', { class: 'note' }, 'All of them pay the same Liquid address, so anyone can see those payments went to one place, though not their amounts.'),
    q.refused.length > 0 && h('p', { class: 'warn', id: 'quote-refused' }, `${q.refused.length} of the quotes didn't come through, so this fills less than you asked for: ${q.refused[0]}`),
    q.direction === 'L2S' && h('p', { class: 'note' }, 'You will need a little L-BTC only if a refund is ever needed.'),
    h('button', { class: 'btn', id: 'accept', onclick: acceptQuotes }, many ? 'Accept all' : 'Accept'),
    h('button', { class: 'btn quiet', onclick: () => { clearQuote(); render(); } }, 'Cancel'));
}

async function acceptFill(f, q, order, tip) {
  const { n } = f;
  const secrets = JSON.parse(W.new_swap_secrets());
  const answered = waitFor(n, (g) => g.body.type === 'rfq-terms', 30000);
  const rumor = sendTo(n, 'rfq-accept', {
    quote_event: f.rumor, lock_point: secrets.lock_point, taker_key: secrets.taker_key, blinding_key: secrets.blinding_key, taker_solana: q.owner,
  });
  const got = await answered;
  const terms = got.body.body;
  const problem = W.terms_problem(spreadLimit(), q.direction, JSON.stringify(f.quote), rumor, JSON.stringify(terms), tip);
  if (problem) throw new Error(`A maker's terms aren't acceptable, so nothing was sent for that swap: ${problem}.`);
  return deriveSwap({
    id: n.id, order, created_at: now(), direction: q.direction, maker: n.maker, nostr: n.nostr, secrets, terms,
    fee_coin_sats: n.offer?.offer.fee_coin_sats, solana_owner: q.owner, to_address: q.to, state: 'recovery', status: '',
  });
}

async function acceptQuotes() {
  const q = ui.quote;
  const order = JSON.parse(W.new_nostr_key()).secret.slice(0, 16);
  try {
    ui.quote = { status: 'Fixing the terms with the maker…' };
    render();
    const tip = await esplora[0].tip();
    const settled = await Promise.allSettled(q.fills.map((f) => acceptFill(f, q, order, tip)));
    const swaps = settled.filter((r) => r.status === 'fulfilled').map((r) => r.value);
    const refused = settled.filter((r) => r.status === 'rejected');
    if (!swaps.length) throw refused[0].reason;
    for (const s of swaps) {
      saveSwap(s);
      watch(s);
    }
    clearQuote();
    ui.openOrder = order;
    render();
    if (refused.length) fail(new Error(`${refused.length} of the swaps didn't start: ${refused[0].reason.message} The others go ahead, and nothing was sent for those.`));
  } catch (e) {
    clearQuote();
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
  // A refund leaves this path, and a list with nothing marked would only confuse.
  if (at < 0 && s.state !== 'done') return null;
  return h('ol', { class: 'steps' }, flow.map(([k, text], i) => h('li', { class: i < at || s.state === 'done' ? 'done' : i === at ? 'now' : '' }, text)));
}

// One file for a whole order, and nothing is funded until it has been read back with every swap in it.
function recoveryView(swaps) {
  const pick = h('input', {
    type: 'file', accept: '.json,application/json', class: 'input', id: 'recovery-file',
    onchange: async (e) => {
      try {
        const read = await readRecovery(await e.target.files[0].text());
        const has = (s) => read.some((r) => r.id === s.id && r.secrets.lock_secret === s.secrets.lock_secret);
        if (!swaps.every(has)) throw new Error('That recovery file is for a different swap.');
        await Promise.all(swaps.map((s) => withSwap(s.id, (fresh) => engine.set(fresh, { state: fresh.direction === 'L2S' ? 'fund' : 'lock', recovery_saved: true }))));
        render();
      } catch (err) {
        fail(err);
      }
    },
  });
  return h('div', { class: 'card', id: 'recovery-card' },
    h('h2', {}, 'Save your recovery file'),
    h('p', { class: 'warn' }, 'This file can move your funds, so treat it like money. It is the only way back if this browser loses its data. Never share it: no maker or support person ever needs it.'),
    h('button', { class: 'btn', id: 'download', onclick: async () => offerDownload(await makeRecovery(swaps), `liq2sol-recovery-${swaps[0].order ?? swaps[0].id.slice(0, 8)}.json`) }, swaps.length > 1 ? `Download recovery file for ${swaps.length} swaps` : 'Download recovery file'),
    h('label', { class: 'field' }, h('span', {}, 'Then choose the file you saved, to confirm it is intact. The next step stays hidden until you do.'), pick));
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

// A button's action on one swap, in turn with everything else touching it.
function act(id, fn) {
  return async () => {
    try {
      await withSwap(id, fn);
    } catch (e) {
      fail(e);
    }
    render();
  };
}

function refundLiquidView(s) {
  const input = h('input', { class: 'input refund-to', placeholder: 'tlq1… confidential address' });
  return [
    h('p', { class: 'note' }, `The refund needs a fee coin. From a Liquid wallet, send at least ${NETWORK.refundFeeCoinSats} sats (${(NETWORK.refundFeeCoinSats / 1e8).toFixed(8)} L-BTC) to this address. If your wallet refuses that, send its minimum: Aqua's is 100 sats. All of it pays the network fee, so don't send more than you need to.`),
    h('div', { class: 'addr fee-address' }, s.info.fee_address),
    h('label', { class: 'field' }, h('span', {}, 'Where to send your refund'), input),
    h('button', { class: 'btn refund', onclick: act(s.id, (fresh) => engine.refundOnLiquid(fresh, input.value.trim())) }, 'Refund'),
  ];
}

function swapBody(s) {
  const [from, to] = names[s.direction];
  switch (s.state) {
    case 'fund': {
      const box = h('div', {});
      qrView(s.info.address).then((qr) => box.append(qr)).catch(() => {});
      return [
        h('p', { class: 'note' }, `Send exactly this amount of ${from}, once, from any Liquid wallet:`),
        h('div', { class: 'addr pay-amount' }, `${liquidUnits(s.terms.liquid_amount)} ${from}`),
        h('div', { class: 'addr pay-address' }, s.info.address),
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
        h('button', { class: 'btn lock', disabled: !wallet, onclick: act(s.id, (fresh) => engine.lockOnSolana(fresh)) }, 'Lock on Solana'),
      ];
    case 'await_maker':
      return [
        s.escrow_problem && h('p', { class: 'warn escrow-problem' }, `The maker locked, but the secret stays hidden: ${s.escrow_problem}. Your funds are safe, and can come back with a refund.`),
        h('p', { class: 'status spin' }, s.direction === 'L2S'
          ? `Your coin has ${s.coin?.confirmations ?? 0} confirmations. Waiting for the maker to sign and lock on Solana.`
          : (s.status || 'Waiting for the maker to lock on Liquid.')),
      ];
    case 'ready_claim':
      return [
        h('p', { class: 'good' }, `The maker locked ${show(s.terms.solana_amount)} ${to} for you.`),
        !secondSolana() && h('label', { class: 'check' },
          h('input', { type: 'checkbox', class: 'one-endpoint', checked: !!settings.allowOneEndpoint, onchange: (e) => { settings.allowOneEndpoint = e.target.checked; saveSettings(settings); } }),
          'Only one Solana endpoint is set, so a lying endpoint could fool this page into revealing the secret. Claim anyway. Add a free Helius key in Settings to check two.'),
        walletView(),
        h('button', { class: 'btn claim', disabled: !wallet, onclick: act(s.id, (fresh) => engine.claimOnSolana(fresh)) }, 'Claim on Solana'),
        s.status && h('p', { class: 'status' }, s.status),
      ];
    case 'signing':
      return h('p', { class: 'status spin' }, s.status || 'Signing the claim together with the maker…');
    case 'claiming':
      return h('p', { class: 'status spin' }, s.status || 'Publishing the claim…');
    case 'done':
      return h('p', { class: 'good done' }, s.status || 'Swap complete.');
    case 'refund_wait':
      return h('p', { class: 'status spin' }, s.direction === 'L2S'
        ? `This swap won't finish. Your refund opens at block ${s.terms.lock_height.toLocaleString('en-US')} (now ${(s.tip ?? 0).toLocaleString('en-US')}). Nobody needs to help, and nobody needs your recovery file.`
        : 'This swap won’t finish. Your refund opens at the Solana deadline. Nobody needs to help, and nobody needs your recovery file.');
    case 'refund':
      return refundLiquidView(s);
    case 'refund_ready':
      return [
        h('p', { class: 'note' }, 'The deadline has passed, so your Solana funds can come back.'),
        walletView(),
        h('button', { class: 'btn refund', disabled: !wallet, onclick: act(s.id, (fresh) => engine.refundOnSolana(fresh)) }, 'Refund on Solana'),
      ];
    case 'refunding':
      return h('p', { class: 'status spin' }, s.status || 'Refund sent, waiting for a confirmation.');
    case 'refunded':
      return h('p', { class: 'good refunded' }, 'Refunded.');
    case 'expired':
      return h('p', { class: 'note' }, 'This swap expired before anything was paid.');
    default:
      return h('p', { class: 'status' }, s.status);
  }
}

function swapView(s) {
  const [from, to] = names[s.direction];
  return h('div', { class: 'card swap-card', 'data-state': s.state, 'data-id': s.id },
    h('h2', {}, `${from} → ${to}`),
    h('dl', { class: 'facts' },
      h('div', {}, h('dt', {}, 'You send'), h('dd', {}, s.direction === 'L2S' ? `${show(BigInt(s.terms.liquid_amount) / 100n)} ${from}` : `${show(s.terms.solana_amount)} ${from}`)),
      h('div', {}, h('dt', {}, 'You receive'), h('dd', {}, s.direction === 'L2S' ? `${show(s.terms.solana_amount)} ${to}` : `${show(BigInt(s.terms.liquid_amount) / 100n)} ${to}`))),
    stepsView(s),
    swapBody(s));
}

const orderOf = (id) => Object.values(loadSwaps()).filter((s) => (s.order ?? s.id) === id).sort((a, b) => a.id.localeCompare(b.id));

function orderView(swaps) {
  const waiting = swaps.filter((s) => s.state === 'recovery');
  const back = h('div', { class: 'card back' }, h('button', { class: 'btn quiet', onclick: () => { ui.openOrder = null; render(); } }, 'Back'));
  if (waiting.length) return [recoveryView(waiting), back];
  const header = swaps.length > 1 && h('p', { class: 'sub order-summary' }, `${swaps.filter((s) => s.state === 'done').length} of ${swaps.length} swaps complete. Each one finishes or refunds by itself.`);
  return [header, swaps.map(swapView), back];
}

// ---- Rescue and history ----
function rescueView() {
  const pick = h('input', {
    type: 'file', accept: '.json,application/json', class: 'input', id: 'rescue-file',
    onchange: async (e) => {
      try {
        const swaps = await readRecovery(await e.target.files[0].text());
        for (const raw of swaps) {
          const s = deriveSwap(raw);
          if (s.state === 'recovery') s.state = s.direction === 'L2S' ? 'fund' : 'lock';
          saveSwap(s);
          watch(s);
          ui.openOrder = s.order ?? s.id;
        }
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
      h('button', { class: 'btn quiet', onclick: () => { ui.openOrder = s.order ?? s.id; location.hash = '#swap'; render(); } }, 'Open'),
      FINISHED.includes(s.state) && h('button', { class: 'btn quiet', onclick: () => { removeSwap(s.id); render(); } }, 'Forget'));
  }));
}

// Support material, Boltz-style: copy it, save it, or clear it. It only leaves this browser if the user sends it.
function logsRow() {
  const note = h('p', { class: 'note', id: 'logs-note' }, `${logsCount()} lines. Secrets are removed, and these stay in this browser until you share them.`);
  const copy = h('button', {
    class: 'icon-btn', id: 'logs-copy', title: 'Copy logs', 'aria-label': 'Copy logs',
    onclick: async () => {
      try {
        await navigator.clipboard.writeText(logsText());
        note.textContent = 'Copied.';
      } catch (e) {
        note.textContent = `Couldn't copy: ${e.message}`;
      }
    },
  }, '⧉');
  const save = h('button', {
    class: 'icon-btn', id: 'logs-download', title: 'Download logs', 'aria-label': 'Download logs',
    onclick: () => offerDownload(logsText(), `liq2sol-logs-${new Date().toISOString().slice(0, 19).replace(/[:T]/g, '-')}.txt`),
  }, '⤓');
  const del = h('button', {
    class: 'icon-btn danger', id: 'logs-delete', title: 'Delete logs', 'aria-label': 'Delete logs',
    onclick: () => { clearLogs(); note.textContent = 'Logs deleted.'; },
  }, '🗑');
  return [h('div', { class: 'setting-row' }, h('span', {}, 'Logs'), h('span', { class: 'icon-btns' }, copy, save, del)), note];
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
    h('h4', {}, 'Support'),
    logsRow(),
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
  if (active && (active.classList?.contains('refund-to') || active.type === 'file') && view.contains(active)) return;
  view.replaceChildren();
  below.replaceChildren();
  if (tab === 'rescue') {
    view.append(rescueView());
  } else if (tab === 'history') {
    view.append(historyView());
  } else if (ui.openOrder && orderOf(ui.openOrder).length) {
    view.append(...orderView(orderOf(ui.openOrder)).flat().filter(Boolean));
  } else if (ui.quote) {
    view.append(quoteView());
  } else {
    view.append(formView());
    below.append(bookView(units(ui.amount)));
    below.append(h('div', { class: 'hero' }, h('h1', {}, 'Liquid ⇄ Solana Atomic Swaps'), h('p', {}, 'Swap USDt across chains while staying in full control. Nobody can link the two sides.')));
  }
}

// A swap waiting on a slow wallet holds up only itself, and ticks don't pile up behind it.
const ticking = new Set();

function tickAll() {
  for (const s of Object.values(loadSwaps())) {
    if (FINISHED.includes(s.state) || ticking.has(s.id)) continue;
    ticking.add(s.id);
    withSwap(s.id, (fresh) => engine.tick(fresh))
      .catch((e) => console.warn('tick', e.message))
      .finally(() => ticking.delete(s.id));
  }
}

async function main() {
  captureConsole();
  log('info', `started, build ${document.querySelector('footer a')?.textContent ?? ''}, ${navigator.userAgent}`);
  await init();
  if (NETWORK.regtest) W.use_regtest(NETWORK.usdt, NETWORK.lbtc);
  endpoints();
  $('net').textContent = NETWORK.label;
  engine = new Engine({
    esplora, solana, secondSolana, getWallet: () => wallet, relays: null,
    onChange: () => { if (ui.openOrder) render(); },
    allowOneEndpoint: () => !!settings.allowOneEndpoint,
  });
  startRelays();
  const open = Object.values(loadSwaps()).filter((s) => !FINISHED.includes(s.state)).sort((a, b) => b.created_at - a.created_at)[0];
  if (open) ui.openOrder = open.order ?? open.id;
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
  setInterval(() => { if (!ui.openOrder && !ui.quote) refreshForm(); }, 5000);
}

main().catch(fail);
