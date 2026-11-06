// Browser storage. WalletConnect moves localStorage keys containing "walletconnect", "wc@", "wc_" or "wallet_connect" elsewhere,
// so names here avoid those. This holds secrets until a swap settles, which is why the recovery file exists too.
const SWAPS = 'liq2sol.swaps';
const SETTINGS = 'liq2sol.settings';

function read(key, fallback) {
  try {
    return JSON.parse(localStorage.getItem(key)) ?? fallback;
  } catch {
    return fallback;
  }
}

function write(key, value) {
  localStorage.setItem(key, JSON.stringify(value));
}

export const loadSwaps = () => read(SWAPS, {});

export function saveSwap(swap) {
  const all = loadSwaps();
  all[swap.id] = swap;
  write(SWAPS, all);
}

export function removeSwap(id) {
  const all = loadSwaps();
  delete all[id];
  write(SWAPS, all);
}

// One writer per swap. A tick waits on the network, and meanwhile a relay message or a button can change the swap, so each of them
// runs here in turn, starting from what is stored now. Without this, a slow tick saved its old copy over newer facts, such as
// that we had locked, or the maker's one-time signature share.
const queues = new Map();

export function withSwap(id, fn) {
  const next = (queues.get(id) ?? Promise.resolve()).catch(() => {}).then(() => {
    const swap = loadSwaps()[id];
    return swap ? fn(swap) : undefined;
  });
  queues.set(id, next);
  return next;
}

export const loadSettings = () => read(SETTINGS, {});
export const saveSettings = (settings) => write(SETTINGS, settings);
