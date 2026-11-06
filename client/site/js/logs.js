// A log a user can hand to someone for support. It stays in this browser unless they copy or download it. People paste these into
// chats, so everything is scrubbed on the way in, and nothing here may ever hold a secret.
import { loadSettings, loadSwaps } from './store.js';

const KEY = 'liq2sol.logs';
const MAX_LINES = 1000;
const MAX_LINE = 500;

function secretsKnown() {
  const out = [];
  for (const s of Object.values(loadSwaps())) {
    for (const v of [s.secrets?.lock_secret, s.secrets?.taker_sk, s.secrets?.blinding_key, s.nostr?.secret]) if (v) out.push(v);
    // What a claim reveals is the secret in another form, and a finished claim's signature gives it up too.
    for (const v of [s.revealed, s.claim_tx, s.signed?.share, s.maker_share]) if (v) out.push(v);
  }
  const { heliusKey } = loadSettings();
  if (heliusKey) out.push(heliusKey);
  return out;
}

/** Remove what must never be in a log. Whole transactions go too: they are long, and a claim's signature is sensitive before it is published. */
export function scrub(text) {
  let out = String(text);
  for (const secret of secretsKnown()) out = out.split(secret).join('[secret]');
  return out
    .replace(/api-key=[^\s&"']+/g, 'api-key=[secret]')
    .replace(/[0-9a-fA-F]{200,}/g, '[long hex removed]')
    .slice(0, MAX_LINE);
}

function read() {
  try {
    return JSON.parse(localStorage.getItem(KEY)) ?? [];
  } catch {
    return [];
  }
}

let lines = read();

function write() {
  try {
    localStorage.setItem(KEY, JSON.stringify(lines));
  } catch {
    // A full disk or a private window shouldn't stop a swap for the sake of its log.
  }
}

export function log(level, ...parts) {
  const text = scrub(parts.map((p) => (p instanceof Error ? (p.stack ?? p.message) : typeof p === 'string' ? p : safeJson(p))).join(' '));
  const last = lines.at(-1);
  // A wallet's script can repeat one error hundreds of times.
  if (last && last.text === text && last.level === level) {
    last.count = (last.count ?? 1) + 1;
    last.last = Date.now();
  } else {
    lines.push({ at: Date.now(), level, text });
    if (lines.length > MAX_LINES) lines = lines.slice(-MAX_LINES);
  }
  write();
}

function safeJson(v) {
  try {
    return JSON.stringify(v);
  } catch {
    return String(v);
  }
}

export function logsText() {
  // Scrubbed again, because a swap's secrets may be newer than a line that came in first.
  return lines.map((l) => `${new Date(l.at).toISOString()} ${l.level} ${scrub(l.text)}${l.count ? ` (x${l.count})` : ''}`).join('\n');
}

export const logsCount = () => lines.length;

export function clearLogs() {
  lines = [];
  try {
    localStorage.removeItem(KEY);
  } catch {
    // Nothing to do.
  }
}

/** Also keep what the browser reports: warnings, errors, and failures nothing caught. */
export function captureConsole() {
  for (const level of ['warn', 'error']) {
    const original = console[level].bind(console);
    console[level] = (...args) => {
      log(level, ...args);
      original(...args);
    };
  }
  window.addEventListener('error', (e) => log('error', e.message, e.filename ?? '', e.lineno ?? ''));
  window.addEventListener('unhandledrejection', (e) => log('error', 'unhandled:', e.reason));
}
