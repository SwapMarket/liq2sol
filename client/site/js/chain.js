// Reading and writing both chains over public APIs. Nothing here decides anything: the WASM reads coins and accounts.
import * as W from '../wasm/liq2sol_client_wasm.js';

export const hexToBytes = (h) => Uint8Array.from(h.match(/../g) ?? [], (b) => parseInt(b, 16));
export const bytesToHex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
export const bytesToB64 = (b) => btoa(String.fromCharCode(...b));
export const b64ToBytes = (s) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));

async function http(url, options) {
  const res = await fetch(url, options);
  const text = await res.text();
  if (!res.ok) throw new Error(`${new URL(url).host}: ${text.slice(0, 200) || res.status}`);
  return text;
}

export class Esplora {
  constructor(base) {
    this.base = base;
    this.cache = new Map();
  }

  tip = async () => Number(await http(`${this.base}/blocks/tip/height`));
  genesis = () => http(`${this.base}/block-height/0`).then((h) => h.trim());
  history = async (address) => JSON.parse(await http(`${this.base}/address/${address}/txs`));
  utxos = async (address) => JSON.parse(await http(`${this.base}/address/${address}/utxo`));

  // Fixed by its id, so one copy is enough, and it costs a public API only once.
  async txHex(txid) {
    if (!this.cache.has(txid)) this.cache.set(txid, (await http(`${this.base}/tx/${txid}/hex`)).trim());
    return this.cache.get(txid);
  }

  // The registry's entry for an asset: name, ticker, precision and the issuer's domain. Absent for unregistered ones.
  asset = async (id) => JSON.parse(await http(`${this.base}/asset/${id}`));

  broadcast = (hex) => http(`${this.base}/tx`, { method: 'POST', body: hex }).then((t) => t.trim());
  outspend = async (txid, vout) => JSON.parse(await http(`${this.base}/tx/${txid}/outspend/${vout}`));
  status = async (txid) => JSON.parse(await http(`${this.base}/tx/${txid}/status`));

  // Every output at this script that reads as what we were told, with its depth. Junk at the address is simply not in the list.
  async readCoins(plainAddress, script, blindingKey) {
    const tip = await this.tip();
    const found = [];
    for (const utxo of await this.utxos(plainAddress)) {
      const hex = await this.txHex(utxo.txid);
      for (const coin of JSON.parse(W.read_coins(hex, script, blindingKey))) {
        if (coin.vout !== utxo.vout) continue;
        const height = utxo.status?.block_height ?? null;
        found.push({ ...coin, txid: utxo.txid, hex, confirmations: height === null ? 0 : tip + 1 - height });
      }
    }
    return found;
  }

  parentsOf = async (hex) => Promise.all(JSON.parse(W.parents_of(hex)).map((txid) => this.txHex(txid)));
}

export class Solana {
  constructor(url) {
    this.url = url;
  }

  async rpc(method, params = []) {
    const body = JSON.stringify({ jsonrpc: '2.0', id: 1, method, params });
    const text = await http(this.url, { method: 'POST', headers: { 'content-type': 'application/json' }, body });
    const reply = JSON.parse(text);
    // An RPC's message can repeat the URL, and a Helius URL holds the user's key.
    if (reply.error) throw new Error(`Solana RPC: ${reply.error.message}`.split('api-key=')[0]);
    return reply.result;
  }

  genesis = () => this.rpc('getGenesisHash');
  slot = () => this.rpc('getSlot', [{ commitment: 'finalized' }]);
  signatures = async (key) => this.rpc('getSignaturesForAddress', [key, { limit: 1, commitment: 'confirmed' }]);
  lamports = async (key) => BigInt((await this.rpc('getBalance', [key, { commitment: 'confirmed' }])).value);

  async tokenBalance(account) {
    const info = await this.rpc('getAccountInfo', [account, { encoding: 'jsonParsed', commitment: 'confirmed' }]);
    return info.value ? BigInt(info.value.data.parsed.info.tokenAmount.amount) : 0n;
  }

  // Finalized, so nothing is decided on a block that could still drop. Anything that isn't an escrow is an error.
  async escrow(address, commitment = 'finalized') {
    const info = await this.rpc('getAccountInfo', [address, { encoding: 'base64', commitment }]);
    if (!info.value) return null;
    return JSON.parse(W.parse_escrow(bytesToHex(b64ToBytes(info.value.data[0]))));
  }

  blockhash = async () => (await this.rpc('getLatestBlockhash', [{ commitment: 'confirmed' }])).value.blockhash;

  // What recently landed on these accounts, so a claim doesn't wait behind everyone else when the cluster is busy.
  async priorityFee(accounts, floor, cap) {
    const fees = (await this.rpc('getRecentPrioritizationFees', [accounts])).map((f) => f.prioritizationFee).sort((a, b) => a - b);
    const p75 = fees.length ? fees[Math.floor(fees.length * 0.75)] : 0;
    return Math.min(cap, Math.max(floor, p75));
  }

  // A failed transaction is still public, so a claim isn't sent unless this comes back clean.
  async simulate(signed) {
    const result = await this.rpc('simulateTransaction', [bytesToB64(signed), { encoding: 'base64', sigVerify: true, commitment: 'confirmed' }]);
    if (result.value.err) {
      const log = (result.value.logs ?? []).filter((l) => l.includes('Error') || l.includes('failed')).join(' ');
      throw new Error(`The transaction wouldn't succeed, so it wasn't sent: ${JSON.stringify(result.value.err)} ${log}`);
    }
  }

  send = (signed) => this.rpc('sendTransaction', [bytesToB64(signed), { encoding: 'base64', skipPreflight: true, maxRetries: 3 }]);
}
