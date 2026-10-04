//! `~/.sunmao/im/state.db` — the gateway's durable facts that are NOT
//! session facts: session_key→session-id routing index, pairing codes,
//! allowlist, delivery ledger, adapter cursors. Session truth stays in
//! the SessionEvent log; this file is the *edge* of the system — SQLite
//! (first in the workspace) because "update one row" / "list undelivered"
//! are exactly what an append-only JSONL can't do without a rewrite.
//!
//! `rusqlite` is sync — every call site here is short-lived; the gateway
//! never holds a lock across `.await`.

use std::path::Path;
use std::sync::Mutex;

use anyhow::Context as _;

/// Epoch seconds — the schema stores plain ints; ISO strings would make
/// range queries string-order puzzles.
pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// One pairing code a stranger was issued. `expires`/`last_try` bound the
/// issuance rate and lifetime; an approved code's row is deleted (the
/// allowlist row is the durable fact).
#[derive(Debug, Clone)]
pub struct PairingRow {
    pub channel: String,
    pub sender: String,
    pub code: String,
    pub created: i64,
    pub expires: i64,
}

/// A delivery-ledger row — `state` is pending → attempting → delivered.
/// `attempting` rows at startup are the redelivery set (their text already
/// gets the ♻️ prefix by the replayer, not stored).
#[derive(Debug, Clone)]
pub struct DeliveryRow {
    pub id: i64,
    pub channel: String,
    pub chat: String,
    pub text: String,
    pub state: String,
    pub attempts: i64,
}

/// Shared handle — `pub(crate)` clone-able so the `sunmao pairing` CLI and
/// the gateway share one implementation.
pub struct Store {
    conn: Mutex<rusqlite::Connection>,
}

impl Store {
    /// Open (creating) the state DB and run the schema. WAL mode so the
    /// `sunmao pairing` CLI can read while the daemon holds it open.
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let conn = rusqlite::Connection::open(dir.join("state.db"))
            .with_context(|| format!("open {}", dir.join("state.db").display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // WAL lets the pairing CLI read, but two WRITERS (daemon +
        // `sunmao pairing approve`) still collide — a short busy retry
        // turns that SQLITE_BUSY into a wait instead of an error
        conn.pragma_update(None, "busy_timeout", 2000u64)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS routes(
               session_key TEXT PRIMARY KEY,
               session_id  TEXT NOT NULL,
               created     INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS pairing(
               channel TEXT NOT NULL,
               sender  TEXT NOT NULL,
               code    TEXT NOT NULL,
               created INTEGER NOT NULL,
               expires INTEGER NOT NULL,
               PRIMARY KEY(channel, sender));
             CREATE TABLE IF NOT EXISTS allowlist(
               channel TEXT NOT NULL,
               sender  TEXT NOT NULL,
               role    TEXT NOT NULL DEFAULT 'user',
               created INTEGER NOT NULL,
               PRIMARY KEY(channel, sender));
             CREATE TABLE IF NOT EXISTS delivery(
               id       INTEGER PRIMARY KEY AUTOINCREMENT,
               channel  TEXT NOT NULL,
               chat     TEXT NOT NULL,
               text     TEXT NOT NULL,
               state    TEXT NOT NULL,
               attempts INTEGER NOT NULL DEFAULT 0,
               created  INTEGER NOT NULL,
               updated  INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS meta(
               key   TEXT PRIMARY KEY,
               value TEXT NOT NULL);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // ── routing index ──

    /// The session id bound to a session key — `None` when no message ever
    /// routed there.
    pub fn route_get(&self, key: &str) -> Option<String> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT session_id FROM routes WHERE session_key=?1",
                [key],
                |r| r.get(0),
            )
            .ok()
    }

    pub fn route_put(&self, key: &str, session_id: &str) -> anyhow::Result<()> {
        self.conn.lock().unwrap().execute(
            concat!(
                "INSERT OR REPLACE INTO routes(",
                "session_key, session_id, created) VALUES(?1,?2,?3)"
            ),
            rusqlite::params![key, session_id, now()],
        )?;
        Ok(())
    }

    // ── allowlist ──

    /// The sender's role (`owner`/`user`) if admitted, `None` otherwise.
    pub fn allow_role(&self, channel: &str, sender: &str) -> Option<String> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT role FROM allowlist WHERE channel=?1 AND sender=?2",
                [channel, sender],
                |r| r.get(0),
            )
            .ok()
    }

    /// Admit a sender directly — the test + mgmt path; `pairing_approve`
    /// is the transactional production path.
    #[cfg(test)]
    pub fn allow_add(&self, channel: &str, sender: &str) -> anyhow::Result<String> {
        let role = if self.has_owner() { "user" } else { "owner" };
        self.conn.lock().unwrap().execute(
            concat!(
                "INSERT OR REPLACE INTO allowlist(",
                "channel, sender, role, created) VALUES(?1,?2,?3,?4)"
            ),
            rusqlite::params![channel, sender, role, now()],
        )?;
        Ok(role.to_string())
    }

    pub fn allow_remove(&self, channel: &str, sender: &str) -> anyhow::Result<bool> {
        Ok(self.conn.lock().unwrap().execute(
            "DELETE FROM allowlist WHERE channel=?1 AND sender=?2",
            [channel, sender],
        )? > 0)
    }

    #[cfg(test)]
    pub fn has_owner(&self) -> bool {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT 1 FROM allowlist WHERE role='owner' LIMIT 1",
                [],
                |_| Ok(()),
            )
            .is_ok()
    }

    /// All admitted senders — `sunmao pairing list`'s tail section.
    pub fn allow_list(&self) -> Vec<(String, String, String)> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn
            .prepare("SELECT channel, sender, role FROM allowlist ORDER BY created")
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .flatten()
            .collect()
    }

    // ── pairing codes ──

    /// Live codes for one channel — issuance cap counts these.
    pub fn pairing_live(&self, channel: &str) -> Vec<PairingRow> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn
            .prepare(
                "SELECT channel, sender, code, created, expires FROM pairing
                 WHERE channel=?1 AND expires>?2",
            )
            .unwrap();
        st.query_map(rusqlite::params![channel, now()], |r| {
            Ok(PairingRow {
                channel: r.get(0)?,
                sender: r.get(1)?,
                code: r.get(2)?,
                created: r.get(3)?,
                expires: r.get(4)?,
            })
        })
        .unwrap()
        .flatten()
        .collect()
    }

    /// Every live code — `sunmao pairing list`.
    pub fn pairing_all(&self) -> Vec<PairingRow> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn
            .prepare(
                "SELECT channel, sender, code, created, expires FROM pairing
                 WHERE expires>?1 ORDER BY created",
            )
            .unwrap();
        st.query_map([now()], |r| {
            Ok(PairingRow {
                channel: r.get(0)?,
                sender: r.get(1)?,
                code: r.get(2)?,
                created: r.get(3)?,
                expires: r.get(4)?,
            })
        })
        .unwrap()
        .flatten()
        .collect()
    }

    /// When this sender last got a code — the per-sender cooldown check.
    pub fn pairing_sender_created(&self, channel: &str, sender: &str) -> Option<i64> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT created FROM pairing WHERE channel=?1 AND sender=?2",
                [channel, sender],
                |r| r.get(0),
            )
            .ok()
    }

    pub fn pairing_insert(&self, row: &PairingRow) -> anyhow::Result<()> {
        self.conn.lock().unwrap().execute(
            concat!(
                "INSERT OR REPLACE INTO pairing(",
                "channel, sender, code, created, expires) VALUES(?1,?2,?3,?4,?5)"
            ),
            rusqlite::params![row.channel, row.sender, row.code, row.created, row.expires],
        )?;
        Ok(())
    }

    /// `pairing approve` as ONE transaction — consume the code and admit
    /// the sender atomically. The two-step form could strand a consumed
    /// code if the process died before the allowlist insert: the sender
    /// would have to re-pair for no reason they could see.
    /// Returns the (channel, sender, role) on a live code, None on an
    /// expired/unknown one.
    pub fn pairing_approve(&self, code: &str) -> anyhow::Result<Option<(String, String, String)>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let row = tx
            .query_row(
                "SELECT channel, sender, expires FROM pairing WHERE code=?1",
                [code],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .ok();
        let Some((channel, sender, expires)) = row else {
            return Ok(None);
        };
        if expires <= now() {
            return Ok(None);
        }
        let has_owner: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM allowlist WHERE role='owner')",
            [],
            |r| r.get(0),
        )?;
        let role = if has_owner { "user" } else { "owner" };
        tx.execute(
            "DELETE FROM pairing WHERE channel=?1 AND sender=?2",
            [&channel, &sender],
        )?;
        tx.execute(
            concat!(
                "INSERT OR REPLACE INTO allowlist(",
                "channel, sender, role, created) VALUES(?1,?2,?3,?4)"
            ),
            rusqlite::params![channel, sender, role, now()],
        )?;
        tx.commit()?;
        Ok(Some((channel, sender, role.to_string())))
    }

    // ── delivery ledger ──

    /// Record an outbound reply before the first send — the ledger is
    /// what turns a crash mid-send into a redelivery instead of a loss.
    pub fn deliver_pending(&self, channel: &str, chat: &str, text: &str) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            concat!(
                "INSERT INTO delivery(",
                "channel, chat, text, state, attempts, created, updated) ",
                "VALUES(?1,?2,?3,'pending',0,?4,?4)"
            ),
            rusqlite::params![channel, chat, text, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Send in progress — a crash after this line means the message may
    /// have gone out; the replayer marks those with the ♻️ prefix.
    pub fn deliver_attempting(&self, id: i64) -> anyhow::Result<()> {
        self.conn.lock().unwrap().execute(
            concat!(
                "UPDATE delivery SET state='attempting', ",
                "attempts=attempts+1, updated=?2 WHERE id=?1"
            ),
            rusqlite::params![id, now()],
        )?;
        Ok(())
    }

    pub fn deliver_done(&self, id: i64) -> anyhow::Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE delivery SET state='delivered', updated=?2 WHERE id=?1",
            rusqlite::params![id, now()],
        )?;
        Ok(())
    }

    /// Dead-letter a row that exhausted its retry budget — it stays in
    /// the ledger (audit) but leaves the replay set.
    pub fn deliver_dead(&self, id: i64) -> anyhow::Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE delivery SET state='dead', updated=?2 WHERE id=?1",
            rusqlite::params![id, now()],
        )?;
        Ok(())
    }

    /// Everything that never made it — pending (never attempted) and
    /// attempting (send in flight when the process died).
    pub fn deliver_outstanding(&self) -> Vec<DeliveryRow> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn
            .prepare(
                "SELECT id, channel, chat, text, state, attempts FROM delivery
                 WHERE state IN ('pending','attempting') ORDER BY id",
            )
            .unwrap();
        st.query_map([], |r| {
            Ok(DeliveryRow {
                id: r.get(0)?,
                channel: r.get(1)?,
                chat: r.get(2)?,
                text: r.get(3)?,
                state: r.get(4)?,
                attempts: r.get(5)?,
            })
        })
        .unwrap()
        .flatten()
        .collect()
    }

    // ── adapter cursors ──

    /// Key-value slot for adapter state (Telegram's getUpdates offset).
    /// Persisted so a restart can't replay an already-processed update.
    pub fn kv_get(&self, key: &str) -> Option<String> {
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
            .ok()
    }

    pub fn kv_set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES(?1,?2)",
            [key, value],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "sunmao-im-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (Store::open(&dir).unwrap(), dir)
    }

    #[test]
    fn routes_round_trip() {
        let (s, dir) = store();
        assert_eq!(s.route_get("im:main"), None);
        s.route_put("im:main", "im-main").unwrap();
        assert_eq!(s.route_get("im:main").as_deref(), Some("im-main"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn pairing_lifecycle() {
        let (s, dir) = store();
        s.pairing_insert(&PairingRow {
            channel: "telegram".into(),
            sender: "7".into(),
            code: "ABCD2345".into(),
            created: now(),
            expires: now() + 3600,
        })
        .unwrap();
        assert_eq!(s.pairing_live("telegram").len(), 1);
        // first approval bootstraps owner — consume+admit is one
        // transaction now
        let (ch, sender, role) = s.pairing_approve("ABCD2345").unwrap().unwrap();
        assert_eq!(
            (ch.as_str(), sender.as_str(), role.as_str()),
            ("telegram", "7", "owner")
        );
        assert_eq!(s.allow_role("telegram", "7").as_deref(), Some("owner"));
        // second sender never inherits owner
        assert_eq!(s.allow_add("telegram", "8").unwrap(), "user");
        // consumed codes are single-use
        assert!(s.pairing_approve("ABCD2345").unwrap().is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn delivery_ledger_states() {
        let (s, dir) = store();
        let id = s.deliver_pending("telegram", "42", "hi").unwrap();
        assert_eq!(s.deliver_outstanding().len(), 1);
        s.deliver_attempting(id).unwrap();
        assert_eq!(s.deliver_outstanding().len(), 1);
        s.deliver_done(id).unwrap();
        assert!(s.deliver_outstanding().is_empty());
        std::fs::remove_dir_all(dir).ok();
    }
}
