// node --test client/test/vetted.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { VETTED_MAKERS, vettedMaker } from '../site/js/vetted.js';

const entries = Object.entries(VETTED_MAKERS).flatMap(([network, list]) => list.map((m) => ({ network, ...m })));

test('every listed key can match, because a badly typed one would silently never show', () => {
  for (const m of entries) {
    assert.match(m.key, /^[0-9a-f]{64}$/, m.name);
    assert.ok(m.name.trim(), m.key);
  }
  for (const list of Object.values(VETTED_MAKERS)) assert.equal(new Set(list.map((m) => m.key)).size, list.length, 'duplicate key');
});

test('this build lists test makers only', () => {
  assert.deepEqual(Object.keys(VETTED_MAKERS), ['testnet']);
});

test('only the whole key on the right network is vetted', () => {
  const { key } = VETTED_MAKERS.testnet[0];
  assert.equal(vettedMaker('testnet', key).key, key);
  assert.equal(vettedMaker('mainnet', key), null, 'a test maker is not vetted elsewhere');
  const middle = key[30] === '0' ? '1' : '0';
  assert.equal(vettedMaker('testnet', key.slice(0, 30) + middle + key.slice(31)), null, 'a key that looks the same on screen');
  assert.equal(vettedMaker('testnet', key.toUpperCase()), null);
  assert.equal(vettedMaker('testnet', key.slice(0, 32)), null);
  assert.equal(vettedMaker('testnet', ''), null);
  assert.equal(vettedMaker('testnet', undefined), null);
});
