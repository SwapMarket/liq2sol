// node --test client/test/logs.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';

const memory = new Map();
globalThis.localStorage = { getItem: (k) => memory.get(k) ?? null, setItem: (k, v) => memory.set(k, v), removeItem: (k) => memory.delete(k) };
const { saveSwap, saveSettings } = await import('../site/js/store.js');
const { clearLogs, log, logsCount, logsText, scrub } = await import('../site/js/logs.js');

const SECRET = 'a'.repeat(64);
const KEY = 'b'.repeat(64);
const BLINDING = 'c'.repeat(64);
const NOSTR = 'd'.repeat(64);

saveSwap({ id: 'x', secrets: { lock_secret: SECRET, taker_sk: KEY, blinding_key: BLINDING }, nostr: { secret: NOSTR, public: 'e'.repeat(64) } });

test('every secret of every swap is removed', () => {
  for (const s of [SECRET, KEY, BLINDING, NOSTR]) assert.ok(!scrub(`before ${s} after`).includes(s));
  assert.equal(scrub(`got ${SECRET}!`), 'got [secret]!');
});

test('what is public stays readable', () => {
  const txid = 'f'.repeat(64);
  assert.ok(scrub(`claim ${txid}`).includes(txid), 'a txid helps support');
  assert.ok(scrub('e'.repeat(64)).includes('e'.repeat(64)), 'a public key isn’t secret');
});

test('a Helius key never gets in, known or not', () => {
  saveSettings({ heliusKey: 'my-private-key' });
  assert.ok(!scrub('failed with my-private-key').includes('my-private-key'));
  assert.ok(!scrub('https://devnet.helius-rpc.com/?api-key=someone-elses&x=1').includes('someone-elses'));
  saveSettings({});
});

test('whole transactions are left out', () => {
  assert.equal(scrub('9'.repeat(5000)), '[long hex removed]');
});

test('a secret that appears after its line was logged is still removed when read', () => {
  clearLogs();
  const later = '1'.repeat(64);
  log('warn', `saw ${later}`);
  assert.ok(logsText().includes(later));
  saveSwap({ id: 'y', secrets: { lock_secret: later }, nostr: {} });
  assert.ok(!logsText().includes(later));
});

test('repeats collapse, and the log can’t grow without limit', () => {
  clearLogs();
  for (let i = 0; i < 300; i++) log('error', 'the same thing');
  assert.equal(logsCount(), 1);
  assert.ok(logsText().includes('(x300)'));
  for (let i = 0; i < 1500; i++) log('warn', `line ${i}`);
  assert.equal(logsCount(), 1000);
  assert.ok(logsText().includes('line 1499'));
});

test('errors keep their message, and clearing empties it', () => {
  clearLogs();
  log('error', new Error('boom'));
  assert.ok(logsText().includes('boom'));
  clearLogs();
  assert.equal(logsText(), '');
});
