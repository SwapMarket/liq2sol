// node --test client/test/plan.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { MAX_FILLS, bpsAt, effectivePrice, planOrder, receiveFor, sendLimits } from '../site/js/plan.js';

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
  assert.deepEqual(p.fills.map((f) => [f.maker, f.send]), [['cheap', 1000n], ['dear', 500n]], 'one swap per maker');
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
  const many = Array.from({ length: MAX_FILLS + 3 }, (_, i) => maker(`m${i}`, 30 + i, 100n, 100000n));
  const p = planOrder(100000n, many);
  assert.equal(p.fills.length, MAX_FILLS);
  assert.equal(p.unfilled, 100000n - 100n * BigInt(MAX_FILLS));
});

test('a maker the taker picked comes first, even at a worse price', () => {
  const p = planOrder(100n, [maker('cheap', 30, 500n, 500n), maker('mine', 90, 500n, 500n)], 'mine');
  assert.equal(p.fills[0].maker, 'mine');
});

test('a fill whose rounding breaks the taker’s spread limit is not proposed', () => {
  // 120 fills, and 2 more units would receive 1, which is a 50% spread.
  const makers = [maker('a', 100, 120n, 120n, 1n), maker('b', 100, 120n, 120n, 1n)];
  const p = planOrder(122n, makers, null, 200);
  for (const f of p.fills) assert.ok((f.send - f.receive) * 10000n <= f.send * 200n, `${f.send} gives up too much`);
  assert.ok(p.fills.every((f) => f.send > 2n), 'not the 2 left over, which would receive 1');
  assert.equal(p.unfilled, 0n, 'a fill gives up some size, so the second one can be a sensible one');
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

test('the effective price is what the fills really pay, not the quoted step', () => {
  assert.equal(effectivePrice([{ send: 1_000_000n, receive: 997_000n }]).toFixed(4), '1.0030');
  assert.equal(effectivePrice([{ send: 100n, receive: 99n }]).toFixed(4), '1.0101', 'rounding at a tiny size');
  assert.equal(effectivePrice([{ send: 1_000_000n, receive: 997_000n }, { send: 1_000_000n, receive: 995_000n }]).toFixed(4), '1.0040', 'a mix of steps');
  assert.equal(effectivePrice([{ send: 2_000n, receive: 1_994n }, { send: 1_000n, receive: 997n }]).toFixed(4), '1.0030');
});

test('send limits: the minimum counts both sides of the trade, and the maximum what makers can pay', () => {
  const limits = sendLimits([maker('a', 30, 2000n, 100000n, 1000n)]);
  assert.equal(limits.min, 1004n, '1000 is the minimum, but it must also pay out 1000');
  assert.equal(receiveFor(limits.min, 30) >= 1000n, true);
  assert.equal(receiveFor(limits.min - 1n, 30) < 1000n, true);
  assert.equal(limits.max, 2000n, 'its largest single swap');
  assert.equal(sendLimits([]), null);
  assert.equal(sendLimits([maker('dry', 30, 1000n, 0n)]), null, 'a maker that can pay nothing');
  assert.equal(sendLimits([maker('tight', 30, 1000n, 500n, 1000n)]), null, 'its minimum is more than it can pay');
});

test('send limits: the maximum stops at what the makers can pay, across several swaps', () => {
  const { max } = sendLimits([maker('a', 100, 1000n, 1485n), maker('b', 50, 1000n, 995n)]);
  const plan = planOrder(max, [maker('a', 100, 1000n, 1485n), maker('b', 50, 1000n, 995n)]);
  assert.equal(plan.unfilled, 0n);
  assert.equal(plan.fills.length > 1, true);
  assert.equal(planOrder(max + 100n, [maker('a', 100, 1000n, 1485n), maker('b', 50, 1000n, 995n)]).unfilled > 0n, true);
});

test('send limits always fill completely, on any book, and nothing below the minimum does', () => {
  let seed = 12345;
  const next = (n) => { seed = (seed * 1103515245 + 12345) % 2147483648; return seed % n; };
  for (let round = 0; round < 500; round++) {
    const makers = Array.from({ length: 1 + next(4) }, (_, i) => {
      const a = BigInt(1 + next(4000));
      const b = a + BigInt(1 + next(40000));
      return {
        maker: `m${i}`,
        tiers: [{ up_to: a, bps: 10 + next(100) }, { up_to: b, bps: 10 + next(200) }],
        min: BigInt(1 + next(500)),
        max: b,
        available: BigInt(next(60000)),
      };
    });
    const spread = next(2) ? 10000 : 250;
    const favorites = new Set(next(2) ? ['m0'] : []);
    const limits = sendLimits(makers, null, spread, favorites);
    if (!limits) {
      assert.equal(planOrder(1000n, makers, null, spread, favorites).fills.length, 0, `round ${round}: a fillable book had no limits`);
      continue;
    }
    for (const send of [limits.min, limits.max]) {
      const plan = planOrder(send, makers, null, spread, favorites);
      assert.equal(plan.unfilled, 0n, `round ${round}: ${send} left ${plan.unfilled}`);
      assert.ok(plan.fills.length >= 1 && plan.fills.length <= MAX_FILLS);
    }
    assert.ok(limits.min <= limits.max, `round ${round}`);
    assert.ok(planOrder(limits.min - 1n, makers, null, spread, favorites).unfilled > 0n, `round ${round}: below the minimum`);
    // The planner is greedy, so an odd mix of steps can leave the true largest order slightly higher. It was 0.6% at worst over these books.
    assert.ok(planOrder((limits.max * 102n) / 100n + 1n, makers, null, spread, favorites).unfilled > 0n, `round ${round}: well above the maximum`);
  }
});

test('one swap per maker, however much it could pay, because its limit per swap is its own to set', () => {
  const p = planOrder(1000n, [maker('a', 30, 100n, 100000n)]);
  assert.equal(p.fills.length, 1);
  assert.equal(p.unfilled, 900n);
  assert.equal(sendLimits([maker('a', 30, 100n, 100000n)]).max, 100n, 'send max follows');
  const two = planOrder(150n, [maker('a', 30, 100n, 100000n), maker('b', 90, 100n, 100000n)]);
  assert.deepEqual(two.fills.map((f) => [f.maker, f.send]), [['a', 100n], ['b', 50n]]);
});

test('a fill gives up some of its size, when what’s left is below the next maker’s minimum', () => {
  const top = { maker: 'top', tiers: [{ up_to: 5_000_000n, bps: 30 }, { up_to: 25_000_000n, bps: 20 }], min: 100_000n, max: 25_000_000n, available: 100_000_000n };
  const bottom = { maker: 'bottom', tiers: [{ up_to: 10_000_000n, bps: 50 }, { up_to: 50_000_000n, bps: 40 }], min: 10_000_000n, max: 50_000_000n, available: 100_000_000n };
  const p = planOrder(30_000_000n, [top, bottom]);
  assert.equal(p.unfilled, 0n, 'all of it fills');
  assert.deepEqual(p.fills.map((f) => f.maker), ['top', 'bottom']);
  assert.equal(p.fills[0].send + p.fills[1].send, 30_000_000n);
  assert.ok(p.fills[1].receive >= bottom.min, 'the second maker’s minimum, on what it pays too');
  assert.ok(p.fills[1].send < 10_100_000n, 'only as much as that needs, so the cheaper maker keeps the rest');
  assert.equal(planOrder(25_000_000n, [top, bottom]).fills.length, 1, 'no split when one maker can take it all');
});

test('a fill won’t give up so much that it falls below its own maker’s minimum', () => {
  const top = maker('top', 20, 25n, 1000n, 20n);
  const bottom = maker('bottom', 40, 50n, 1000n, 20n);
  const p = planOrder(26n, [top, bottom]);
  assert.equal(p.fills.length, 1, '6 would be below the top maker’s minimum of 20');
  assert.ok(p.unfilled > 0n);
});
