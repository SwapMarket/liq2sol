//! Low-balance alarms: USDT on either chain below the largest trade we offer, or gas on either chain for fewer than a
//! few swaps. A maker that runs dry only stops quoting, which nobody notices from outside.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BalanceAlarms {
    /// Page when gas would last fewer swaps than this.
    pub gas_swaps: u64,
    /// SOL one swap ties up: an escrow's rent, which comes back on closing, and fees. About 2.76M rent and 10k fees in tests.
    pub lamports_per_swap: u64,
    /// L-BTC one swap spends: the lock, the taker's fee coin, the claim or refund, and a share of fee-coin top-ups.
    pub lbtc_sats_per_swap: u64,
    pub every_secs: u64,
    /// While an alarm lasts, page again this often.
    pub repeat_secs: u64,
}

impl Default for BalanceAlarms {
    fn default() -> Self {
        Self { gas_swaps: 10, lamports_per_swap: 3_000_000, lbtc_sats_per_swap: 200, every_secs: 600, repeat_secs: 6 * 3600 }
    }
}

/// What the signer holds, in each chain's base units: Liquid has 8 decimals, Solana's USDT 6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holdings {
    pub liquid_usdt: u64,
    pub liquid_lbtc_sats: u64,
    pub solana_usdt: u64,
    pub sol_lamports: u64,
}

fn usdt(units: u64) -> String {
    format!("{:.2}", units as f64 / 1e6)
}

/// Each alarm that holds, by name, with what to tell the operator. `max_trade` is in Solana's USDT units.
pub fn low(h: &Holdings, max_trade: u64, a: &BalanceAlarms) -> Vec<(&'static str, String)> {
    let mut out = vec![];
    let liquid_usdt = h.liquid_usdt / 100;
    if h.solana_usdt < max_trade {
        let what = format!("SOL-USDT is {}, below the largest trade, {}", usdt(h.solana_usdt), usdt(max_trade));
        out.push(("solana_usdt", what));
    }
    if liquid_usdt < max_trade {
        let what = format!("L-USDt is {}, below the largest trade, {}", usdt(liquid_usdt), usdt(max_trade));
        out.push(("liquid_usdt", what));
    }
    let sol_needed = a.lamports_per_swap.saturating_mul(a.gas_swaps);
    if h.sol_lamports < sol_needed {
        let what = format!(
            "SOL is {:.4}, enough for fewer than {} swaps ({:.4})",
            h.sol_lamports as f64 / 1e9,
            a.gas_swaps,
            sol_needed as f64 / 1e9
        );
        out.push(("sol", what));
    }
    let lbtc_needed = a.lbtc_sats_per_swap.saturating_mul(a.gas_swaps);
    if h.liquid_lbtc_sats < lbtc_needed {
        let what = format!(
            "L-BTC is {} sats, enough for fewer than {} swaps ({} sats)",
            h.liquid_lbtc_sats, a.gas_swaps, lbtc_needed
        );
        out.push(("lbtc", what));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: Holdings =
        Holdings { liquid_usdt: 5_000_000_000, liquid_lbtc_sats: 2_000, solana_usdt: 50_000_000, sol_lamports: 30_000_000 };

    fn names(h: Holdings) -> Vec<&'static str> {
        low(&h, 50_000_000, &BalanceAlarms::default()).into_iter().map(|(n, _)| n).collect()
    }

    #[test]
    fn exactly_enough_is_no_alarm() {
        assert!(names(FULL).is_empty());
    }

    #[test]
    fn each_balance_alarms_on_its_own() {
        assert_eq!(names(Holdings { solana_usdt: 49_999_999, ..FULL }), ["solana_usdt"]);
        // Liquid's two extra decimals: 49.99999999 L-USDt is short of 50.
        assert_eq!(names(Holdings { liquid_usdt: 4_999_999_999, ..FULL }), ["liquid_usdt"]);
        assert_eq!(names(Holdings { sol_lamports: 29_999_999, ..FULL }), ["sol"]);
        assert_eq!(names(Holdings { liquid_lbtc_sats: 1_999, ..FULL }), ["lbtc"]);
        let empty = Holdings { liquid_usdt: 0, liquid_lbtc_sats: 0, solana_usdt: 0, sol_lamports: 0 };
        assert_eq!(names(empty), ["solana_usdt", "liquid_usdt", "sol", "lbtc"]);
    }

    #[test]
    fn the_message_says_how_much_and_against_what() {
        let alarms = low(&Holdings { solana_usdt: 12_340_000, ..FULL }, 50_000_000, &BalanceAlarms::default());
        assert_eq!(alarms[0].1, "SOL-USDT is 12.34, below the largest trade, 50.00");
    }

    #[test]
    fn config_fields_default_one_by_one() {
        let a: BalanceAlarms = serde_json::from_str(r#"{"gas_swaps": 3}"#).unwrap();
        assert_eq!(a, BalanceAlarms { gas_swaps: 3, ..BalanceAlarms::default() });
    }
}
