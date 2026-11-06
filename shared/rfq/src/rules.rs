//! What each side must check. Pure functions, so every rule can be tested without a relay.

use crate::messages::*;

/// How far a message's `sent_at` may be from our clock. NIP-59 randomizes the outer timestamps, so this is the only
/// freshness signal, and it is inside the encrypted payload.
pub const MAX_CLOCK_SKEW_SECS: u64 = 120;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    WrongVersion,
    WrongNetwork,
    BadId,
    Stale,
    UnexpectedMessage,
    UnknownNegotiation,
    QuoteExpired,
    /// The accept names a quote we didn't send, or the terms name an accept we didn't send.
    WrongReference,
    Malformed(&'static str),
    OutOfLimits,
    NotMultipleOf100,
    /// We wouldn't receive what was agreed, or would pay more.
    BadAmounts,
    BadLockHeight,
    WrongProgram,
    WrongAsset,
    WrongTemplate,
    DuplicateHash,
    RateLimited,
}

/// Checks every message needs, on either side.
pub fn check_envelope(e: &Envelope, network: &str, now: u64) -> Result<(), Refused> {
    if e.v != VERSION {
        return Err(Refused::WrongVersion);
    }
    if e.network != network {
        return Err(Refused::WrongNetwork);
    }
    if hex32(&e.id).is_none() {
        return Err(Refused::BadId);
    }
    if e.sent_at.abs_diff(now) > MAX_CLOCK_SKEW_SECS {
        return Err(Refused::Stale);
    }
    Ok(())
}

/// The maker's pricing and limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MakerRules {
    pub network: String,
    pub min_sol_units: u64,
    pub max_sol_units: u64,
    /// What the maker can pay right now, less what open swaps are counting on.
    pub available_l2s: u64,
    pub available_s2l: u64,
    pub l2s_bps: u32,
    pub s2l_bps: u32,
    pub quote_secs: u64,
    pub taker_lock_window: u32,
    pub l2s_lock_blocks: u32,
    pub s2l_lock_blocks: u32,
    pub min_deadline_slots: u64,
}

/// (send, receive) for a request, always rounding in the maker's favour. `None` if it can't be priced.
pub fn price(request: &Request, bps: u32) -> Option<(u64, u64)> {
    let amount = u128::from(request.amount.0);
    let keep = 10_000u128.checked_sub(u128::from(bps)).filter(|k| *k > 0)?;
    let (send, receive) = match request.amount_side {
        // Sending `amount`: the taker receives it less the spread, rounded down.
        AmountSide::Send => (amount, amount.checked_mul(keep)? / 10_000),
        // Receiving `amount`: the taker sends it plus the spread, rounded up.
        AmountSide::Receive => (amount.checked_mul(10_000)?.div_ceil(keep), amount),
    };
    if send == 0 || receive == 0 {
        return None;
    }
    Some((u64::try_from(send).ok()?, u64::try_from(receive).ok()?))
}

/// The maker's answer to a request.
pub fn make_quote(r: &MakerRules, request: &Request, now: u64) -> Result<Quote, Refused> {
    let bps = match request.direction {
        Direction::L2S => r.l2s_bps,
        Direction::S2L => r.s2l_bps,
    };
    let (send, receive) = price(request, bps).ok_or(Refused::OutOfLimits)?;
    if send.max(receive) > r.max_sol_units || send.min(receive) < r.min_sol_units {
        return Err(Refused::OutOfLimits);
    }
    if send.checked_mul(100).is_none() || receive.checked_mul(100).is_none() {
        return Err(Refused::OutOfLimits);
    }
    // The maker pays `receive`. A quote it can't fill would tie up the taker's funds until a refund.
    let available = match request.direction {
        Direction::L2S => r.available_l2s,
        Direction::S2L => r.available_s2l,
    };
    if receive > available {
        return Err(Refused::OutOfLimits);
    }
    Ok(Quote {
        send: Units(send),
        receive: Units(receive),
        quote_expiry: now + r.quote_secs,
        taker_lock_window: r.taker_lock_window,
    })
}

/// An accept the maker has checked, with its fields decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedAccept {
    pub lock_point: [u8; 33],
    pub taker_key: [u8; 33],
    pub blinding_key: [u8; 32],
    pub taker_solana: String,
}

/// Solana addresses are base58 of 32 bytes. Full validation is left to the Solana parser that uses them.
fn plausible_solana(s: &str) -> bool {
    (32..=44).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() && !b"0OIl".contains(&b))
}

/// The maker's check of an accept. It must also refuse any point it has ever seen, in its store.
pub fn check_accept(accept: &Accept, quote: &Quote, quote_rumor_id: &str, now: u64) -> Result<CheckedAccept, Refused> {
    if accept.quote_event != quote_rumor_id {
        return Err(Refused::WrongReference);
    }
    if now > quote.quote_expiry {
        return Err(Refused::QuoteExpired);
    }
    let lock_point = hex33(&accept.lock_point).ok_or(Refused::Malformed("lock_point"))?;
    let taker_key = hex33(&accept.taker_key).ok_or(Refused::Malformed("taker_key"))?;
    let blinding_key = hex32(&accept.blinding_key).ok_or(Refused::Malformed("blinding_key"))?;
    if !plausible_solana(&accept.taker_solana) {
        return Err(Refused::Malformed("taker_solana"));
    }
    Ok(CheckedAccept { lock_point, taker_key, blinding_key, taker_solana: accept.taker_solana.clone() })
}

/// What a taker holds to, pinned per network in the released code. Nothing here comes from the maker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakerRules {
    pub network: String,
    pub program_id: String,
    pub usdt_asset: String,
    pub template_version: u32,
    /// L→S: the taker's funds are locked until this far ahead. Near enough not to tie them up, far enough for the swap.
    pub l2s_min_blocks: u32,
    pub l2s_max_blocks: u32,
    /// S→L: how long the taker has to claim.
    pub s2l_min_blocks: u32,
    pub s2l_max_blocks: u32,
    /// The worst spread the taker accepts, in basis points.
    pub max_spread_bps: u32,
    pub max_deadline_slots: u64,
}

/// The taker's check of a quote against what it asked for.
pub fn check_quote(r: &TakerRules, request: &Request, quote: &Quote, now: u64) -> Result<(), Refused> {
    if now > quote.quote_expiry {
        return Err(Refused::QuoteExpired);
    }
    let (send, receive) = (quote.send.0, quote.receive.0);
    let asked = match request.amount_side {
        AmountSide::Send => send == request.amount.0,
        AmountSide::Receive => receive == request.amount.0,
    };
    if !asked || receive == 0 || receive > send {
        return Err(Refused::BadAmounts);
    }
    // Spread = (send - receive) / send, compared without division.
    let lost = u128::from(send - receive) * 10_000;
    if lost > u128::from(send) * u128::from(r.max_spread_bps) {
        return Err(Refused::BadAmounts);
    }
    Ok(())
}

/// E8: the taker's last check before funding. `tip` is from the taker's own chain view.
pub fn check_terms(
    r: &TakerRules,
    direction: Direction,
    quote: &Quote,
    accept_rumor_id: &str,
    terms: &MakerTerms,
    tip: u32,
) -> Result<(), Refused> {
    if terms.accept_event != accept_rumor_id {
        return Err(Refused::WrongReference);
    }
    if terms.program_id != r.program_id {
        return Err(Refused::WrongProgram);
    }
    if terms.usdt_asset != r.usdt_asset {
        return Err(Refused::WrongAsset);
    }
    if terms.template_version != r.template_version {
        return Err(Refused::WrongTemplate);
    }
    let (send, receive) = (quote.send.0, quote.receive.0);
    let (liquid, solana) = match direction {
        // The taker pays Liquid and receives Solana.
        Direction::L2S => (send.checked_mul(100), receive),
        // The taker pays Solana and receives Liquid.
        Direction::S2L => (receive.checked_mul(100), send),
    };
    if Some(terms.liquid_amount.0) != liquid || terms.solana_amount.0 != solana {
        return Err(Refused::BadAmounts);
    }
    if !terms.liquid_amount.0.is_multiple_of(100) {
        return Err(Refused::NotMultipleOf100);
    }
    let blocks = terms.lock_height.saturating_sub(tip);
    let (min, max) = match direction {
        Direction::L2S => (r.l2s_min_blocks, r.l2s_max_blocks),
        Direction::S2L => (r.s2l_min_blocks, r.s2l_max_blocks),
    };
    if blocks < min || blocks > max {
        return Err(Refused::BadLockHeight);
    }
    if terms.min_deadline_slots > r.max_deadline_slots {
        return Err(Refused::BadLockHeight);
    }
    if hex33(&terms.maker_key).is_none() || !plausible_solana(&terms.maker_solana) {
        return Err(Refused::Malformed("maker keys"));
    }
    if direction == Direction::L2S && !terms.maker_initiator.as_deref().is_some_and(plausible_solana) {
        return Err(Refused::Malformed("maker_initiator"));
    }
    Ok(())
}

/// Per-sender rate limiting. Sender keys are free, so this only raises the cost: the maker's caps are the real bound.
#[derive(Debug, Default)]
pub struct RateLimit {
    hits: std::collections::HashMap<String, Vec<u64>>,
}

impl RateLimit {
    /// Allow at most `max` per `window` seconds for this sender. Also caps how many senders are remembered.
    pub fn allow(&mut self, sender: &str, now: u64, window: u64, max: usize) -> bool {
        const MAX_SENDERS: usize = 10_000;
        if self.hits.len() >= MAX_SENDERS && !self.hits.contains_key(sender) {
            self.hits.retain(|_, times| times.iter().any(|t| now.saturating_sub(*t) < window));
            if self.hits.len() >= MAX_SENDERS {
                return false;
            }
        }
        let times = self.hits.entry(sender.to_string()).or_default();
        times.retain(|t| now.saturating_sub(*t) < window);
        if times.len() >= max {
            return false;
        }
        times.push(now);
        true
    }
}
