//! Choosing a network: what the operator says must match what both chains say, in both directions.
use std::str::FromStr;

use liq2sol_chain::liquid::{
    check_genesis, Net, NetworkConfig, LIQUID_MAINNET_GENESIS, LIQUID_TESTNET_GENESIS, MAINNET_LBTC, MAINNET_MAX_PER_SWAP,
    MAINNET_USDT, SOLANA_MAINNET_GENESIS,
};
use liq2sol_chain::sol;
use simplicityhl::elements::{AddressParams, AssetId};

const DEVNET: &str = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG";

#[test]
fn a_network_must_match_both_chains() {
    assert!(check_genesis(Net::Testnet, LIQUID_TESTNET_GENESIS, DEVNET).is_ok());
    assert!(check_genesis(Net::Testnet, "a regtest genesis", "a local validator").is_ok());
    assert!(check_genesis(Net::Mainnet, LIQUID_MAINNET_GENESIS, SOLANA_MAINNET_GENESIS).is_ok());
    // Real funds under test rules: either chain on mainnet is enough to stop.
    assert!(check_genesis(Net::Testnet, LIQUID_MAINNET_GENESIS, DEVNET).is_err());
    assert!(check_genesis(Net::Testnet, LIQUID_TESTNET_GENESIS, SOLANA_MAINNET_GENESIS).is_err());
    assert!(check_genesis(Net::Testnet, LIQUID_MAINNET_GENESIS, SOLANA_MAINNET_GENESIS).is_err());
    // A mixed pair could never settle: one leg would be worthless.
    assert!(check_genesis(Net::Mainnet, LIQUID_MAINNET_GENESIS, DEVNET).is_err());
    assert!(check_genesis(Net::Mainnet, LIQUID_TESTNET_GENESIS, SOLANA_MAINNET_GENESIS).is_err());
    assert!(check_genesis(Net::Mainnet, LIQUID_TESTNET_GENESIS, DEVNET).is_err());
}

#[test]
fn network_names_are_exact() {
    assert_eq!(Net::parse("testnet"), Ok(Net::Testnet));
    assert_eq!(Net::parse("mainnet"), Ok(Net::Mainnet));
    assert_eq!(Net::default(), Net::Testnet, "nothing reaches mainnet by leaving a setting out");
    for bad in ["", "Mainnet", "main", "mainnet ", "liquidv1"] {
        assert!(Net::parse(bad).is_err(), "{bad:?}");
    }
    assert_eq!(Net::parse(Net::Mainnet.name()), Ok(Net::Mainnet));
}

#[test]
fn mainnet_constants_are_the_real_ones() {
    let main = NetworkConfig::mainnet();
    assert_eq!(main.lbtc, AssetId::LIQUID_BTC, "the elements crate's own constant");
    assert_eq!(MAINNET_LBTC, AssetId::LIQUID_BTC.to_string());
    assert_eq!(main.usdt, AssetId::from_str(MAINNET_USDT).unwrap());
    assert_ne!(main.usdt, main.lbtc);
    // Mainnet's address prefixes: "lq1" for confidential addresses and "ex1" for plain ones, not testnet's "tlq1" and "tex1".
    assert_eq!(*main.params, AddressParams::LIQUID);
    assert_eq!((main.params.blech_hrp.to_string(), main.params.bech_hrp.to_string()), ("lq".to_string(), "ex".to_string()));
    assert_eq!(MAINNET_MAX_PER_SWAP, 100_000_000, "100 USDT, in units of 10^-6");
    assert_eq!(sol::MAINNET_MINT.to_string(), "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB");
}

#[test]
fn mainnet_cannot_be_selected_before_the_program_is_there() {
    if sol::MAINNET_PROGRAM.is_none() {
        let refused = sol::select(Net::Mainnet, LIQUID_MAINNET_GENESIS, SOLANA_MAINNET_GENESIS).unwrap_err();
        assert!(refused.contains("isn't on mainnet yet"), "{refused}");
        // And nothing was switched by trying.
        assert_eq!(sol::program_id(), solana_escrow::id());
        assert_eq!(sol::usdt_mint(), solana_escrow::USDT_MINT);
    }
    assert!(sol::select(Net::Testnet, LIQUID_TESTNET_GENESIS, DEVNET).is_ok());
    assert!(sol::select(Net::Testnet, LIQUID_MAINNET_GENESIS, DEVNET).is_err());
}

#[test]
fn mainnet_refuses_test_settings_and_limits_over_the_cap() {
    let (l, s) = (LIQUID_MAINNET_GENESIS, SOLANA_MAINNET_GENESIS);
    let over = sol::enter("mainnet", false, &[50_000_000, MAINNET_MAX_PER_SWAP + 1], l, s).unwrap_err();
    assert!(over.contains("above mainnet's cap"), "{over}");
    assert!(sol::enter("mainnet", true, &[1], l, s).unwrap_err().contains("regtest_assets"));
    assert!(sol::enter("Mainnet", false, &[1], l, s).unwrap_err().contains("unknown network"));
    // At the cap exactly, the only thing left in the way is the program not being deployed.
    if sol::MAINNET_PROGRAM.is_none() {
        assert!(sol::enter("mainnet", false, &[MAINNET_MAX_PER_SWAP], l, s).unwrap_err().contains("isn't on mainnet yet"));
    }
    // Testnet keeps whatever limits its operator sets.
    assert_eq!(sol::enter("testnet", true, &[u64::MAX], LIQUID_TESTNET_GENESIS, DEVNET), Ok(Net::Testnet));
    assert!(sol::enter("testnet", false, &[1], l, DEVNET).is_err());
}
