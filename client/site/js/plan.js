// How to fill an order from the makers book: the best price first, each fill inside what that maker quotes and can pay.
// Pure and exact, with amounts as BigInt in SOL-USDT base units. A maker can still decline, and each fill is its own atomic swap.

export const MAX_FILLS = 5;

const keep = (bps) => BigInt(10000 - bps);

/** What a maker pays for `send`, rounded in its favour, as it prices a quote. */
export const receiveFor = (send, bps) => (send * keep(bps)) / 10000n;

/** The most a taker can send to a maker that can pay `available`. */
const sendFor = (available, bps) => (available * 10000n) / keep(bps);

/**
 * @param total what the taker wants to send
 * @param makers [{ maker, bps, min, max, available }], in any order
 * @param first a maker to prefer over the best price, if the taker picked one
 * @param maxSpreadBps the most the taker will give up on a fill, after rounding, which matters for very small fills
 * @returns { fills: [{ maker, bps, send, receive }], unfilled, receive }
 */
export function planOrder(total, makers, first = null, maxSpreadBps = 10000) {
  const order = [...makers].sort((a, b) => (a.maker === first ? -1 : b.maker === first ? 1 : a.bps - b.bps));
  const fills = [];
  let left = total;
  for (const m of order) {
    let room = m.available;
    while (left > 0n && fills.length < MAX_FILLS) {
      let send = left < m.max ? left : m.max;
      const payable = sendFor(room, m.bps);
      if (payable < send) send = payable;
      const receive = receiveFor(send, m.bps);
      // A maker refuses anything below its minimum, on either side of the trade.
      if (send <= 0n || send < m.min || receive < m.min) break;
      if ((send - receive) * 10000n > send * BigInt(maxSpreadBps)) break;
      fills.push({ maker: m.maker, bps: m.bps, send, receive });
      left -= send;
      room -= receive;
    }
  }
  return { fills, unfilled: left, receive: fills.reduce((sum, f) => sum + f.receive, 0n) };
}
