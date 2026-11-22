// Which form of a Solana transaction each wallet is sent. Pure, with no imports, so it can be tested without the page's WebAssembly.

// The same transaction as a v0 one: the message gets the version prefix and an empty list of lookup tables, and the instructions
// don't change. Solana accepts both. The bytes start as a compact count of signature slots, all empty, then the message.
export function asV0(tx) {
  let count = 0, at = 0, shift = 0, byte;
  do { byte = tx[at++]; count |= (byte & 0x7f) << shift; shift += 7; } while (byte & 0x80);
  const message = tx.subarray(at + 64 * count);
  if (message[0] & 0x80) return tx;
  const out = new Uint8Array(tx.length + 2);
  out.set(tx.subarray(0, at + 64 * count));
  out[at + 64 * count] = 0x80;
  out.set(message, at + 64 * count + 1);
  out[out.length - 1] = 0;
  return out;
}

// Over WalletConnect, Trust Wallet takes a legacy transaction, lets the person approve it, and never answers, so the page waits on a
// signature already given (reported 2026-11-22; others saw Trust fail on legacy and sign v0, cowprotocol/cowswap#8693). It gets v0
// there too. Other wallets keep what they have always been sent.
export const walletConnectTransaction = (peerName, tx) => (/trust/i.test(peerName ?? '') ? asV0(tx) : tx);
