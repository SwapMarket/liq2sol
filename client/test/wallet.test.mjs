// node --test client/test/wallet.test.mjs
// What each wallet is sent to sign over WalletConnect: Trust Wallet gets v0, everyone else the transaction as built.
import test from 'node:test';
import assert from 'node:assert/strict';

globalThis.localStorage = { getItem: () => null, setItem: () => {}, removeItem: () => {} };
globalThis.location = { reload: () => {} };
// The Wallet Standard handshake runs when wallet.js loads; no wallet answers here.
globalThis.window = { addEventListener: () => {}, dispatchEvent: () => true };
globalThis.CustomEvent ??= class { constructor(type, init) { this.type = type; this.detail = init?.detail; } };
const { asV0, walletConnectTransaction } = await import('../site/js/wallet.js');

// A legacy transaction: one empty signature slot, then a message whose first byte (the number of required signatures) is below 0x80.
const message = Uint8Array.from([1, 0, 1, 2, ...new Array(32 * 2).fill(7), ...new Array(32).fill(9), 1, 1, 2, 0, 1, 0]);
const legacy = Uint8Array.from([1, ...new Array(64).fill(0), ...message]);

test('Trust Wallet gets a v0 transaction over WalletConnect: the same message with the version prefix and no lookup tables', () => {
  for (const name of ['Trust Wallet', 'Trust', 'trust wallet']) {
    const sent = walletConnectTransaction(name, legacy);
    assert.equal(sent.length, legacy.length + 2, name);
    assert.equal(sent[1 + 64], 0x80, 'the version prefix, where the message starts');
    assert.deepEqual(sent.subarray(1 + 64 + 1, sent.length - 1), message, 'the message itself unchanged');
    assert.equal(sent[sent.length - 1], 0, 'an empty list of lookup tables');
    assert.deepEqual(sent, asV0(legacy));
  }
});

test('every other wallet gets exactly the transaction as built', () => {
  for (const name of ['Phantom', 'Solflare', 'Backpack', undefined, '']) assert.equal(walletConnectTransaction(name, legacy), legacy, String(name));
});

test('a transaction that is already v0 is sent as it is, even to Trust Wallet', () => {
  const v0 = asV0(legacy);
  assert.equal(walletConnectTransaction('Trust Wallet', v0), v0);
});
