//! Prints a wrapped message for checking against other Nostr libraries: recipient secret, then the event.
use liq2sol_rfq::messages::*;
use liq2sol_rfq::wrap::wrap;
use nostr::prelude::*;

fn main() {
    let (sender, recipient) = (Keys::generate(), Keys::generate());
    let envelope = Envelope {
        v: VERSION,
        id: hex(&[7; 32]),
        network: "testnet".into(),
        sent_at: 1_793_888_350,
        body: Body::RfqRequest(Request {
            direction: Direction::L2S,
            amount_side: AmountSide::Send,
            amount: Units(100),
        }),
    };
    let (event, rumor_id) = wrap(&sender, &recipient.public_key(), &envelope, 1_793_888_350).unwrap();
    println!("{}", recipient.secret_key().to_secret_hex());
    println!("{}", sender.public_key().to_hex());
    println!("{rumor_id}");
    println!("{}", event.as_json());
}
