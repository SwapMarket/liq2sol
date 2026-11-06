// How to fill an order from the makers book: the best price at each size first, each fill inside what that maker quotes and can pay.
// Pure and exact, with amounts as BigInt in SOL-USDT base units. A maker can still decline, and each fill is its own atomic swap.

export const MAX_FILLS = 5;

const keep = (bps) => BigInt(10000 - bps);

/** What a maker pays for `send`, rounded in its favour, as it prices a quote. */
export const receiveFor = (send, bps) => (send * keep(bps)) / 10000n;

/** What the taker really pays per unit received across these fills, with rounding and any mix of price steps. */
export const effectivePrice = (fills) => {
  const paid = fills.reduce((sum, f) => sum + f.send, 0n);
  const got = fills.reduce((sum, f) => sum + f.receive, 0n);
  return Number(paid) / Number(got);
};

/** The most a taker can send to a maker that can pay `available`. */
const sendFor = (available, bps) => (available * 10000n) / keep(bps);

/** The rate for a fill: the first step covering what is sent, or null above them all. */
export function bpsAt(tiers, send) {
  return tiers.find((t) => send <= t.up_to)?.bps ?? null;
}

// The largest fill this maker can take of `wanted`. A smaller size can fall into a cheaper step, which pays out more per unit, so this
// tries from the top, and steps down through the sizes where the rate changes.
function fillFor(m, wanted, room, maxSpreadBps) {
  let send = wanted < m.max ? wanted : m.max;
  for (let tries = 0; tries <= m.tiers.length; tries++) {
    const bps = bpsAt(m.tiers, send);
    if (bps === null) return null;
    const payable = sendFor(room, bps);
    if (send <= payable) {
      const receive = receiveFor(send, bps);
      // A maker refuses anything below its minimum, on either side of the trade.
      if (send <= 0n || send < m.min || receive < m.min) return null;
      if ((send - receive) * 10000n > send * BigInt(maxSpreadBps)) return null;
      return { maker: m.maker, bps, send, receive };
    }
    send = payable;
  }
  return null;
}

/**
 * @param total what the taker wants to send
 * @param makers [{ maker, tiers: [{ up_to, bps }], min, max, available }], in any order
 * @param first a maker to prefer over everything, if the taker picked one for this order
 * @param maxSpreadBps the most the taker will give up on a fill, after rounding, which matters for very small fills
 * @param favorites makers to prefer over a better price from anyone else, because the taker trusts them
 * @returns { fills: [{ maker, bps, send, receive }], unfilled, receive }
 */
export function planOrder(total, makers, first = null, maxSpreadBps = 10000, favorites = new Set()) {
  const rank = (maker) => (maker === first ? 2 : favorites.has(maker) ? 1 : 0);
  const room = new Map(makers.map((m) => [m.maker, m.available]));
  const fills = [];
  let left = total;
  while (left > 0n && fills.length < MAX_FILLS) {
    // Each fill goes to whoever is cheapest at the size it would be, because a maker's price depends on that.
    let best = null;
    for (const m of makers) {
      const fill = fillFor(m, left, room.get(m.maker), maxSpreadBps);
      if (!fill) continue;
      const better = !best || rank(m.maker) > rank(best.maker) || (rank(m.maker) === rank(best.maker) && fill.bps < best.bps);
      if (better) best = fill;
    }
    if (!best) break;
    fills.push(best);
    left -= best.send;
    room.set(best.maker, room.get(best.maker) - best.receive);
  }
  return { fills, unfilled: left, receive: fills.reduce((sum, f) => sum + f.receive, 0n) };
}

// The smallest amount this maker takes whole. Its minimum applies to both sides, and the step and rounding move with the size.
function smallestFor(m, maxSpreadBps) {
  let send = m.min > 1n ? m.min : 1n;
  for (let tries = 0; tries < 64 && send <= m.max; tries++) {
    const bps = bpsAt(m.tiers, send);
    if (bps === null) return null;
    const needed = (m.min * 10000n + keep(bps) - 1n) / keep(bps);
    if (needed > send) {
      send = needed;
      continue;
    }
    const fill = fillFor(m, send, m.available, maxSpreadBps);
    if (fill?.send === send) return send;
    send += 1n;
  }
  return null;
}

/**
 * The smallest and largest orders the book can fill completely, as planOrder would fill them. The minimum is exact. The planner is
 * greedy, so with an odd mix of price steps a slightly larger order than the maximum could also fill.
 * @returns { min, max }, or null if no maker can take anything
 */
export function sendLimits(makers, first = null, maxSpreadBps = 10000, favorites = new Set()) {
  const whole = (send) => {
    const plan = planOrder(send, makers, first, maxSpreadBps, favorites);
    return plan.fills.length > 0 && plan.unfilled === 0n;
  };
  const smallest = makers.map((m) => smallestFor(m, maxSpreadBps)).filter((s) => s !== null).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
  const min = smallest.find(whole);
  if (min === undefined) return null;
  // Everything the planner would take from an endless order. A smaller order can be split differently, so check it really fills.
  const endless = 1n << 60n;
  const everything = planOrder(endless, makers, first, maxSpreadBps, favorites);
  let max = endless - everything.unfilled;
  for (let tries = 0; tries < 64 && max > min && !whole(max); tries++) {
    max -= planOrder(max, makers, first, maxSpreadBps, favorites).unfilled || 1n;
  }
  return { min, max: whole(max) ? max : min };
}
