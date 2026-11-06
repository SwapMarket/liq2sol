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

export const loadSettings = () => read(SETTINGS, {});
export const saveSettings = (settings) => write(SETTINGS, settings);
