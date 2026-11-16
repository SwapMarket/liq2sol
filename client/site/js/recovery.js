// The recovery file: enough, with no browser storage, to get funds back. The funding address stays hidden until it has been saved and read back.
import { NETWORK } from './config.js';


async function checksum(body) {
  const { checksum: _, ...rest } = body;
  const text = JSON.stringify(sortDeep(rest));
  const hash = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text));
  return Array.from(new Uint8Array(hash), (b) => b.toString(16).padStart(2, '0')).join('');
}

function sortDeep(v) {
  if (Array.isArray(v)) return v.map(sortDeep);
  if (v && typeof v === 'object') return Object.fromEntries(Object.keys(v).sort().map((k) => [k, sortDeep(v[k])]));
  return v;
}

export async function makeRecovery(swaps) {
  // Files from before mainnet carry no network, and are testnet's.
  const body = { format: 'liq2sol-recovery', version: 2, network: NETWORK.name, created_at: Math.floor(Date.now() / 1000), swaps };
  return JSON.stringify({ ...body, checksum: await checksum(body) }, null, 1);
}

// The swaps in an intact file. Throws otherwise. Whether they are the ones expected is the caller's check.
export async function readRecovery(text) {
  let body;
  try {
    body = JSON.parse(text);
  } catch {
    throw new Error("This isn't a recovery file.");
  }
  if (body.format !== 'liq2sol-recovery' || body.version !== 2 || !body.swaps?.length || !body.swaps.every((s) => s.secrets)) throw new Error("This isn't a liq2sol recovery file.");
  if ((await checksum(body)) !== body.checksum) throw new Error('This recovery file is damaged: its checksum is wrong.');
  const from = body.network ?? 'testnet';
  if ((from === 'mainnet') !== (NETWORK.name === 'mainnet')) {
    throw new Error(`This recovery file is from ${from === 'mainnet' ? 'mainnet' : 'a test network'}. Switch networks in Settings to use it.`);
  }
  return body.swaps;
}

export function offerDownload(text, name) {
  const url = URL.createObjectURL(new Blob([text], { type: 'application/json' }));
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  a.hidden = true;
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 60000);
}
