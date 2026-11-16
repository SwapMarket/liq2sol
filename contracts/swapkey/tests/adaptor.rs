//! The adapter-signature flow, checked against an independent verifier: libsecp256k1's own BIP340 verification, through
//! rust-secp256k1, and the taproot output key as rust-elements derives it. `musig2`'s adaptor code has no outside audit, so
//! nothing here trusts its verdict on its own output. Both parities of the adapted nonce must come up, since each takes a
//! different branch when signing, adapting and extracting.

use liq2sol_swapkey::{
    combine, complete, extract_secret, point_of, verify_share, Adaptor, Session, SwapOutput, SwapParams,
};
use rand::RngCore;
use simplicityhl::elements::secp256k1_zkp::{schnorr::Signature, Message, PublicKey, Secp256k1, SecretKey, XOnlyPublicKey};

fn random32() -> [u8; 32] {
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b
}

struct Party {
    secret: [u8; 32],
    public: [u8; 33],
}

fn party() -> Party {
    let secp = Secp256k1::new();
    loop {
        let secret = random32();
        if let Ok(sk) = SecretKey::from_slice(&secret) {
            return Party { secret, public: PublicKey::from_secret_key(&secp, &sk).serialize() };
        }
    }
}

fn output(maker: &Party, taker: &Party) -> SwapOutput {
    SwapOutput::new(&SwapParams { maker_key: maker.public, taker_key: taker.public, refund_pk: [7; 32], lock_height: 100 })
        .unwrap()
}

/// The output key, read from the script the chain will check signatures against.
fn output_key(output: &SwapOutput) -> XOnlyPublicKey {
    let script = output.script_pubkey();
    let bytes = script.as_bytes();
    assert_eq!((bytes.len(), bytes[0], bytes[1]), (34, 0x51, 32), "a taproot output");
    XOnlyPublicKey::from_slice(&bytes[2..]).unwrap()
}

/// What a Liquid node does with a key-path signature.
fn chain_accepts(key: &XOnlyPublicKey, signature: &[u8], message: &[u8; 32]) -> bool {
    let Ok(sig) = Signature::from_slice(signature) else { return false };
    Secp256k1::verification_only().verify_schnorr(&sig, &Message::from_digest_slice(message).unwrap(), key).is_ok()
}

/// Both parties sign `message` under `point`, each checking the other's share, as the swap does.
fn adapter(maker: &Party, taker: &Party, out: &SwapOutput, point: &[u8; 33], message: &[u8; 32]) -> Adaptor {
    let ctx = out.key_ctx();
    let m = Session::start(&maker.secret, ctx, message).unwrap();
    let t = Session::start(&taker.secret, ctx, message).unwrap();
    let (m_nonce, t_nonce) = (m.public.clone(), t.public.clone());
    let (m_share, agg) = m.sign(&maker.secret, ctx, &t_nonce, point, message).unwrap();
    let (t_share, agg_t) = t.sign(&taker.secret, ctx, &m_nonce, point, message).unwrap();
    assert_eq!(agg, agg_t, "both derive the same aggregate nonce");
    verify_share(ctx, t_share, &agg, &taker.public, &t_nonce, point, message).unwrap();
    verify_share(ctx, m_share, &agg, &maker.public, &m_nonce, point, message).unwrap();
    combine(ctx, &agg, [m_share, t_share], point, message).unwrap()
}

#[test]
fn a_completed_adapter_signature_is_valid_on_chain_and_gives_up_exactly_the_secret() {
    let (mut even, mut odd) = (0, 0);
    for _ in 0..24 {
        let (maker, taker) = (party(), party());
        let out = output(&maker, &taker);
        let key = output_key(&out);
        let (secret, message) = (party().secret, random32());
        let point = point_of(&secret).unwrap();
        let sig = adapter(&maker, &taker, &out, &point, &message);

        // Before the secret, there is nothing a node would accept: neither the stored value read as a signature, nor a
        // completion with another secret.
        let stored = liq2sol_swapkey::adaptor_bytes(&sig);
        assert!(!chain_accepts(&key, &stored[stored.len() - 64..], &message));
        let wrong = complete(&sig, &party().secret).unwrap();
        assert!(!chain_accepts(&key, &wrong, &message), "another secret must not complete it");

        let finished = complete(&sig, &secret).unwrap();
        assert!(chain_accepts(&key, &finished, &message), "libsecp256k1 accepts the completed signature");
        assert!(!chain_accepts(&key, &finished, &random32()), "and only for the agreed message");
        assert_eq!(extract_secret(&sig, &finished, &point), Some(secret), "publishing it gives the other side the secret");

        // The published nonce is the adapted one with even y. Its relation to the stored nonce differs by parity.
        let stored_nonce = &stored[..33];
        if finished[..32] == stored_nonce[1..] {
            unreachable!("the adapted nonce can't equal the unadapted one");
        }
        let adapted_even = {
            // Recompute R + T and read its parity, with libsecp256k1's point arithmetic.
            let r = PublicKey::from_slice(stored_nonce).unwrap();
            let t = PublicKey::from_slice(&point).unwrap();
            let sum = r.combine(&t).unwrap().serialize();
            assert_eq!(sum[1..], finished[..32], "the published nonce is R + T");
            sum[0] == 2
        };
        if adapted_even { even += 1 } else { odd += 1 }
    }
    assert!(even > 0 && odd > 0, "both parities were exercised: {even} even, {odd} odd");
}

#[test]
fn a_share_is_refused_for_any_other_message_point_key_or_nonce() {
    let (maker, taker, stranger) = (party(), party(), party());
    let out = output(&maker, &taker);
    let ctx = out.key_ctx();
    let (secret, message) = (party().secret, random32());
    let point = point_of(&secret).unwrap();
    let other_point = point_of(&party().secret).unwrap();

    let m = Session::start(&maker.secret, ctx, &message).unwrap();
    let t = Session::start(&taker.secret, ctx, &message).unwrap();
    let (m_nonce, t_nonce) = (m.public.clone(), t.public.clone());
    let (m_share, agg) = m.sign(&maker.secret, ctx, &t_nonce, &point, &message).unwrap();
    let (t_share, _) = t.sign(&taker.secret, ctx, &m_nonce, &point, &message).unwrap();

    assert!(verify_share(ctx, t_share, &agg, &taker.public, &t_nonce, &point, &message).is_ok());
    assert!(verify_share(ctx, t_share, &agg, &taker.public, &t_nonce, &point, &random32()).is_err(), "another message");
    assert!(verify_share(ctx, t_share, &agg, &taker.public, &t_nonce, &other_point, &message).is_err(), "another point");
    assert!(verify_share(ctx, t_share, &agg, &maker.public, &t_nonce, &point, &message).is_err(), "the other key");
    assert!(verify_share(ctx, t_share, &agg, &stranger.public, &t_nonce, &point, &message).is_err(), "a key outside the swap");
    assert!(verify_share(ctx, t_share, &agg, &taker.public, &m_nonce, &point, &message).is_err(), "the other nonce");
    assert!(verify_share(ctx, m_share, &agg, &taker.public, &t_nonce, &point, &message).is_err(), "our own share back");

    // Shares that don't belong together never combine into something to rely on.
    assert!(combine(ctx, &agg, [m_share, m_share], &point, &message).is_err());
    assert!(combine(ctx, &agg, [m_share, t_share], &other_point, &message).is_err());
    assert!(combine(ctx, &agg, [m_share, t_share], &point, &random32()).is_err());
    assert!(combine(ctx, &agg, [m_share, t_share], &point, &message).is_ok());
}

#[test]
fn a_signature_from_elsewhere_gives_up_nothing() {
    let (maker, taker) = (party(), party());
    let out = output(&maker, &taker);
    let (secret, message) = (party().secret, random32());
    let point = point_of(&secret).unwrap();
    let sig = adapter(&maker, &taker, &out, &point, &message);
    let finished = complete(&sig, &secret).unwrap();

    // A second session on the same swap has other nonces, so its published signature says nothing about this one.
    let other = adapter(&maker, &taker, &out, &point, &message);
    assert_eq!(extract_secret(&other, &finished, &point), None);
    // The right signature read against the wrong point is refused, not misread.
    assert_eq!(extract_secret(&sig, &finished, &point_of(&party().secret).unwrap()), None);
    let mut bent = finished;
    bent[40] ^= 1;
    assert_eq!(extract_secret(&sig, &bent, &point), None);
    assert_eq!(extract_secret(&sig, &finished, &point), Some(secret));
}

#[test]
fn a_stranger_cannot_sign_into_the_swap() {
    let (maker, taker, stranger) = (party(), party(), party());
    let out = output(&maker, &taker);
    let ctx = out.key_ctx();
    let message = random32();
    // A key outside the aggregate can't even open a session that signs.
    let s = Session::start(&stranger.secret, ctx, &message).unwrap();
    let t = Session::start(&taker.secret, ctx, &message).unwrap();
    let point = point_of(&party().secret).unwrap();
    assert!(s.sign(&stranger.secret, ctx, &t.public, &point, &message).is_err());
    // And a session's nonce is bound to its own key.
    let m = Session::start(&maker.secret, ctx, &message).unwrap();
    assert!(m.sign(&taker.secret, ctx, &t.public, &point, &message).is_err());
}
