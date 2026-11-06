// node --test client/test/plan.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { MAX_FILLS, bpsAt, planOrder, receiveFor } from '../site/js/plan.js';

const flat = (bps, up_to = 1n << 60n) => [{ up_to, bps }];
const maker = (name, bps, max, available, min = 10n) => ({ maker: name, tiers: flat(bps), min, max, available });

test('one maker that can take it all, at the best price', () => {
  const p = planOrder(1000n, [maker('dear', 100, 5000n, 5000n), maker('cheap', 30, 5000n, 5000n)]);
  assert.deepEqual(p.fills.map((f) => [f.maker, f.send]), [['cheap', 1000n]]);
  assert.equal(p.unfilled, 0n);
  assert.equal(p.receive, receiveFor(1000n, 30));
});

test('an order larger than the best offer flows to the next', () => {
  const p = planOrder(1500n, [maker('dear', 100, 5000n, 5000n), maker('cheap', 30, 1000n, 5000n)]);
  assert.deepEqual(p.fills.map((f) => [f.maker, f.send]), [['cheap', 1000n], ['cheap', 500n]], 'the same maker again, while it can pay');
  const q = planOrder(1500n, [maker('dear', 100, 5000n, 5000n), maker('cheap', 30, 1000n, 700n)]);
  assert.equal(q.fills[0].maker, 'cheap');
  assert.ok(q.fills[0].receive <= 700n, 'never above what it can pay');
  assert.equal(q.fills.at(-1).maker, 'dear');
  assert.equal(q.unfilled, 0n);
});

test('the fills add up exactly, and nobody is asked for more than they can pay', () => {
  const makers = [maker('a', 10, 333n, 500n), maker('b', 50, 400n, 390n), maker('c', 99, 1000n, 600n)];
  for (const total of [1n, 10n, 11n, 333n, 334n, 999n, 1000n, 1500n, 5000n]) {
    const p = planOrder(total, makers);
    assert.equal(p.fills.reduce((s, f) => s + f.send, 0n) + p.unfilled, total, `${total}`);
    for (const m of makers) {
      const paid = p.fills.filter((f) => f.maker === m.maker).reduce((s, f) => s + f.receive, 0n);
      assert.ok(paid <= m.available, `${m.maker} pays ${paid} of ${total}`);
    }
    for (const f of p.fills) {
      const m = makers.find((x) => x.maker === f.maker);
      assert.ok(f.send <= m.max && f.send >= m.min && f.receive >= m.min, `${f.maker} ${f.send}`);
      assert.equal(f.receive, receiveFor(f.send, m.tiers[0].bps));
    }
  }
});

test('what the book can’t cover is reported, not hidden', () => {
  const p = planOrder(1000n, [maker('a', 30, 300n, 300n)]);
  assert.equal(p.fills.length, 1);
  assert.equal(p.unfilled, 1000n - p.fills[0].send);
  assert.equal(planOrder(5n, [maker('a', 30, 300n, 300n)]).unfilled, 5n, 'below every minimum');
  assert.equal(planOrder(100n, []).fills.length, 0);
  assert.equal(planOrder(100n, [maker('a', 30, 300n, 0n)]).unfilled, 100n, 'a maker with nothing available');
});

test('the number of swaps is capped, because each needs its own payment or approval', () => {
  const p = planOrder(100000n, [maker('a', 30, 100n, 100000n)]);
  assert.equal(p.fills.length, MAX_FILLS);
  assert.equal(p.unfilled, 100000n - 100n * BigInt(MAX_FILLS));
});

test('a maker the taker picked comes first, even at a worse price', () => {
  const p = planOrder(100n, [maker('cheap', 30, 500n, 500n), maker('mine', 90, 500n, 500n)], 'mine');
  assert.equal(p.fills[0].maker, 'mine');
});

test('a fill whose rounding breaks the taker’s spread limit is not proposed', () => {
  // 120 fills, and 2 more units would receive 1, which is a 50% spread.
  const p = planOrder(122n, [maker('a', 100, 120n, 120n, 1n)], null, 200);
  assert.deepEqual(p.fills.map((f) => f.send), [120n]);
  assert.equal(p.unfilled, 2n);
  assert.equal(planOrder(122n, [maker('a', 100, 120n, 120n, 1n)]).fills.length, 2, 'with no limit it would have been');
});

test('what a maker can pay may be more than one swap’s limit', () => {
  const p = planOrder(170n, [maker('a', 100, 120n, 401n, 1n)], null, 200);
  assert.deepEqual(p.fills.map((f) => f.send), [120n, 50n]);
  assert.equal(p.unfilled, 0n);
});

test('the price comes from the step covering the fill’s size', () => {
  const tiers = [{ up_to: 100n, bps: 20 }, { up_to: 1000n, bps: 40 }];
  assert.equal(bpsAt(tiers, 100n), 20);
  assert.equal(bpsAt(tiers, 101n), 40);
  assert.equal(bpsAt(tiers, 1001n), null);
  const m = { maker: 'a', tiers, min: 10n, max: 1000n, available: 100000n };
  assert.deepEqual(planOrder(50n, [m]).fills.map((f) => f.bps), [20]);
  assert.deepEqual(planOrder(500n, [m]).fills.map((f) => f.bps), [40]);
});

test('makers are compared at the size being filled, not by their best step', () => {
  const stepped = { maker: 'stepped', tiers: [{ up_to: 100n, bps: 10 }, { up_to: 10000n, bps: 90 }], min: 10n, max: 10000n, available: 100000n };
  const level = { maker: 'level', tiers: flat(50), min: 10n, max: 10000n, available: 100000n };
  assert.equal(planOrder(100n, [level, stepped]).fills[0].maker, 'stepped', 'small: its cheap step wins');
  assert.equal(planOrder(5000n, [stepped, level]).fills[0].maker, 'level', 'large: the level price wins');
});

test('short of funds, a fill can drop into a cheaper step, and still never pays more than the maker has', () => {
  const m = { maker: 'a', tiers: [{ up_to: 100n, bps: 10 }, { up_to: 1000n, bps: 90 }], min: 10n, max: 1000n, available: 95n };
  const p = planOrder(500n, [m]);
  assert.ok(p.receive <= 95n, `${p.receive}`);
  for (const f of p.fills) assert.equal(f.bps, bpsAt(m.tiers, f.send));
  assert.equal(p.fills.reduce((s, f) => s + f.send, 0n) + p.unfilled, 500n);
});

test('favorites fill first, even at a worse price, and a fake bargain can’t get ahead of them', () => {
  const makers = [maker('bargain', 1, 500n, 500n), maker('mine', 90, 500n, 500n), maker('also', 50, 500n, 500n)];
  const favorites = new Set(['mine', 'also']);
  assert.equal(planOrder(100n, makers).fills[0].maker, 'bargain', 'without favorites, the best price wins');
  assert.equal(planOrder(100n, makers, null, 10000, favorites).fills[0].maker, 'also', 'the better of the two favorites');
  const big = planOrder(1200n, makers, null, 10000, favorites);
  assert.deepEqual([...new Set(big.fills.map((f) => f.maker))], ['also', 'mine', 'bargain'], 'others only for what favorites can’t fill');
  assert.equal(planOrder(100n, makers, 'bargain', 10000, favorites).fills[0].maker, 'bargain', 'picking a maker for this order beats a favorite');
});
