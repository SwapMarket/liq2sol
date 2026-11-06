// One swap's state machine, from docs/protocol.md sections 4 and 5. It is resumed from storage after a reload, and each tick reads
// the chains again, so nothing depends on this page having stayed open. Decisions about funds and secrets are in the WASM.
import * as W from '../wasm/liq2sol_client_wasm.js';
import { NETWORK } from './config.js';
import { hexToBytes } from './chain.js';
import { log } from './logs.js';
import { saveSwap } from './store.js';

const now = () => Math.floor(Date.now() / 1000);

export const rulesJson = (maxSpread = NETWORK.maxSpreadBps) => JSON.stringify({
  network: NETWORK.name, program_id: NETWORK.programId, usdt_asset: NETWORK.usdt, max_spread_bps: maxSpread, ...NETWORK.rules,
});

export const FINISHED = ['done', 'refunded', 'expired'];

const swapArgs = (s) => ({
  direction: s.direction, maker_key: s.terms.maker_key, taker_key: s.secrets.taker_key,
  blinding_key: s.secrets.blinding_key, lock_height: s.terms.lock_height,
});

/** Everything derived from agreed terms. The address is never taken from the maker. */
export function deriveSwap(s) {
  s.info = JSON.parse(W.swap_info(JSON.stringify(swapArgs(s))));
  const initiator = s.direction === 'L2S' ? s.terms.maker_initiator : s.solana_owner;
  s.escrow_address = W.escrow_address(initiator, s.secrets.lock_point_64, NETWORK.programId);
  return s;
}

// Elements' refusals of a transaction it already has, in a block or in its mempool.
const ALREADY_SENT = /already in block chain|already.*mempool|txn-already/i;

export class Engine {
  constructor({ esplora, solana, secondSolana, getWallet, relays, onChange, allowOneEndpoint, feeCoinSats }) {
    Object.assign(this, { esplora, solana, secondSolana, getWallet, relays, onChange, allowOneEndpoint, feeCoinSats });
    this.sessions = new Map();
    this.pendingOffers = new Map();
  }

  save(s) {
    if (s.logged_state !== s.state) {
      log('info', `swap ${s.id.slice(0, 8)} ${s.direction}: ${s.logged_state ?? 'new'} -> ${s.state}`, s.status ?? '');
      s.logged_state = s.state;
    }
    saveSwap(s);
    this.onChange(s);
  }

  set(s, fields) {
    Object.assign(s, fields);
    this.save(s);
  }

  send(s, body) {
    const { event } = JSON.parse(W.wrap_message(s.nostr.secret, s.maker, s.id, NETWORK.name, JSON.stringify(body), BigInt(now())));
    this.relays.publish(event);
  }

  // Coins at the swap's address that are exactly what was agreed. A wrong coin is reported, and never acted on.
  async exactCoins(esplora, s) {
    const coins = await esplora.readCoins(s.info.plain_address, s.info.script, s.secrets.blinding_key);
    return coins.filter((c) => c.is_usdt && c.amount === s.terms.liquid_amount);
  }

  async wrongCoins(s) {
    const coins = await this.esplora[0].readCoins(s.info.plain_address, s.info.script, s.secrets.blinding_key);
    return coins.filter((c) => !(c.is_usdt && c.amount === s.terms.liquid_amount));
  }

  checkEscrow(e, s, slot) {
    if (!e || e.state !== 'Open') return 'it is not open';
    if (e.amount !== s.terms.solana_amount) return 'the amount is wrong';
    if (e.lock_point !== s.secrets.lock_point_64) return 'it locks to another point';
    if (e.recipient !== s.solana_owner) return 'it pays someone else';
    if (e.deadline_slot - slot < NETWORK.claimCutoffSlots) return 'it is too close to its deadline to safely reveal the secret';
    return '';
  }

  // Revealing the secret against a lock that isn't there would give the swap away, so the lock is confirmed on two endpoints first.
  async secondOpinionOnEscrow(s) {
    if (!this.secondSolana()) {
      if (this.allowOneEndpoint()) return '';
      return 'one-endpoint';
    }
    const second = this.secondSolana();
    return this.checkEscrow(await second.escrow(s.escrow_address), s, await second.slot());
  }

  // ---- L to S ----

  async tickL2S(s, tip) {
    const slot = await this.solana.slot();
    const escrow = await this.solana.escrow(s.escrow_address);
    if (escrow && escrow.state === 'Claimed' && s.state !== 'done') {
      this.set(s, { state: 'done', status: 'You were paid on Solana. The maker can now collect the Liquid coin.' });
      return;
    }
    if (s.state === 'fund' || s.state === 'await_maker') {
      const coins = await this.exactCoins(this.esplora[0], s);
      const wrong = await this.wrongCoins(s);
      s.wrong_coin = wrong.length > 0;
      const coin = coins[0];
      if (coin) {
        s.coin = { txid: coin.txid, vout: coin.vout, confirmations: coin.confirmations };
        if (s.state === 'fund') s.state = 'await_maker';
        await this.answerPendingOffer(s);
      }
    }
    if (s.state === 'await_maker' || s.state === 'ready_claim') {
      const problem = escrow ? this.checkEscrow(escrow, s, slot) : 'no escrow yet';
      s.escrow_problem = escrow && problem ? problem : '';
      if (!problem) {
        s.escrow = escrow;
        s.state = 'ready_claim';
        s.slots_left = escrow.deadline_slot - slot;
        s.status = '';
      } else if (s.state === 'ready_claim') {
        s.state = 'await_maker';
      }
    }
    if (!['done', 'refund_wait', 'refund'].includes(s.state)) {
      // The maker's gate leaves 210 blocks when it locks, so past that it won't. With no coin, there is nothing to get back either.
      const makerGone = tip > s.terms.lock_height - 200 && !escrow;
      const refunded = escrow?.state === 'Refunded';
      if (s.state === 'fund' && tip >= s.terms.lock_height) s.state = 'expired';
      else if (makerGone || refunded || tip >= s.terms.lock_height) s.state = 'refund_wait';
    }
    if (s.state === 'refund_wait' && tip >= s.terms.lock_height) s.state = 'refund';
    if (s.state === 'refund' || s.state === 'refunding') await this.tickRefund(s);
    s.tip = tip;
    this.save(s);
  }

  async answerPendingOffer(s) {
    const offer = this.pendingOffers.get(s.id);
    if (!offer || !s.coin || s.answered === offer.nonce) return;
    // Marked first, because a tick and an arriving message can both get here while this waits on the network.
    s.answered = offer.nonce;
    const claim = offer.tx;
    const args = {
      swap: swapArgs(s), taker_sk: s.secrets.taker_sk, lock_point: s.secrets.lock_point,
      coin_tx: await this.esplora[0].txHex(s.coin.txid), coin_vout: s.coin.vout,
      claim_tx: claim, maker_nonce: offer.nonce, parents: await this.esplora[0].parentsOf(claim), genesis: NETWORK.genesis,
    };
    // A nonce is made and used inside this call, so answering twice only wastes a little work.
    const answer = JSON.parse(W.answer_claim_offer(JSON.stringify(args)));
    this.send(s, { type: 'claim-answer', body: { lock_point: s.secrets.lock_point, nonce: answer.nonce, share: answer.share } });
    this.save(s);
  }

  async claimOnSolana(s) {
    const wallet = this.getWallet();
    if (!wallet || wallet.address !== s.solana_owner) throw new Error('Connect the Solana wallet this swap pays.');
    const escrow = await this.solana.escrow(s.escrow_address);
    const problem = this.checkEscrow(escrow, s, await this.solana.slot());
    if (problem) throw new Error(`The secret stays hidden: ${problem}.`);
    const second = await this.secondOpinionOnEscrow(s);
    if (second) throw new Error(second === 'one-endpoint' ? 'Add a second Solana endpoint in Settings, or allow one endpoint there.' : `The second endpoint disagrees: ${second}.`);
    const fee = await this.solana.priorityFee([s.escrow_address, NETWORK.programId], NETWORK.priorityFloor, NETWORK.priorityCap);
    const unsigned = W.build_claim_tx(NETWORK.programId, NETWORK.mint, wallet.address, s.escrow_address, escrow.recipient, escrow.rent_payer,
      W.revealed_value(s.secrets.lock_secret), await this.solana.blockhash(), BigInt(fee));
    this.set(s, { status: `Waiting for ${wallet.name} to sign the claim…` });
    const signed = await wallet.sign(hexToBytes(unsigned));
    // A phone can take a while, so check the deadline again. A signature on its own reveals nothing until it is sent.
    const late = this.checkEscrow(await this.solana.escrow(s.escrow_address), s, await this.solana.slot());
    if (late) throw new Error(`The secret stays hidden: ${late}.`);
    await this.solana.simulate(signed);
    this.set(s, { status: 'Sending the claim…', claim_sig: await this.solana.send(signed) });
  }

  // ---- S to L ----

  async lockOnSolana(s) {
    const wallet = this.getWallet();
    if (!wallet) throw new Error('Connect a Solana wallet first.');
    if (s.state !== 'lock') return;
    if (await this.solana.escrow(s.escrow_address, 'confirmed')) return;
    if (await this.esplora[0].tip() > s.terms.h_accept + 15) throw new Error('The time to lock has passed, so the maker would no longer fill this. Nothing was sent.');
    const account = W.token_account(wallet.address, NETWORK.mint);
    if (await this.solana.tokenBalance(account) < BigInt(s.terms.solana_amount)) throw new Error('This wallet holds too little USDT.');
    const deadline = (await this.solana.slot()) + Math.max(s.terms.min_deadline_slots, 0) + 300;
    const fee = await this.solana.priorityFee([account], NETWORK.priorityFloor, NETWORK.priorityCap);
    const unsigned = W.build_lock_tx(NETWORK.programId, NETWORK.mint, wallet.address, s.secrets.lock_point_64, s.terms.maker_solana,
      s.terms.solana_amount, BigInt(deadline), await this.solana.blockhash(), BigInt(fee));
    this.set(s, { status: `Waiting for ${wallet.name} to sign the lock…` });
    const signed = await wallet.sign(hexToBytes(unsigned));
    await this.solana.simulate(signed);
    await this.solana.send(signed);
    this.set(s, { state: 'await_maker', status: 'Locked. Waiting for the maker to lock on Liquid.' });
  }

  async tickS2L(s, tip) {
    const slot = await this.solana.slot();
    const escrow = await this.solana.escrow(s.escrow_address);
    s.tip = tip;
    s.slot = slot;
    if (s.state === 'refunding') {
      if (escrow?.state === 'Refunded') s.state = 'refunded';
    }
    if (escrow?.state === 'Refunded' && !s.claim_tx) s.state = 'refunded';
    if (s.claim_tx) await this.tickClaimed(s);
    else if (['await_maker', 'signing'].includes(s.state)) {
      const pastClaim = tip >= s.terms.lock_height - NETWORK.claimCutoffBlocks;
      const coins = await this.exactCoins(this.esplora[0], s);
      const deep = coins.find((c) => c.confirmations >= NETWORK.liquidConfirmations);
      if (pastClaim || (escrow && slot >= escrow.deadline_slot)) {
        s.state = 'refund_wait';
      } else if (deep) {
        s.coin = { txid: deep.txid, vout: deep.vout, confirmations: deep.confirmations };
        await this.runSession(s, deep);
      }
    }
    if (s.state === 'refund_wait' && escrow && slot >= escrow.deadline_slot) s.state = 'refund_ready';
    this.save(s);
  }

  // The maker's coin must look the same from a second source before anything is signed, and the fee coin must be there.
  async runSession(s, coin) {
    if (s.signed && s.maker_share) return this.finishClaim(s);
    const second = this.esplora[1];
    if (second) {
      const other = await this.exactCoins(second, s).catch(() => []);
      if (!other.some((c) => c.txid === coin.txid && c.vout === coin.vout && c.confirmations >= NETWORK.liquidConfirmations)) {
        s.status = 'Waiting for the second Liquid source to see the maker’s coin.';
        return undefined;
      }
    }
    const fees = await this.esplora[0].readCoins(s.info.plain_fee_address, s.info.fee_script, s.secrets.blinding_key);
    const fee = fees.find((c) => c.is_lbtc && c.confirmations >= 1);
    if (!fee) {
      s.status = 'Waiting for the fee coin the maker sends.';
      return undefined;
    }
    s.state = 'signing';
    const open = this.sessions.get(s.id);
    // Already in the air: give it time. The maker keeps one session per swap, and a retry starts a fresh nonce.
    if (open && Date.now() - open.at < 30000) return undefined;
    if (s.signed) {
      this.send(s, { type: 'taker-share', body: { lock_point: s.secrets.lock_point, share: s.signed.share } });
      this.sessions.set(s.id, { at: Date.now() });
      return undefined;
    }
    const session = new W.ClaimSession(JSON.stringify({
      swap: swapArgs(s), taker_sk: s.secrets.taker_sk, lock_point: s.secrets.lock_point,
      coin_tx: coin.hex, coin_vout: coin.vout, fee_tx: await this.esplora[0].txHex(fee.txid), fee_vout: fee.vout,
      to_address: s.to_address, genesis: NETWORK.genesis,
    }));
    this.sessions.set(s.id, { at: Date.now(), session });
    this.send(s, { type: 'session-open', body: { lock_point: s.secrets.lock_point, tx: session.tx(), nonce: session.nonce() } });
    s.status = 'Signing the claim together with the maker…';
    return undefined;
  }

  onSessionNonce(s, nonce) {
    const open = this.sessions.get(s.id);
    if (!open?.session) return;
    const share = open.session.sign(nonce);
    // Saved before our share goes out: the maker answers once, and only this state can use its answer.
    s.signed = JSON.parse(open.session.signed_state());
    this.save(s);
    open.session.free();
    open.session = null;
    open.at = Date.now();
    this.send(s, { type: 'taker-share', body: { lock_point: s.secrets.lock_point, share } });
  }

  async onMakerShare(s, share) {
    if (!s.signed || s.claim_tx) return;
    s.maker_share = share;
    this.save(s);
    await this.finishClaim(s);
  }

  async finishClaim(s) {
    const tip = await this.esplora[0].tip();
    if (tip >= s.terms.lock_height - NETWORK.claimCutoffBlocks) {
      this.set(s, { state: 'refund_wait', status: 'Too close to the maker’s refund to reveal the secret, so it stays hidden.' });
      return;
    }
    const escrow = await this.solana.escrow(s.escrow_address);
    if (!escrow || escrow.state !== 'Open') throw new Error('Our Solana lock is no longer open, so the claim was not published.');
    const hex = W.finish_claim(JSON.stringify({
      swap: swapArgs(s), taker_sk: s.secrets.taker_sk, lock_point: s.secrets.lock_point, lock_secret: s.secrets.lock_secret,
      signed: s.signed, parents: await this.esplora[0].parentsOf(s.signed.tx), maker_share: s.maker_share, genesis: NETWORK.genesis,
    }));
    this.set(s, { claim_tx: hex, state: 'claiming', status: 'Publishing the claim…' });
    await this.tickClaimed(s);
  }

  // Published means the maker can read the secret, so keep sending until it confirms, or the maker could take both.
  async tickClaimed(s) {
    // From the transaction, not the reply: a lost reply would otherwise leave this waiting on a claim that has already confirmed.
    s.claim_txid ??= W.tx_id(s.claim_tx);
    const status = await this.esplora[0].status(s.claim_txid).catch(() => null);
    if (status?.confirmed) {
      s.state = 'done';
      s.status = 'You were paid on Liquid.';
      return;
    }
    try {
      await this.esplora[0].broadcast(s.claim_tx);
      s.status = 'Claim published, waiting for a confirmation.';
    } catch (e) {
      if (!ALREADY_SENT.test(e.message)) s.status = `Still trying to publish the claim: ${e.message.slice(0, 120)}`;
    }
  }

  async refundOnSolana(s) {
    const wallet = this.getWallet();
    if (!wallet) throw new Error('Connect a Solana wallet to pay the network fee. It can be any wallet: the refund only pays the swap’s owner.');
    if (s.claim_tx) throw new Error('The claim was published, so this swap can’t be refunded.');
    const e = await this.solana.escrow(s.escrow_address);
    const fee = await this.solana.priorityFee([s.escrow_address], NETWORK.priorityFloor, NETWORK.priorityCap);
    const unsigned = W.build_refund_tx(NETWORK.programId, wallet.address, s.escrow_address, e.refund_token, e.rent_payer, await this.solana.blockhash(), BigInt(fee));
    const signed = await wallet.sign(hexToBytes(unsigned));
    await this.solana.simulate(signed);
    await this.solana.send(signed);
    this.set(s, { state: 'refunding', status: 'Refund sent.' });
  }

  // ---- L to S refund ----

  async tickRefund(s) {
    if (s.refund_txid) {
      const status = await this.esplora[0].status(s.refund_txid).catch(() => null);
      if (status?.confirmed) s.state = 'refunded';
      else if (s.refund_tx) await this.esplora[0].broadcast(s.refund_tx).catch(() => {});
    }
  }

  // Any coin at the address can come back, whatever it holds. It needs a small blinded L-BTC coin at the fee address.
  async refundOnLiquid(s, toAddress) {
    const coins = await this.esplora[0].readCoins(s.info.plain_address, s.info.script, s.secrets.blinding_key);
    if (!coins.length) throw new Error('There is no coin at the swap address to refund.');
    const coin = coins[0];
    const fees = await this.esplora[0].readCoins(s.info.plain_fee_address, s.info.fee_script, s.secrets.blinding_key);
    const fee = fees.find((c) => c.is_lbtc && c.confirmations >= 1 && BigInt(c.amount) >= BigInt(NETWORK.refundFeeCoinSats));
    if (!fee) throw new Error(`Send at least ${NETWORK.refundFeeCoinSats} sats of L-BTC to the fee address first, and wait for a confirmation.`);
    const hex = W.build_refund(JSON.stringify({
      swap: swapArgs(s), taker_sk: s.secrets.taker_sk, coin_tx: coin.hex, coin_vout: coin.vout,
      fee_tx: await this.esplora[0].txHex(fee.txid), fee_vout: fee.vout, to_address: toAddress, genesis: NETWORK.genesis,
    }));
    const txid = W.tx_id(hex);
    // Sent earlier with the reply lost: the transaction is out there, so carry on and watch it.
    await this.esplora[0].broadcast(hex).catch((e) => { if (!ALREADY_SENT.test(e.message)) throw e; });
    this.set(s, { refund_tx: hex, refund_txid: txid, state: 'refunding', status: 'Refund sent, waiting for a confirmation.' });
  }

  // ---- Messages and ticks ----

  async onMessage(s, got) {
    if (got.sender !== s.maker || got.id !== s.id) return;
    const { type, body } = got.body;
    if (body.lock_point && body.lock_point !== s.secrets.lock_point) return;
    if (type === 'claim-offer' && s.direction === 'L2S') {
      this.pendingOffers.set(s.id, body);
      await this.answerPendingOffer(s);
    } else if (type === 'session-nonce' && s.direction === 'S2L') {
      this.onSessionNonce(s, body.nonce);
    } else if (type === 'maker-share' && s.direction === 'S2L') {
      await this.onMakerShare(s, body.share);
    }
  }

  async tick(s) {
    if (FINISHED.includes(s.state) || s.state === 'recovery') return;
    const tip = await this.esplora[0].tip();
    if (s.direction === 'L2S') await this.tickL2S(s, tip);
    else await this.tickS2L(s, tip);
  }
}
