//! A local relay for tests. It stores and forwards like a real one, and can misbehave in the ways a relay can: it can
//! replay everything it has seen, or silently drop what it is given.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nostr::prelude::{Event, Kind};
use serde_json::Value;
use tungstenite::{accept, Message};

const MAX_EPHEMERAL_AGE_SECS: u64 = 10;

#[derive(Default)]
struct Shared {
    events: Vec<Event>,
    subs: Vec<(usize, String, Value)>,
    outbox: Vec<(usize, String)>,
}

pub struct MockRelay {
    addr: SocketAddr,
    shared: Arc<Mutex<Shared>>,
    drop_all: Arc<AtomicBool>,
}

fn matches(filter: &Value, event: &Event) -> bool {
    let kind_ok = filter["kinds"]
        .as_array()
        .is_none_or(|ks| ks.iter().any(|k| k.as_u64() == Some(u64::from(event.kind.as_u16()))));
    let tagged_ok = filter["#p"].as_array().is_none_or(|ps| {
        let tags = serde_json::to_value(&event.tags).unwrap_or_default();
        ps.iter().any(|p| tags.as_array().is_some_and(|ts| ts.iter().any(|t| t[0] == "p" && t[1] == *p)))
    });
    let since_ok = filter["since"].as_u64().is_none_or(|s| event.created_at.as_secs() >= s);
    kind_ok && tagged_ok && since_ok
}

impl MockRelay {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("address");
        let shared = Arc::new(Mutex::new(Shared::default()));
        let drop_all = Arc::new(AtomicBool::new(false));
        let (s, d) = (shared.clone(), drop_all.clone());
        std::thread::spawn(move || {
            for (conn, stream) in listener.incoming().flatten().enumerate() {
                let (s, d) = (s.clone(), d.clone());
                std::thread::spawn(move || serve(stream, conn, s, d));
            }
        });
        Self { addr, shared, drop_all }
    }

    pub fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// Send every stored event to every subscriber again, as a hostile relay could at any time.
    pub fn replay_everything(&self) {
        let mut g = self.shared.lock().expect("lock");
        let mut out = vec![];
        for event in &g.events {
            for (conn, sub, filter) in &g.subs {
                if matches(filter, event) {
                    out.push((*conn, format!("[\"EVENT\",\"{sub}\",{}]", event.as_json())));
                }
            }
        }
        g.outbox.extend(out);
    }

    /// Accept everything and deliver nothing.
    pub fn drop_all(&self, on: bool) {
        self.drop_all.store(on, Ordering::SeqCst);
    }

    pub fn stored(&self, kind: u16) -> Vec<Event> {
        self.shared.lock().expect("lock").events.iter().filter(|e| e.kind == Kind::from(kind)).cloned().collect()
    }

    /// Deliver an event of the test's choosing, as if a third party had published it.
    pub fn inject(&self, event: Event) {
        let mut g = self.shared.lock().expect("lock");
        deliver(&mut g, event);
    }
}

fn deliver(g: &mut Shared, event: Event) {
    let out: Vec<(usize, String)> = g
        .subs
        .iter()
        .filter(|(_, _, filter)| matches(filter, &event))
        .map(|(conn, sub, _)| (*conn, format!("[\"EVENT\",\"{sub}\",{}]", event.as_json())))
        .collect();
    g.outbox.extend(out);
    g.events.push(event);
}

fn serve(stream: TcpStream, conn: usize, shared: Arc<Mutex<Shared>>, drop_all: Arc<AtomicBool>) {
    let Ok(mut socket) = accept(stream) else { return };
    let _ = socket.get_mut().set_read_timeout(Some(Duration::from_millis(50)));
    loop {
        let pending: Vec<String> = {
            let mut g = shared.lock().expect("lock");
            let mine: Vec<String> = g.outbox.iter().filter(|(c, _)| *c == conn).map(|(_, t)| t.clone()).collect();
            g.outbox.retain(|(c, _)| *c != conn);
            mine
        };
        for text in pending {
            if socket.send(Message::Text(text)).is_err() {
                return;
            }
        }
        let text = match socket.read() {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) => return,
            Ok(_) => continue,
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(_) => return,
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
        match value[0].as_str() {
            Some("EVENT") => {
                let Ok(event) = Event::from_json(value[1].to_string()) else { continue };
                let id = event.id.to_hex();
                let mut g = shared.lock().expect("lock");
                // Real relays refuse stale ephemeral events: relay.coinos.pro takes 5 seconds, and not 30.
                let ephemeral = (20_000..30_000).contains(&event.kind.as_u16());
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                if ephemeral && now.saturating_sub(event.created_at.as_secs()) > MAX_EPHEMERAL_AGE_SECS {
                    g.outbox.push((conn, format!("[\"OK\",\"{id}\",false,\"invalid: ephemeral event expired\"]")));
                    continue;
                }
                g.outbox.push((conn, format!("[\"OK\",\"{id}\",true,\"\"]")));
                if !drop_all.load(Ordering::SeqCst) {
                    deliver(&mut g, event);
                }
            }
            Some("REQ") => {
                let sub = value[1].as_str().unwrap_or("").to_string();
                let mut g = shared.lock().expect("lock");
                g.subs.push((conn, sub.clone(), value[2].clone()));
                let stored: Vec<(usize, String)> = g
                    .events
                    .iter()
                    .filter(|e| matches(&value[2], e))
                    .map(|e| (conn, format!("[\"EVENT\",\"{sub}\",{}]", e.as_json())))
                    .collect();
                g.outbox.extend(stored);
                g.outbox.push((conn, format!("[\"EOSE\",\"{sub}\"]")));
            }
            _ => {}
        }
    }
}
