// Nostr relays over plain WebSockets. Encryption and checks are in the WASM. A relay can only delay or drop messages.
export class Relays {
  constructor(urls, onEvent) {
    this.onEvent = onEvent;
    this.subs = new Map();
    this.seen = new Set();
    this.sockets = new Map();
    this.closed = false;
    for (const url of urls) this.connect(url);
  }

  connect(url) {
    if (this.closed) return;
    let ws;
    try {
      ws = new WebSocket(url);
    } catch {
      return;
    }
    this.sockets.set(url, ws);
    ws.onopen = () => {
      for (const [id, filter] of this.subs) ws.send(JSON.stringify(['REQ', id, filter]));
    };
    ws.onmessage = ({ data }) => {
      let message;
      try {
        message = JSON.parse(data);
      } catch {
        return;
      }
      if (message[0] !== 'EVENT' || !message[2]?.id || this.seen.has(message[2].id)) return;
      this.seen.add(message[2].id);
      if (this.seen.size > 20000) this.seen.clear();
      this.onEvent(message[2]);
    };
    ws.onclose = () => setTimeout(() => this.connect(url), 5000);
  }

  subscribe(id, filter) {
    this.subs.set(id, filter);
    for (const ws of this.sockets.values()) {
      if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(['REQ', id, filter]));
    }
  }

  publish(event) {
    let sent = 0;
    for (const ws of this.sockets.values()) {
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify(['EVENT', event]));
        sent += 1;
      }
    }
    if (!sent) throw new Error('No Nostr relay is connected. Check your connection, or the relays in Settings.');
  }

  connected() {
    return [...this.sockets.values()].filter((ws) => ws.readyState === WebSocket.OPEN).length;
  }

  close() {
    this.closed = true;
    for (const ws of this.sockets.values()) ws.close();
  }
}
