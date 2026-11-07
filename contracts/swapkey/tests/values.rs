//! The link between the two chains' views of the secret, against vectors computed separately from the curve's definition. The
//! same vectors are what the Solana program's tests open through the real syscall.

use liq2sol_swapkey::*;

fn unhex<const N: usize>(s: &str) -> [u8; N] {
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap();
    }
    out
}

const VECTORS: [(&str, &str, &str); 2] = [
    ("0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20", "84bf7562262bbd6940085748f3be6afa52ae317155181ece31b66351ccffa4b08cc43d63b2859d469fee15f31c9edb5324266e6fd0407e87382d60fc4511acd8", "4ce28f147e857ec4476456049bb053c134968e79e649f3a90e935310a8533273"),
    ("fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210", "88e2ddeb04657dbd0edadf9c1f98da3b3895faa1f00527934dd35d17542ffe9b1e7640d7737e24e36d208effb77e86affe670a9a497aa7fb52bf4e687a17fff4", "4c0b29f33d12e45ab6a60bf684540b7647c891483c9464672a83684d8fa02caf"),
];

#[test]
fn values_match_the_independent_vectors() {
    for (t, point, s) in VECTORS {
        let (t, point, s): ([u8; 32], [u8; 64], [u8; 32]) = (unhex(t), unhex(point), unhex(s));
        assert_eq!(solana_point(&t).unwrap(), point);
        assert_eq!(solana_value(&t).unwrap(), s);
        assert_eq!(secret_from_solana(&s).unwrap(), t, "the counterparty recovers the secret");
        let compressed = point_of(&t).unwrap();
        assert_eq!(compressed[1..], point[..32], "the adapter point and the Solana point are the same point");
        assert_eq!(compressed[0], 2 + (point[63] & 1), "with the right sign");
    }
}

#[test]
fn bad_values_are_refused() {
    assert!(secret_from_solana(&[0; 32]).is_err(), "zero");
    assert!(secret_from_solana(&[0xff; 32]).is_err(), "out of range");
    assert!(solana_value(&[0; 32]).is_err());
    assert!(point_of(&[0xff; 32]).is_err());
}

/// The stored inverse is checked against the generator, so a wrong constant can't slip through.
#[test]
fn gx_inverse_is_right() {
    for t in [[1u8; 32], [7; 32], [0x42; 32]] {
        assert_eq!(secret_from_solana(&solana_value(&t).unwrap()).unwrap(), t);
    }
    let mut one = [0u8; 32];
    one[31] = 1;
    let gx = solana_value(&one).unwrap();
    assert_eq!(hex_string(&gx), "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798", "s = Gx when t = 1");
    assert_eq!(secret_from_solana(&gx).unwrap(), one, "Gx / Gx = 1");
}

fn hex_string(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
