//! Selecting mainnet, in a process of its own, since the choice is made once per process.
use liq2sol_chain::liquid::{self, Net, LIQUID_MAINNET_GENESIS, LIQUID_TESTNET_GENESIS, MAINNET_MAX_PER_SWAP, MAINNET_USDT, SOLANA_MAINNET_GENESIS};
use liq2sol_chain::sol;

#[test]
fn mainnet_switches_every_id_and_then_stays() {
    assert_eq!(sol::enter("mainnet", false, &[MAINNET_MAX_PER_SWAP], LIQUID_MAINNET_GENESIS, SOLANA_MAINNET_GENESIS), Ok(Net::Mainnet));
    assert_eq!(sol::program_id().to_string(), "DsgtATJzzejhSZweUsna77kU5vYFqrSojoEYUNmufi6b");
    assert_eq!(sol::usdt_mint(), sol::MAINNET_MINT);
    assert_eq!(liquid::usdt().to_string(), MAINNET_USDT);
    assert_eq!(liquid::network().params.blech_hrp.to_string(), "lq");
    // An escrow's address is derived from the mainnet program now.
    let pda = sol::escrow_pda(&sol::MAINNET_MINT, &[7; 64]);
    assert_ne!(pda, solana_escrow::ID);
    // Once chosen, nothing in the process can go back to test networks.
    let back = sol::select(Net::Testnet, LIQUID_TESTNET_GENESIS, "devnet").unwrap_err();
    assert!(back.contains("already chose"), "{back}");
    assert_eq!(sol::program_id().to_string(), "DsgtATJzzejhSZweUsna77kU5vYFqrSojoEYUNmufi6b");
}
