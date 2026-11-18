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

// ---- As pictures, for phones whose browser can't save or pick files ----
// One QR code per swap: its recovery, without what the page derives again from the rest (the addresses, scripts and escrow, as
// Rescue does with any file), compressed. A file of any form goes through readRecovery, checksum and all.

const DERIVED = ['info', 'escrow_address', 'state', 'status'];

async function squeeze(bytes, how) {
  const stream = new Blob([bytes]).stream().pipeThrough(how);
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

// Each swap's code, as raw bytes.
export async function recoveryCodes(swaps) {
  return Promise.all(swaps.map(async (s) => {
    const kept = Object.fromEntries(Object.entries(s).filter(([k]) => !DERIVED.includes(k)));
    const text = JSON.stringify(JSON.parse(await makeRecovery([kept])));
    return squeeze(new TextEncoder().encode(text), new CompressionStream('deflate-raw'));
  }));
}

// What a code holds, as recovery text.
export async function codeText(bytes) {
  try {
    return new TextDecoder().decode(await squeeze(bytes, new DecompressionStream('deflate-raw')));
  } catch {
    throw new Error("That QR code isn't a liq2sol recovery code.");
  }
}

// The recovery text in a chosen file: a recovery file, or a picture of a recovery code.
export async function fileText(file) {
  if (!file.type.startsWith('image/')) return file.text();
  const bitmap = await createImageBitmap(file);
  const canvas = document.createElement('canvas');
  canvas.width = bitmap.width;
  canvas.height = bitmap.height;
  const context = canvas.getContext('2d', { willReadFrequently: true });
  context.drawImage(bitmap, 0, 0);
  const { data, width, height } = context.getImageData(0, 0, bitmap.width, bitmap.height);
  const { jsQR } = await import('../vendor/jsqr.js');
  const found = jsQR(data, width, height);
  if (!found) throw new Error('No QR code could be read in that picture. Choose the screenshot of the recovery code itself.');
  return codeText(new Uint8Array(found.binaryData));
}

// Every swap in the chosen files, each checked as readRecovery does.
export async function readRecoveryFiles(files) {
  const swaps = [];
  for (const file of files) swaps.push(...(await readRecovery(await fileText(file))));
  return swaps;
}
