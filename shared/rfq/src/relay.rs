//! A small relay client: one connection, publish and subscribe, nothing cached. Everything a relay sends is untrusted,
//! so oversized frames are refused and each event is verified again by whoever uses it.

use std::net::TcpStream;
use std::time::Duration;

use nostr::prelude::{Event, Kind, PublicKey};
use serde_json::{json, Value};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message, WebSocket};

use crate::messages::MAX_EVENT_BYTES;

pub struct Relay {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayEvent {
    Event(Box<Event>),
    /// The relay's answer to a publish: event id, accepted, and its message.
    Ok(String, bool, String),
    EndOfStored,
    Notice(String),
}

impl Relay {
    pub fn connect(url: &str, read_timeout: Duration) -> Result<Self, String> {
        // Harmless if a provider is already installed.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (mut socket, _) = connect(url).map_err(|e| format!("{url}: {e}"))?;
        let cfg = WebSocketConfig {
            max_message_size: Some(MAX_EVENT_BYTES * 2),
            max_frame_size: Some(MAX_EVENT_BYTES * 2),
            ..WebSocketConfig::default()
        };
        socket.set_config(|c| *c = cfg);
        let stream = match socket.get_mut() {
            MaybeTlsStream::Plain(s) => s,
            MaybeTlsStream::Rustls(s) => s.get_mut(),
            other => return Err(format!("unexpected stream type: {other:?}")),
        };
        stream.set_read_timeout(Some(read_timeout)).map_err(|e| e.to_string())?;
        Ok(Self { socket, url: url.to_string() })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn publish(&mut self, event: &Event) -> Result<(), String> {
        let text = format!("[\"EVENT\",{}]", event.as_json());
        self.socket.send(Message::Text(text)).map_err(|e| e.to_string())
    }

    /// Everything addressed to `to`, and optionally any events of these kinds, from `since` onward.
    pub fn subscribe(&mut self, id: &str, kinds: &[Kind], to: Option<&PublicKey>, since: u64) -> Result<(), String> {
        let kinds: Vec<u16> = kinds.iter().map(|k| k.as_u16()).collect();
        let mut filter = json!({ "kinds": kinds, "since": since });
        if let Some(pk) = to {
            filter["#p"] = json!([pk.to_hex()]);
        }
        let text = json!(["REQ", id, filter]).to_string();
        self.socket.send(Message::Text(text)).map_err(|e| e.to_string())
    }

    /// The next message, or `None` on a read timeout. A connection error is `Err`, and the caller reconnects.
    pub fn next_event(&mut self) -> Result<Option<RelayEvent>, String> {
        loop {
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    if let Some(event) = parse(&text) {
                        return Ok(Some(event));
                    }
                }
                Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_)) => {}
                Ok(Message::Close(_)) => return Err("the relay closed the connection".into()),
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

fn parse(text: &str) -> Option<RelayEvent> {
    let value: Value = serde_json::from_str(text).ok()?;
    let items = value.as_array()?;
    match items.first()?.as_str()? {
        "EVENT" => Some(RelayEvent::Event(Box::new(Event::from_json(items.get(2)?.to_string()).ok()?))),
        "OK" => Some(RelayEvent::Ok(
            items.get(1)?.as_str()?.to_string(),
            items.get(2)?.as_bool()?,
            items.get(3).and_then(Value::as_str).unwrap_or("").to_string(),
        )),
        "EOSE" => Some(RelayEvent::EndOfStored),
        "NOTICE" => Some(RelayEvent::Notice(items.get(1)?.as_str()?.to_string())),
        _ => None,
    }
}
