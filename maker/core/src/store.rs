//! Durable state. Every irreversible action is recorded before it is attempted, so a restart can tell what might have
//! happened, read the chains, and carry on. SQLite in WAL mode with `synchronous = FULL` fsyncs each commit.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{de::DeserializeOwned, Serialize};

use crate::engine::{Action, Direction, Outcome, State, Terms};
use crate::policy::{Ledger, SwapRecord};

#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self(e.to_string())
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        Self(e.to_string())
    }
}

type Result<T> = std::result::Result<T, StoreError>;

fn json<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

fn from_json<T: DeserializeOwned>(text: &str) -> Result<T> {
    Ok(serde_json::from_str(text)?)
}

fn open(path: &Path, schema: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.execute_batch(schema)?;
    Ok(conn)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A UTC date, from Howard Hinnant's civil_from_days, so this needs no date library.
pub fn day_of(secs: u64) -> String {
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// One line of the report: swaps of one direction and outcome, on one day.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportLine {
    pub swaps: u64,
    /// In Solana base units.
    pub volume: i128,
    pub spread: i128,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// By day (none for swaps from before days were kept), direction and outcome.
    pub swaps: std::collections::BTreeMap<(Option<String>, String, String), ReportLine>,
    /// By day: L-BTC sats and lamports, less rent that came back.
    pub costs: std::collections::BTreeMap<String, (u64, i64)>,
    pub open: u64,
    pub abandoned: u64,
}

/// The engine's store. The signer's ledger is separate, in its own file and process.
pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = open(
            path,
            "CREATE TABLE IF NOT EXISTS swaps (
                 swap BLOB PRIMARY KEY,
                 terms TEXT NOT NULL,
                 state TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS intents (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 swap BLOB NOT NULL,
                 action TEXT NOT NULL,
                 stored_tx BLOB,
                 finished INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS flags (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS details (swap BLOB PRIMARY KEY, body TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS seen_coins (swap BLOB PRIMARY KEY, txid TEXT NOT NULL, vout INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS finished (swap BLOB PRIMARY KEY, at_secs INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS closed (swap BLOB PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS escrow_seen (swap BLOB PRIMARY KEY, slot INTEGER NOT NULL, deadline INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS costs (
                 swap BLOB NOT NULL, kind TEXT NOT NULL, at_secs INTEGER NOT NULL,
                 lbtc_sats INTEGER NOT NULL, lamports INTEGER NOT NULL,
                 PRIMARY KEY (swap, kind)
             );
             CREATE TABLE IF NOT EXISTS last_tip (id INTEGER PRIMARY KEY CHECK (id = 1), height INTEGER NOT NULL, hash TEXT NOT NULL);",
        )?;
        Ok(Self { conn })
    }

    /// Register a new swap. Refuses a payment swap ever seen before, forever: a used preimage is public.
    pub fn accept(&self, terms: &Terms, state: &State) -> Result<bool> {
        let added = self.conn.execute(
            "INSERT OR IGNORE INTO swaps (swap, terms, state) VALUES (?1, ?2, ?3)",
            params![terms.swap_id().as_slice(), json(terms)?, json(state)?],
        )?;
        Ok(added == 1)
    }

    pub fn state(&self, swap: &[u8; 32]) -> Result<Option<(Terms, State)>> {
        let row: Option<(String, String)> = self
            .conn
            .query_row("SELECT terms, state FROM swaps WHERE swap = ?1", params![swap.as_slice()], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        row.map(|(t, s)| Ok((from_json(&t)?, from_json(&s)?))).transpose()
    }

    /// Swaps not yet finished, for restart and for each tick.
    pub fn open_swaps(&self) -> Result<Vec<(Terms, State)>> {
        let mut stmt = self.conn.prepare("SELECT terms, state FROM swaps")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = vec![];
        for row in rows {
            let (t, s) = row?;
            let state: State = from_json(&s)?;
            if !matches!(state, State::Done { .. } | State::Abandoned) {
                out.push((from_json(&t)?, state));
            }
        }
        Ok(out)
    }

    /// Commit a new state together with the actions it requires, atomically, before attempting any of them.
    pub fn commit_step(&self, swap: &[u8; 32], state: &State, actions: &[Action]) -> Result<Vec<i64>> {
        let tx = self.conn.unchecked_transaction()?;
        // Final, because the signer has refused this swap for good. An engine pass that read it earlier can't bring it back.
        let current: Option<String> =
            tx.query_row("SELECT state FROM swaps WHERE swap = ?1", params![swap.as_slice()], |r| r.get(0)).optional()?;
        if current.as_deref() == Some(json(&State::Abandoned)?.as_str()) {
            return Ok(vec![]);
        }
        tx.execute("UPDATE swaps SET state = ?1 WHERE swap = ?2", params![json(state)?, swap.as_slice()])?;
        if matches!(state, State::Done { .. }) {
            tx.execute("INSERT OR IGNORE INTO finished (swap, at_secs) VALUES (?1, ?2)", params![swap.as_slice(), now_secs() as i64])?;
        }
        let mut ids = vec![];
        for action in actions {
            tx.execute("INSERT INTO intents (swap, action) VALUES (?1, ?2)", params![swap.as_slice(), json(action)?])?;
            ids.push(tx.last_insert_rowid());
        }
        tx.commit()?;
        Ok(ids)
    }

    /// What one step cost, for the report. Counted once, because a retry sends the same step again.
    /// Finished L→S swaps whose escrow, which we paid for, hasn't been closed for its rent yet.
    pub fn escrows_to_close(&self) -> Result<Vec<Terms>> {
        let mut query = self.conn.prepare(
            "SELECT terms, state FROM swaps WHERE swap NOT IN (SELECT swap FROM closed)",
        )?;
        let rows = query.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = vec![];
        for row in rows {
            let (terms, state) = row?;
            let terms: Terms = from_json(&terms)?;
            if terms.direction == Direction::L2S && matches!(from_json::<State>(&state)?, State::Done { .. }) {
                out.push(terms);
            }
        }
        Ok(out)
    }

    /// Its escrow is closed, or was never there.
    pub fn mark_closed(&self, swap: &[u8; 32]) -> Result<()> {
        self.conn.execute("INSERT OR IGNORE INTO closed (swap) VALUES (?1)", params![swap.as_slice()])?;
        Ok(())
    }

    /// Negative lamports are money back, such as rent.
    pub fn record_cost(&self, swap: &[u8; 32], kind: &str, lbtc_sats: u64, lamports: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO costs (swap, kind, at_secs, lbtc_sats, lamports) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![swap.as_slice(), kind, now_secs() as i64, lbtc_sats as i64, lamports],
        )?;
        Ok(())
    }

    /// Every finished swap, and every recorded cost. Swaps from before this was recorded have no day.
    pub fn report(&self) -> Result<Report> {
        let mut report = Report::default();
        let mut query = self.conn.prepare(
            "SELECT s.terms, s.state, f.at_secs FROM swaps s LEFT JOIN finished f ON f.swap = s.swap",
        )?;
        let rows = query.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?)))?;
        for row in rows {
            let (terms, state, at) = row?;
            let terms: Terms = from_json(&terms)?;
            let outcome = match from_json::<State>(&state)? {
                State::Done { outcome } => outcome,
                State::Abandoned => {
                    report.abandoned += 1;
                    continue;
                }
                _ => {
                    report.open += 1;
                    continue;
                }
            };
            let day = at.map(|secs| day_of(secs as u64));
            let line = report.swaps.entry((day, format!("{:?}", terms.direction), format!("{outcome:?}"))).or_default();
            line.swaps += 1;
            if outcome == Outcome::Completed {
                // Liquid amounts have two more decimals than Solana's.
                let (liquid, solana) = (i128::from(terms.liquid_amount / 100), i128::from(terms.solana_amount));
                line.volume += liquid.max(solana);
                line.spread += match terms.direction {
                    Direction::L2S => liquid - solana,
                    Direction::S2L => solana - liquid,
                };
            }
        }
        let mut query = self.conn.prepare("SELECT at_secs, lbtc_sats, lamports FROM costs")?;
        let rows = query.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?;
        for row in rows {
            let (at, sats, lamports) = row?;
            let line = report.costs.entry(day_of(at as u64)).or_default();
            line.0 += sats as u64;
            line.1 += lamports;
        }
        Ok(report)
    }

    /// Keep the exact lock transaction, before its first broadcast, so a retry can only ever send the same one.
    pub fn store_lock_tx(&self, swap: &[u8; 32], raw_tx: &[u8]) -> Result<()> {
        if let Some(existing) = self.stored_lock_tx(swap)? {
            if existing != raw_tx {
                return Err(StoreError("a different lock transaction is already stored for this swap".into()));
            }
            return Ok(());
        }
        let updated = self.conn.execute(
            "UPDATE intents SET stored_tx = ?1 WHERE id = (SELECT id FROM intents WHERE swap = ?2 AND action LIKE '%LockLiquid%' ORDER BY id LIMIT 1)",
            params![raw_tx, swap.as_slice()],
        )?;
        if updated == 1 {
            Ok(())
        } else {
            Err(StoreError("no lock intent for this swap".into()))
        }
    }

    pub fn stored_lock_tx(&self, swap: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn
            .query_row(
                "SELECT stored_tx FROM intents WHERE swap = ?1 AND stored_tx IS NOT NULL ORDER BY id LIMIT 1",
                params![swap.as_slice()],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn finish_intent(&self, id: i64) -> Result<()> {
        self.conn.execute("UPDATE intents SET finished = 1 WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Intents that may or may not have happened, which a restart must reconcile against the chains.
    pub fn unfinished_intents(&self) -> Result<Vec<(i64, [u8; 32], Action)>> {
        let mut stmt = self.conn.prepare("SELECT id, swap, action FROM intents WHERE finished = 0 ORDER BY id")?;
        let rows =
            stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, String>(2)?)))?;
        let mut out = vec![];
        for row in rows {
            let (id, swap, action) = row?;
            let swap: [u8; 32] = swap.try_into().map_err(|_| StoreError("bad swap in store".into()))?;
            out.push((id, swap, from_json(&action)?));
        }
        Ok(out)
    }

    /// The full terms for a swap, in whatever form the daemon keeps them.
    pub fn set_details(&self, swap: &[u8; 32], body: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO details (swap, body) VALUES (?1, ?2) ON CONFLICT(swap) DO UPDATE SET body = ?2",
            params![swap.as_slice(), body],
        )?;
        Ok(())
    }

    pub fn details(&self, swap: &[u8; 32]) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT body FROM details WHERE swap = ?1", params![swap.as_slice()], |r| r.get(0))
            .optional()?)
    }

    /// Remember the swap's coin the first time it is seen, because a spent coin can't be found at its address any more.
    pub fn remember_coin(&self, swap: &[u8; 32], txid: &str, vout: u32) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO seen_coins (swap, txid, vout) VALUES (?1, ?2, ?3)",
            params![swap.as_slice(), txid, vout],
        )?;
        Ok(())
    }

    pub fn seen_coin(&self, swap: &[u8; 32]) -> Result<Option<(String, u32)>> {
        Ok(self
            .conn
            .query_row("SELECT txid, vout FROM seen_coins WHERE swap = ?1", params![swap.as_slice()], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?)
    }

    /// Note an escrow seen open, at the slot it was read at. Both only go up: a deadline only moves later.
    pub fn see_escrow(&self, swap: &[u8; 32], slot: u64, deadline: u64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO escrow_seen (swap, slot, deadline) VALUES (?1, ?2, ?3)
             ON CONFLICT (swap) DO UPDATE SET slot = max(slot, excluded.slot), deadline = max(deadline, excluded.deadline)",
            params![swap.as_slice(), slot as i64, deadline as i64],
        )?;
        Ok(())
    }

    /// The last slot an escrow was seen at, and the latest deadline seen on it.
    pub fn escrow_seen(&self, swap: &[u8; 32]) -> Result<Option<(u64, u64)>> {
        Ok(self
            .conn
            .query_row("SELECT slot, deadline FROM escrow_seen WHERE swap = ?1", params![swap.as_slice()], |r| {
                Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
            })
            .optional()?)
    }

    /// The last Liquid tip seen, kept across restarts so a rollback during downtime is still noticed.
    pub fn last_tip(&self) -> Result<Option<(u32, String)>> {
        Ok(self
            .conn
            .query_row("SELECT height, hash FROM last_tip WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    }

    pub fn set_last_tip(&self, height: u32, hash: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO last_tip (id, height, hash) VALUES (1, ?1, ?2) ON CONFLICT(id) DO UPDATE SET height = ?1, hash = ?2",
            params![height, hash],
        )?;
        Ok(())
    }

    /// Latch a rollback alarm. Only an operator clears it. Clearing also forgets the last tip seen, because that block
    /// may no longer exist, and comparing against it would raise the alarm again straight away.
    pub fn set_rollback(&self, on: bool) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO flags (name, value) VALUES ('rollback', ?1) ON CONFLICT(name) DO UPDATE SET value = ?1",
            params![on as i64],
        )?;
        if !on {
            tx.execute("DELETE FROM last_tip", [])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn rollback(&self) -> Result<bool> {
        let value: Option<i64> =
            self.conn.query_row("SELECT value FROM flags WHERE name = 'rollback'", [], |r| r.get(0)).optional()?;
        Ok(value == Some(1))
    }
}

/// The signer's ledger, in its own file. It answers `policy::Ledger` and persists each record before signing.
pub struct SignerLedger {
    conn: Connection,
}

impl SignerLedger {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = open(
            path,
            "CREATE TABLE IF NOT EXISTS records (swap BLOB PRIMARY KEY, record TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS locks (at_secs INTEGER NOT NULL, amount INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS lock_txs (swap BLOB PRIMARY KEY, raw_tx BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS adaptors (swap BLOB PRIMARY KEY, body TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS fee_coins (swap BLOB PRIMARY KEY, outpoint TEXT NOT NULL UNIQUE);
             CREATE TABLE IF NOT EXISTS used_coins (outpoint TEXT PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS pending_top_up (id INTEGER PRIMARY KEY CHECK (id = 1), raw_tx BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS addresses (idx INTEGER PRIMARY KEY, swap BLOB UNIQUE);",
        )?;
        Ok(Self { conn })
    }

    /// The one Liquid lock signed for this swap, if any. A retry gets these exact bytes, so we can't pay twice.
    pub fn lock_tx(&self, swap: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn
            .query_row("SELECT raw_tx FROM lock_txs WHERE swap = ?1", params![swap.as_slice()], |r| r.get(0))
            .optional()?)
    }

    /// The adapter session's result together with the record that says so. Losing this after a lock would lose the
    /// taker's coin, so it is fsynced before the Solana lock or our share can happen.
    pub fn commit_adaptor(&self, swap: &[u8; 32], record: &SwapRecord, body: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("INSERT INTO adaptors (swap, body) VALUES (?1, ?2)", params![swap.as_slice(), body])?;
        tx.execute(
            "INSERT INTO records (swap, record) VALUES (?1, ?2) ON CONFLICT(swap) DO UPDATE SET record = ?2",
            params![swap.as_slice(), json(record)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn adaptor(&self, swap: &[u8; 32]) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT body FROM adaptors WHERE swap = ?1", params![swap.as_slice()], |r| r.get(0))
            .optional()?)
    }

    /// Give this swap one fee coin. An adapter signature commits to the exact inputs, so the coin must never be spent on
    /// anything else, or the claim it signs would become invalid. Refuses a coin that is already taken.
    pub fn reserve_fee_coin(&self, swap: &[u8; 32], outpoint: &str) -> Result<bool> {
        let added = self.conn.execute(
            "INSERT OR IGNORE INTO fee_coins (swap, outpoint) VALUES (?1, ?2)",
            params![swap.as_slice(), outpoint],
        )?;
        Ok(added == 1)
    }

    /// An address index for our own coins, recorded before anything pays it, so the wallet always reads it. A swap keeps one
    /// index, so a retried claim or refund pays the same address. Without a swap, each call gives a new one. Index 0 is the
    /// wallet's own address and is never handed out here.
    pub fn address_index(&self, swap: Option<&[u8; 32]>) -> Result<u32> {
        if let Some(swap) = swap {
            let known: Option<u32> = self
                .conn
                .query_row("SELECT idx FROM addresses WHERE swap = ?1", params![swap.as_slice()], |r| r.get(0))
                .optional()?;
            if let Some(idx) = known {
                return Ok(idx);
            }
        }
        let next: u32 = self.conn.query_row("SELECT COALESCE(MAX(idx), 0) + 1 FROM addresses", [], |r| r.get(0))?;
        self.conn.execute("INSERT INTO addresses (idx, swap) VALUES (?1, ?2)", params![next, swap.map(|s| s.to_vec())])?;
        Ok(next)
    }

    /// Every index handed out, in order.
    pub fn address_indexes(&self) -> Result<Vec<u32>> {
        let mut stmt = self.conn.prepare("SELECT idx FROM addresses ORDER BY idx")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn fee_coin(&self, swap: &[u8; 32]) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT outpoint FROM fee_coins WHERE swap = ?1", params![swap.as_slice()], |r| r.get(0))
            .optional()?)
    }

    pub fn is_reserved(&self, outpoint: &str) -> Result<bool> {
        let found: Option<i64> = self
            .conn
            .query_row("SELECT 1 FROM fee_coins WHERE outpoint = ?1", params![outpoint], |r| r.get(0))
            .optional()?;
        Ok(found.is_some())
    }

    /// Whether a signed payment already spends this coin. A node's coin list can lag behind what we've signed, and a lock that
    /// reused a coin could never land, because a retry must send the same bytes. A rollback that drops such a payment leaves
    /// its coins marked, which is safe, and is for the operator to sort out.
    pub fn is_used(&self, outpoint: &str) -> Result<bool> {
        let found: Option<i64> = self
            .conn
            .query_row("SELECT 1 FROM used_coins WHERE outpoint = ?1", params![outpoint], |r| r.get(0))
            .optional()?;
        Ok(found.is_some())
    }

    /// The fee coin top-up signed and not yet seen confirmed. A retry sends these bytes.
    pub fn pending_top_up(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.conn.query_row("SELECT raw_tx FROM pending_top_up WHERE id = 1", [], |r| r.get(0)).optional()?)
    }

    pub fn set_pending_top_up(&self, raw_tx: &[u8], spends: &[String]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for outpoint in spends {
            tx.execute("INSERT OR IGNORE INTO used_coins (outpoint) VALUES (?1)", params![outpoint])?;
        }
        tx.execute("INSERT OR REPLACE INTO pending_top_up (id, raw_tx) VALUES (1, ?1)", params![raw_tx])?;
        tx.commit()?;
        Ok(())
    }

    pub fn clear_pending_top_up(&self) -> Result<()> {
        self.conn.execute("DELETE FROM pending_top_up", [])?;
        Ok(())
    }

    /// Persist, and fsync, before the signature leaves the signer.
    pub fn commit(
        &self,
        swap: &[u8; 32],
        record: &SwapRecord,
        newly_locked: Option<(u64, u64)>,
        lock_tx: Option<&[u8]>,
        spends: &[String],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for outpoint in spends {
            tx.execute("INSERT OR IGNORE INTO used_coins (outpoint) VALUES (?1)", params![outpoint])?;
        }
        if let Some(raw) = lock_tx {
            tx.execute("INSERT INTO lock_txs (swap, raw_tx) VALUES (?1, ?2)", params![swap.as_slice(), raw])?;
        }
        tx.execute(
            "INSERT INTO records (swap, record) VALUES (?1, ?2) ON CONFLICT(swap) DO UPDATE SET record = ?2",
            params![swap.as_slice(), json(record)?],
        )?;
        if let Some((at, amount)) = newly_locked {
            tx.execute("INSERT INTO locks (at_secs, amount) VALUES (?1, ?2)", params![at as i64, amount as i64])?;
        }
        tx.commit()?;
        Ok(())
    }
}

impl Ledger for SignerLedger {
    /// A read failure must refuse, so an unreadable record looks like an already-locked swap with a claim on it.
    fn record(&self, swap: &[u8; 32]) -> SwapRecord {
        let row: rusqlite::Result<Option<String>> = self
            .conn
            .query_row("SELECT record FROM records WHERE swap = ?1", params![swap.as_slice()], |r| r.get(0))
            .optional();
        match row {
            Ok(None) => SwapRecord::default(),
            Ok(Some(text)) => from_json(&text).unwrap_or_else(|_| fail_closed()),
            Err(_) => fail_closed(),
        }
    }

    fn locked_in_window(&self, now_secs: u64, window_secs: u64) -> u64 {
        let since = now_secs.saturating_sub(window_secs) as i64;
        self.conn
            .query_row("SELECT COALESCE(SUM(amount), 0) FROM locks WHERE at_secs > ?1", params![since], |r| {
                r.get::<_, i64>(0)
            })
            .map(|sum| sum as u64)
            .unwrap_or(u64::MAX)
    }
}

fn fail_closed() -> SwapRecord {
    SwapRecord {
        direction: Some(crate::engine::Direction::S2L),
        locked_amount: u64::MAX,
        deadline_slot: 0,
        solana_claimed: true,
        liquid_refunded: false,
        adaptor_held: true,
        early_refunded: true,
    }
}
