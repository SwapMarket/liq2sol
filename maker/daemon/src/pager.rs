//! Pages reach the operator as NIP-17 private messages, which any Nostr app shows. The page file stays the record: a message
//! that no relay takes is lost, and only the file has it.

use std::sync::mpsc::{channel, Sender};
use std::time::{Duration, Instant};

use liq2sol_rfq::relay::{Relay, RelayEvent};
use liq2sol_rfq::wrap::private_message;
use nostr::prelude::{Keys, PublicKey};
use serde::{Deserialize, Serialize};

const ATTEMPTS: u32 = 3;
const RETRY_AFTER: Duration = Duration::from_secs(30);
const ANSWER_WITHIN: Duration = Duration::from_secs(10);

/// Who gets the pages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageTo {
    /// The operator's key, as `npub1…` or hex.
    pub nostr: String,
    /// The relays the operator's app reads private messages from: its DM relay list (NIP-17, kind 10050).
    pub relays: Vec<String>,
}

impl PageTo {
    pub fn check(&self) -> Result<PublicKey, String> {
        if self.relays.is_empty() {
            return Err("page_to needs at least one relay".into());
        }
        PublicKey::parse(&self.nostr).map_err(|e| format!("page_to.nostr: {e}"))
    }
}

/// Publish one event and wait for the relay to accept it.
fn deliver(url: &str, event: &nostr::prelude::Event) -> Result<(), String> {
    let mut relay = Relay::connect(url, Duration::from_secs(1))?;
    relay.publish(event)?;
    let start = Instant::now();
    while start.elapsed() < ANSWER_WITHIN {
        if let Some(RelayEvent::Ok(id, accepted, message)) = relay.next_event()? {
            if id == event.id.to_hex() {
                return if accepted { Ok(()) } else { Err(format!("refused: {message}")) };
            }
        }
    }
    Err("no answer".into())
}

/// Send `text` to every relay, trying each a few times. Returns each relay's outcome.
pub fn send(from: &Keys, page_to: &PageTo, text: &str) -> Vec<(String, Result<(), String>)> {
    let to = match page_to.check() {
        Ok(to) => to,
        Err(e) => return vec![(String::new(), Err(e))],
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let event = match private_message(from, &to, text, now) {
        Ok(event) => event,
        Err(e) => return vec![(String::new(), Err(format!("{e:?}")))],
    };
    let mut outcomes = vec![];
    for url in &page_to.relays {
        let mut result = Err(String::new());
        for attempt in 1..=ATTEMPTS {
            result = deliver(url, &event);
            if result.is_ok() || attempt == ATTEMPTS {
                break;
            }
            std::thread::sleep(RETRY_AFTER);
        }
        outcomes.push((url.clone(), result));
    }
    outcomes
}

/// Pages go out from a thread of their own, so a slow relay never holds up a tick.
pub fn start(from: Keys, page_to: PageTo) -> Sender<String> {
    let (send_page, pages) = channel::<String>();
    std::thread::spawn(move || {
        for text in pages {
            for (url, outcome) in send(&from, &page_to, &text) {
                if let Err(e) = outcome {
                    eprintln!("could not send a page to {url}: {e}");
                }
            }
        }
    });
    send_page
}
