//! Durable state. Every irreversible action is recorded before it is attempted, so a restart can tell what might have
//! happened, read the chains, and carry on. SQLite in WAL mode with `synchronous = FULL` fsyncs each commit.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{de::DeserializeOwned, Serialize};

use crate::engine::{Action, State, Terms};
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
    conn.execute_batch(schema)?;
    Ok(conn)
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
                 hash BLOB PRIMARY KEY,
                 terms TEXT NOT NULL,
                 state TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS intents (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 hash BLOB NOT NULL,
                 action TEXT NOT NULL,
                 stored_tx BLOB,
                 finished INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS flags (name TEXT PRIMARY KEY, value INTEGER NOT NULL);",
        )?;
        Ok(Self { conn })
    }

    /// Register a new swap. Refuses a payment hash ever seen before, forever: a used preimage is public.
    pub fn accept(&self, terms: &Terms, state: &State) -> Result<bool> {
        let added = self.conn.execute(
            "INSERT OR IGNORE INTO swaps (hash, terms, state) VALUES (?1, ?2, ?3)",
            params![terms.payment_hash.as_slice(), json(terms)?, json(state)?],
        )?;
        Ok(added == 1)
    }

    pub fn state(&self, hash: &[u8; 32]) -> Result<Option<(Terms, State)>> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT terms, state FROM swaps WHERE hash = ?1",
                params![hash.as_slice()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(t, s)| Ok((from_json(&t)?, from_json(&s)?)))
            .transpose()
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
    pub fn commit_step(
        &self,
        hash: &[u8; 32],
        state: &State,
        actions: &[Action],
    ) -> Result<Vec<i64>> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE swaps SET state = ?1 WHERE hash = ?2",
            params![json(state)?, hash.as_slice()],
        )?;
        let mut ids = vec![];
        for action in actions {
            tx.execute(
                "INSERT INTO intents (hash, action) VALUES (?1, ?2)",
                params![hash.as_slice(), json(action)?],
            )?;
            ids.push(tx.last_insert_rowid());
        }
        tx.commit()?;
        Ok(ids)
    }

    /// Keep the exact lock transaction, before its first broadcast, so a retry can only ever send the same one.
    pub fn store_lock_tx(&self, hash: &[u8; 32], raw_tx: &[u8]) -> Result<()> {
        if let Some(existing) = self.stored_lock_tx(hash)? {
            if existing != raw_tx {
                return Err(StoreError(
                    "a different lock transaction is already stored for this swap".into(),
                ));
            }
            return Ok(());
        }
        let updated = self.conn.execute(
            "UPDATE intents SET stored_tx = ?1 WHERE id = (SELECT id FROM intents WHERE hash = ?2 AND action LIKE '%LockLiquid%' ORDER BY id LIMIT 1)",
            params![raw_tx, hash.as_slice()],
        )?;
        if updated == 1 {
            Ok(())
        } else {
            Err(StoreError("no lock intent for this swap".into()))
        }
    }

    pub fn stored_lock_tx(&self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn
            .query_row(
                "SELECT stored_tx FROM intents WHERE hash = ?1 AND stored_tx IS NOT NULL ORDER BY id LIMIT 1",
                params![hash.as_slice()],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn finish_intent(&self, id: i64) -> Result<()> {
        self.conn
            .execute("UPDATE intents SET finished = 1 WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Intents that may or may not have happened, which a restart must reconcile against the chains.
    pub fn unfinished_intents(&self) -> Result<Vec<(i64, [u8; 32], Action)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, hash, action FROM intents WHERE finished = 0 ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = vec![];
        for row in rows {
            let (id, hash, action) = row?;
            let hash: [u8; 32] = hash
                .try_into()
                .map_err(|_| StoreError("bad hash in store".into()))?;
            out.push((id, hash, from_json(&action)?));
        }
        Ok(out)
    }

    /// Latch a rollback alarm. Only an operator clears it.
    pub fn set_rollback(&self, on: bool) -> Result<()> {
        self.conn.execute(
            "INSERT INTO flags (name, value) VALUES ('rollback', ?1) ON CONFLICT(name) DO UPDATE SET value = ?1",
            params![on as i64],
        )?;
        Ok(())
    }

    pub fn rollback(&self) -> Result<bool> {
        let value: Option<i64> = self
            .conn
            .query_row("SELECT value FROM flags WHERE name = 'rollback'", [], |r| {
                r.get(0)
            })
            .optional()?;
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
            "CREATE TABLE IF NOT EXISTS records (hash BLOB PRIMARY KEY, record TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS locks (at_secs INTEGER NOT NULL, amount INTEGER NOT NULL);",
        )?;
        Ok(Self { conn })
    }

    /// Persist, and fsync, before the signature leaves the signer.
    pub fn commit(
        &self,
        hash: &[u8; 32],
        record: &SwapRecord,
        newly_locked: Option<(u64, u64)>,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO records (hash, record) VALUES (?1, ?2) ON CONFLICT(hash) DO UPDATE SET record = ?2",
            params![hash.as_slice(), json(record)?],
        )?;
        if let Some((at, amount)) = newly_locked {
            tx.execute(
                "INSERT INTO locks (at_secs, amount) VALUES (?1, ?2)",
                params![at as i64, amount as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

impl Ledger for SignerLedger {
    /// A read failure must refuse, so an unreadable record looks like an already-locked swap with a claim on it.
    fn record(&self, hash: &[u8; 32]) -> SwapRecord {
        let row: rusqlite::Result<Option<String>> = self
            .conn
            .query_row(
                "SELECT record FROM records WHERE hash = ?1",
                params![hash.as_slice()],
                |r| r.get(0),
            )
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
            .query_row(
                "SELECT COALESCE(SUM(amount), 0) FROM locks WHERE at_secs > ?1",
                params![since],
                |r| r.get::<_, i64>(0),
            )
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
    }
}
