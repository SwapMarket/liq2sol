// node --test client/test/store.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';

const memory = new Map();
globalThis.localStorage = { getItem: (k) => memory.get(k) ?? null, setItem: (k, v) => memory.set(k, v) };
const { loadSwaps, saveSwap, withSwap } = await import('../site/js/store.js');

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// The bug this exists for: a slow tick saving its old copy over what a button or a relay message wrote while it waited.
test('a slow update never overwrites what happened while it waited', async () => {
  saveSwap({ id: 'a', state: 'lock' });
  const slowTick = withSwap('a', async (s) => {
    await sleep(30);
    s.tip = 7;
    saveSwap(s);
  });
  const button = withSwap('a', async (s) => {
    s.state = 'await_maker';
    saveSwap(s);
  });
  const message = withSwap('a', async (s) => {
    s.maker_share = 'share';
    saveSwap(s);
  });
  await Promise.all([slowTick, button, message]);
  assert.deepEqual(loadSwaps().a, { id: 'a', state: 'await_maker', tip: 7, maker_share: 'share' });
});

test('a failed update stops nothing behind it, and saves nothing of its own', async () => {
  saveSwap({ id: 'b', n: 1 });
  const bad = withSwap('b', async (s) => {
    s.n = 99;
    throw new Error('no');
  });
  const good = withSwap('b', async (s) => {
    s.m = s.n;
    saveSwap(s);
  });
  await assert.rejects(bad);
  await good;
  assert.deepEqual(loadSwaps().b, { id: 'b', n: 1, m: 1 });
});

test('one swap waiting does not hold up another', async () => {
  saveSwap({ id: 'c' });
  saveSwap({ id: 'd' });
  let release;
  const stuck = withSwap('c', () => new Promise((r) => { release = r; }));
  await withSwap('d', async (s) => {
    s.done = true;
    saveSwap(s);
  });
  assert.equal(loadSwaps().d.done, true);
  release();
  await stuck;
});

test('a swap that is gone is skipped', async () => {
  assert.equal(await withSwap('nothing', () => 'ran'), undefined);
});
