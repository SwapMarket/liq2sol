// How to fill an order from the makers book: the best price at each size first, each fill inside what that maker quotes and can pay.
// Pure and exact, with amounts as BigInt in SOL-USDT base units. A maker can still decline, and each fill is its own atomic swap.

export const MAX_FILLS = 5;

const keep = (bps) => BigInt(10000 - bps);

/** What a maker pays for `send`, rounded in its favour, as it prices a quote. */
export const receiveFor = (send, bps) => (send * keep(bps)) / 10000n;

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
 * @param first a maker to prefer over the best price, if the taker picked one
 * @param maxSpreadBps the most the taker will give up on a fill, after rounding, which matters for very small fills
 * @returns { fills: [{ maker, bps, send, receive }], unfilled, receive }
 */
export function planOrder(total, makers, first = null, maxSpreadBps = 10000) {
  const room = new Map(makers.map((m) => [m.maker, m.available]));
  const fills = [];
  let left = total;
  while (left > 0n && fills.length < MAX_FILLS) {
    // Each fill goes to whoever is cheapest at the size it would be, because a maker's price depends on that.
    let best = null;
    for (const m of makers) {
      const fill = fillFor(m, left, room.get(m.maker), maxSpreadBps);
      if (!fill) continue;
      const better = !best || (m.maker === first && best.maker !== first)
        || (best.maker !== first && fill.bps < best.bps);
      if (better) best = fill;
    }
    if (!best) break;
    fills.push(best);
    left -= best.send;
    room.set(best.maker, room.get(best.maker) - best.receive);
  }
  return { fills, unfilled: left, receive: fills.reduce((sum, f) => sum + f.receive, 0n) };
}
