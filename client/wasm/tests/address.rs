//! An address to be paid at must be for this network, which is testnet in these tests, and confidential.
use liq2sol_client_wasm::address_problem;
use simplicityhl::elements::bitcoin::PublicKey;
use simplicityhl::elements::secp256k1_zkp::{PublicKey as Key, Secp256k1, SecretKey};
use simplicityhl::elements::{Address, AddressParams};

fn address(params: &'static AddressParams, blinded: bool) -> String {
    let secp = Secp256k1::new();
    let key = Key::from_secret_key(&secp, &SecretKey::from_slice(&[7; 32]).unwrap());
    let blinder = Key::from_secret_key(&secp, &SecretKey::from_slice(&[9; 32]).unwrap());
    Address::p2wpkh(&PublicKey::new(key), blinded.then_some(blinder), params).to_string()
}

#[test]
fn only_a_confidential_address_for_this_network_is_paid() {
    let ok = address(&AddressParams::LIQUID_TESTNET, true);
    assert!(ok.starts_with("tlq1"), "{ok}");
    assert_eq!(address_problem(&ok), "");
    assert_eq!(address_problem(&format!("  {ok}\n")), "", "spaces around a pasted address don't matter");
    let mainnet = address(&AddressParams::LIQUID, true);
    assert!(mainnet.starts_with("lq1"), "{mainnet}");
    assert_eq!(address_problem(&mainnet), "This address is for another network.");
    assert_eq!(address_problem(&address(&AddressParams::LIQUID_TESTNET, false)), "Use a confidential address, so the amount stays hidden.");
    assert_eq!(address_problem("tlq1notanaddress"), "This isn't a Liquid address.");
    assert_eq!(address_problem("bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh"), "This isn't a Liquid address.");
}
