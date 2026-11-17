//! A07 additions to the ticket repository: v2 schema/migration, close-operation
//! compare-and-set claims, durable state history, archive references and audited
//! operator recovery. Every method is one short SQLite transaction; no transaction or
//! lock is ever held across an awaited Discord/HTTP call.
use super::repository::{read_ticket, Repository, Ticket};
use crate::Error;
use rusqlite::{params, OptionalExtension};

/// Every nonterminal state holds the per-guild/user/type slot. Only
/// failed/closed/released free it.
pub const NONTERMINAL: &str =
    "'reserved','creating','active','closing','archiving','verified','delete_requested','delete_uncertain','recovery'";
/// States in which a close worker owns (or owned) the ticket under a lease.
pub const IN_CLOSE: &str = "'closing','archiving','verified','delete_requested','delete_uncertain'";
pub const SCHEMA_VERSION: i64 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub from: String,
    pub to: String,
    pub actor: u64,
    pub note: String,
    pub at: i64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    pub actor: u64,
    pub action: String,
    pub reason: String,
    pub at: i64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveRef {
    pub ticket: i64,
    pub seq: i64,
    pub dir_name: String,
    pub manifest_sha256: String,
    pub messages: u64,
    pub assets: u64,
}

fn allowed(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("closing", "archiving")
            | ("closing", "recovery")
            | ("archiving", "verified")
            | ("archiving", "closing")
            | ("verified", "delete_requested")
            | ("delete_requested", "closed")
            | ("delete_requested", "delete_uncertain")
            | ("delete_requested", "verified")
            | ("delete_uncertain", "closed")
            | ("delete_uncertain", "delete_requested")
            | ("delete_uncertain", "recovery")
            | ("verified", "closed")
    )
}

const V2_TICKETS: &str = "CREATE TABLE {name}(id INTEGER PRIMARY KEY AUTOINCREMENT, operation TEXT NOT NULL UNIQUE DEFAULT (lower(hex(randomblob(16)))), guild TEXT NOT NULL, opener TEXT NOT NULL, kind TEXT NOT NULL CHECK(kind IN ('support','age')), status TEXT NOT NULL CHECK(status IN ('reserved','creating','active','failed','closed','closing','archiving','verified','delete_requested','delete_uncertain','recovery','released')), channel TEXT UNIQUE, greeting TEXT NOT NULL DEFAULT 'pending', close_token TEXT, close_actor TEXT, lease_until INTEGER NOT NULL DEFAULT 0);";

fn create_aux(tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS ticket_panels(guild TEXT NOT NULL, channel TEXT NOT NULL, message TEXT PRIMARY KEY, bot TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1);
         CREATE TABLE IF NOT EXISTS ticket_interactions(interaction TEXT PRIMARY KEY, ticket INTEGER NOT NULL REFERENCES tickets(id));
         CREATE TABLE IF NOT EXISTS ticket_history(id INTEGER PRIMARY KEY AUTOINCREMENT, ticket INTEGER NOT NULL, at INTEGER NOT NULL, from_status TEXT NOT NULL, to_status TEXT NOT NULL, actor TEXT NOT NULL, note TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS ticket_archives(ticket INTEGER NOT NULL, seq INTEGER NOT NULL, dir_name TEXT NOT NULL, manifest_sha256 TEXT NOT NULL, messages INTEGER NOT NULL, assets INTEGER NOT NULL, verified_at INTEGER NOT NULL, PRIMARY KEY(ticket,seq));
         CREATE TABLE IF NOT EXISTS ticket_audit(id INTEGER PRIMARY KEY AUTOINCREMENT, ticket INTEGER NOT NULL, at INTEGER NOT NULL, actor TEXT NOT NULL, action TEXT NOT NULL, reason TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS ticket_dm(ticket INTEGER PRIMARY KEY, status TEXT NOT NULL, at INTEGER NOT NULL);
         CREATE UNIQUE INDEX IF NOT EXISTS ticket_active_slot ON tickets(guild,opener,kind) WHERE status IN ({NONTERMINAL});"
    ))
}

/// Create the v2 schema, or migrate v1 -> v2 in one IMMEDIATE transaction. Opening a
/// database written by a newer schema is refused.
pub(super) fn migrate(c: &mut rusqlite::Connection) -> rusqlite::Result<()> {
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    // Foreign keys must be off while the parent table is rebuilt (cannot change inside a tx).
    c.execute_batch("PRAGMA foreign_keys=OFF;")?;
    let result = (|| {
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS ticket_schema(version INTEGER NOT NULL);")?;
        let version: Option<i64> = tx
            .query_row("SELECT version FROM ticket_schema", [], |r| r.get(0))
            .optional()?;
        match version {
            None => {
                tx.execute_batch(&V2_TICKETS.replace("{name}", "tickets"))?;
                create_aux(&tx)?;
                tx.execute("INSERT INTO ticket_schema VALUES(?1)", [SCHEMA_VERSION])?;
            }
            Some(1) => {
                tx.execute_batch(&V2_TICKETS.replace("{name}", "tickets_v2"))?;
                tx.execute_batch("INSERT INTO tickets_v2(id,operation,guild,opener,kind,status,channel,greeting) SELECT id,operation,guild,opener,kind,status,channel,greeting FROM tickets;")?;
                let old_seq: i64 = tx
                    .query_row("SELECT seq FROM sqlite_sequence WHERE name='tickets'", [], |r| r.get(0))
                    .optional()?
                    .unwrap_or(0);
                tx.execute_batch("DROP INDEX IF EXISTS ticket_active_slot; DROP TABLE tickets; ALTER TABLE tickets_v2 RENAME TO tickets;")?;
                let max_id: i64 = tx.query_row("SELECT COALESCE(MAX(id),0) FROM tickets", [], |r| r.get(0))?;
                tx.execute("DELETE FROM sqlite_sequence WHERE name='tickets'", [])?;
                tx.execute(
                    "INSERT INTO sqlite_sequence(name,seq) VALUES('tickets',?1)",
                    [old_seq.max(max_id)],
                )?;
                create_aux(&tx)?;
                let bad: i64 = tx.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| r.get(0))?;
                if bad != 0 {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                tx.execute("UPDATE ticket_schema SET version=?1", [SCHEMA_VERSION])?;
            }
            Some(SCHEMA_VERSION) => create_aux(&tx)?,
            Some(_) => return Err(rusqlite::Error::InvalidQuery),
        }
        tx.commit()
    })();
    c.execute_batch("PRAGMA foreign_keys=ON;")?;
    result
}

fn hist(
    tx: &rusqlite::Transaction<'_>,
    id: i64,
    from: &str,
    to: &str,
    actor: u64,
    note: &str,
    now: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO ticket_history(ticket,at,from_status,to_status,actor,note) VALUES(?1,?2,?3,?4,?5,?6)",
        params![id, now, from, to, actor.to_string(), note],
    )?;
    Ok(())
}
const COLS: &str = "id,guild,opener,kind,status,channel,greeting,operation";

impl Repository {
    pub async fn schema_version(&self) -> Result<i64, Error> {
        Ok(self
            .db
            .call(|c| -> rusqlite::Result<i64> { c.query_row("SELECT version FROM ticket_schema", [], |r| r.get(0)) })
            .await?)
    }
    pub async fn ticket_by_id(&self, id: i64) -> Result<Option<Ticket>, Error> {
        Ok(self
            .db
            .call(move |c| -> rusqlite::Result<Option<Ticket>> {
                c.query_row(&format!("SELECT {COLS} FROM tickets WHERE id=?1"), [id], read_ticket).optional()
            })
            .await?)
    }
    /// Durable linkage: guild AND channel must both match a stored ticket.
    pub async fn ticket_by_channel(&self, guild: u64, channel: u64) -> Result<Option<Ticket>, Error> {
        Ok(self
            .db
            .call(move |c| -> rusqlite::Result<Option<Ticket>> {
                c.query_row(
                    &format!("SELECT {COLS} FROM tickets WHERE guild=?1 AND channel=?2"),
                    params![guild.to_string(), channel.to_string()],
                    read_ticket,
                )
                .optional()
            })
            .await?)
    }
    /// True for ANY ticket row (including closed/archived) that ever owned this channel id.
    pub async fn channel_known(&self, channel: u64) -> Result<bool, Error> {
        Ok(self
            .db
            .call(move |c| -> rusqlite::Result<bool> {
                c.query_row("SELECT EXISTS(SELECT 1 FROM tickets WHERE channel=?1)", [channel.to_string()], |r| r.get(0))
            })
            .await?)
    }
    pub async fn nonterminal(&self) -> Result<Vec<Ticket>, Error> {
        Ok(self
            .db
            .call(|c| -> rusqlite::Result<Vec<Ticket>> {
                let mut s = c.prepare(&format!("SELECT {COLS} FROM tickets WHERE status IN ({NONTERMINAL}) ORDER BY id"))?;
                let rows = s.query_map([], read_ticket)?.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }
    /// Compare-and-set: only an `active` ticket with a stored channel can be claimed.
    pub async fn claim_close(&self, id: i64, actor: u64, token: &str, now: i64, lease_until: i64) -> Result<bool, Error> {
        let token = token.to_string();
        Ok(self
            .db
            .call(move |c| -> rusqlite::Result<bool> {
                let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let n = tx.execute(
                    "UPDATE tickets SET status='closing',close_token=?2,close_actor=?3,lease_until=?4 WHERE id=?1 AND status='active' AND channel IS NOT NULL",
                    params![id, token, actor.to_string(), lease_until],
                )?;
                if n == 1 {
                    hist(&tx, id, "active", "closing", actor, "close claimed", now)?;
                }
                tx.commit()?;
                Ok(n == 1)
            })
            .await?)
    }
    /// Take over an in-progress close whose lease expired (restart or crashed worker).
    pub async fn resume_close(&self, id: i64, actor: u64, token: &str, now: i64, lease_until: i64) -> Result<bool, Error> {
        let token = token.to_string();
        Ok(self
            .db
            .call(move |c| -> rusqlite::Result<bool> {
                let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let status: Option<String> = tx
                    .query_row(
                        &format!("SELECT status FROM tickets WHERE id=?1 AND status IN ({IN_CLOSE}) AND lease_until<=?2"),
                        params![id, now],
                        |r| r.get(0),
                    )
                    .optional()?;
                let Some(status) = status else { return Ok(false) };
                let n = tx.execute(
                    "UPDATE tickets SET close_token=?2,close_actor=?3,lease_until=?4 WHERE id=?1 AND status=?5 AND lease_until<=?6",
                    params![id, token, actor.to_string(), lease_until, status, now],
                )?;
                if n == 1 {
                    hist(&tx, id, &status, &status, actor, "close resumed after lease expiry", now)?;
                }
                tx.commit()?;
                Ok(n == 1)
            })
            .await?)
    }
    /// Token-checked state transition validated against the allowed-transition table.
    pub async fn advance(&self, id: i64, token: &str, from: &str, to: &str, now: i64, lease_until: i64, note: &str) -> Result<bool, Error> {
        if !allowed(from, to) {
            return Ok(false);
        }
        let (token, from, to, note) = (token.to_string(), from.to_string(), to.to_string(), note.to_string());
        Ok(self
            .db
            .call(move |c| -> rusqlite::Result<bool> {
                let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let actor: Option<String> = tx
                    .query_row("SELECT close_actor FROM tickets WHERE id=?1 AND status=?2 AND close_token=?3", params![id, from, token], |r| r.get(0))
                    .optional()?;
                let Some(actor) = actor else { return Ok(false) };
                let n = tx.execute(
                    "UPDATE tickets SET status=?4,lease_until=?5 WHERE id=?1 AND status=?2 AND close_token=?3",
                    params![id, from, token, to, lease_until],
                )?;
                if n == 1 {
                    hist(&tx, id, &from, &to, actor.parse().unwrap_or(0), &note, now)?;
                }
                tx.commit()?;
                Ok(n == 1)
            })
            .await?)
    }
    /// Voluntarily expire the lease (e.g. after an aborted attempt) so a retry may resume.
    pub async fn release_lease(&self, id: i64, token: &str) -> Result<bool, Error> {
        let token = token.to_string();
        Ok(self
            .db
            .call(move |c| -> rusqlite::Result<bool> {
                Ok(c.execute("UPDATE tickets SET lease_until=0 WHERE id=?1 AND close_token=?2", params![id, token])? == 1)
            })
            .await?)
    }
    pub async fn history(&self, id: i64) -> Result<Vec<HistoryEntry>, Error> {
        Ok(self
            .db
            .call(move |c| -> rusqlite::Result<Vec<HistoryEntry>> {
                let mut s = c.prepare("SELECT from_status,to_status,actor,note,at FROM ticket_history WHERE ticket=?1 ORDER BY id")?;
                let rows = s
                    .query_map([id], |r| {
                        Ok(HistoryEntry { from: r.get(0)?, to: r.get(1)?, actor: r.get::<_, String>(2)?.parse().unwrap_or(0), note: r.get(3)?, at: r.get(4)? })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }
    /// Record a verified archive for the lease holder; returns the new sequence number.
    pub async fn record_archive(&self, id: i64, token: &str, dir_name: &str, sha: &str, messages: u64, assets: u64, now: i64) -> Result<Option<i64>, Error> {
        let (token, dir_name, sha) = (token.to_string(), dir_name.to_string(), sha.to_string());
        Ok(self
            .db
            .call(move |c| -> rusqlite::Result<Option<i64>> {
                let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let held: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM tickets WHERE id=?1 AND close_token=?2 AND status IN ('archiving'))", params![id, token], |r| r.get(0))?;
                if !held {
                    return Ok(None);
                }
                let seq: i64 = tx.query_row("SELECT COALESCE(MAX(seq),0)+1 FROM ticket_archives WHERE ticket=?1", [id], |r| r.get(0))?;
                tx.execute(
                    "INSERT INTO ticket_archives(ticket,seq,dir_name,manifest_sha256,messages,assets,verified_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![id, seq, dir_name, sha, messages as i64, assets as i64, now],
                )?;
                tx.commit()?;
                Ok(Some(seq))
            })
            .await?)
    }
    pub async fn next_archive_seq(&self, id: i64) -> Result<i64, Error> {
        Ok(self
            .db
            .call(move |c| -> rusqlite::Result<i64> {
                c.query_row("SELECT COALESCE(MAX(seq),0)+1 FROM ticket_archives WHERE ticket=?1", [id], |r| r.get(0))
            })
            .await?)
    }
    pub async fn latest_archive(&self, id: i64) -> Result<Option<ArchiveRef>, Error> {
        Ok(self
            .db
            .call(move |c| -> rusqlite::Result<Option<ArchiveRef>> {
                c.query_row(
                    "SELECT ticket,seq,dir_name,manifest_sha256,messages,assets FROM ticket_archives WHERE ticket=?1 ORDER BY seq DESC LIMIT 1",
                    [id],
                    |r| Ok(ArchiveRef { ticket: r.get(0)?, seq: r.get(1)?, dir_name: r.get(2)?, manifest_sha256: r.get(3)?, messages: r.get::<_, i64>(4)? as u64, assets: r.get::<_, i64>(5)? as u64 }),
                )
                .optional()
            })
            .await?)
    }
    pub async fn record_dm(&self, id: i64, status: &str, now: i64) -> Result<(), Error> {
        let status = status.to_string();
        self.db
            .call(move |c| -> rusqlite::Result<()> {
                c.execute("INSERT OR REPLACE INTO ticket_dm(ticket,status,at) VALUES(?1,?2,?3)", params![id, status, now])?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    pub async fn dm_status(&self, id: i64) -> Result<Option<String>, Error> {
        Ok(self
            .db
            .call(move |c| -> rusqlite::Result<Option<String>> {
                c.query_row("SELECT status FROM ticket_dm WHERE ticket=?1", [id], |r| r.get(0)).optional()
            })
            .await?)
    }
    /// System transition into `recovery` (retains the slot, needs an audited operator).
    pub async fn mark_recovery(&self, id: i64, now: i64, note: &str) -> Result<bool, Error> {
        let note = note.to_string();
        Ok(self
            .db
            .call(move |c| -> rusqlite::Result<bool> {
                let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let from: Option<String> = tx
                    .query_row("SELECT status FROM tickets WHERE id=?1 AND status IN ('creating','active','delete_uncertain','closing')", [id], |r| r.get(0))
                    .optional()?;
                let Some(from) = from else { return Ok(false) };
                tx.execute("UPDATE tickets SET status='recovery',lease_until=0 WHERE id=?1 AND status=?2", params![id, from])?;
                hist(&tx, id, &from, "recovery", 0, &note, now)?;
                tx.commit()?;
                Ok(true)
            })
            .await?)
    }
    /// Audited operator action: attach a verified remote channel to an unattached operation.
    pub async fn recovery_attach(&self, id: i64, channel: u64, actor: u64, reason: &str, now: i64) -> Result<bool, Error> {
        let reason = reason.to_string();
        Ok(self
            .db
            .call(move |c| -> rusqlite::Result<bool> {
                let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let from: Option<String> = tx
                    .query_row("SELECT status FROM tickets WHERE id=?1 AND status IN ('creating','recovery') AND channel IS NULL", [id], |r| r.get(0))
                    .optional()?;
                let Some(from) = from else { return Ok(false) };
                // UNIQUE(channel) rejects a channel already owned by another ticket.
                let n = tx.execute("UPDATE tickets SET status='active',channel=?2 WHERE id=?1 AND status=?3 AND channel IS NULL", params![id, channel.to_string(), from]);
                match n {
                    Ok(1) => {}
                    Ok(_) | Err(rusqlite::Error::SqliteFailure(_, _)) => return Ok(false),
                    Err(e) => return Err(e),
                }
                hist(&tx, id, &from, "active", actor, "operator attach", now)?;
                tx.execute("INSERT INTO ticket_audit(ticket,at,actor,action,reason) VALUES(?1,?2,?3,'attach',?4)", params![id, now, actor.to_string(), reason])?;
                tx.commit()?;
                Ok(true)
            })
            .await?)
    }
    /// Audited operator action: free the slot of an operation proven (by the caller) not
    /// to own a remote channel. Only from unverified create states or `recovery`.
    pub async fn recovery_release(&self, id: i64, actor: u64, reason: &str, now: i64) -> Result<bool, Error> {
        let reason = reason.to_string();
        Ok(self
            .db
            .call(move |c| -> rusqlite::Result<bool> {
                let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let from: Option<String> = tx
                    .query_row("SELECT status FROM tickets WHERE id=?1 AND ((status='creating' AND channel IS NULL) OR status='recovery')", [id], |r| r.get(0))
                    .optional()?;
                let Some(from) = from else { return Ok(false) };
                tx.execute("UPDATE tickets SET status='released',lease_until=0 WHERE id=?1 AND status=?2", params![id, from])?;
                hist(&tx, id, &from, "released", actor, "operator release", now)?;
                tx.execute("INSERT INTO ticket_audit(ticket,at,actor,action,reason) VALUES(?1,?2,?3,'release',?4)", params![id, now, actor.to_string(), reason])?;
                tx.commit()?;
                Ok(true)
            })
            .await?)
    }
    pub async fn audit(&self, id: i64) -> Result<Vec<AuditEntry>, Error> {
        Ok(self
            .db
            .call(move |c| -> rusqlite::Result<Vec<AuditEntry>> {
                let mut s = c.prepare("SELECT actor,action,reason,at FROM ticket_audit WHERE ticket=?1 ORDER BY id")?;
                let rows = s
                    .query_map([id], |r| Ok(AuditEntry { actor: r.get::<_, String>(0)?.parse().unwrap_or(0), action: r.get(1)?, reason: r.get(2)?, at: r.get(3)? }))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }
    #[cfg(test)]
    pub async fn force_status_for_test(&self, id: i64, status: &str) -> Result<(), Error> {
        let status = status.to_string();
        self.db
            .call(move |c| -> rusqlite::Result<()> {
                c.execute("UPDATE tickets SET status=?2 WHERE id=?1", params![id, status])?;
                Ok(())
            })
            .await?;
        Ok(())
    }
}
