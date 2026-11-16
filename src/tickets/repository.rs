use super::policy::Kind;
use crate::Error;
use rusqlite::{params, OptionalExtension};
use std::sync::Arc;
use tokio_rusqlite::Connection;

#[derive(Clone)]
pub struct Repository {
    db: Arc<Connection>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub id: i64,
    pub operation: String,
    pub guild: u64,
    pub opener: u64,
    pub kind: Kind,
    pub status: String,
    pub channel: Option<u64>,
    pub greeting: String,
}
#[derive(Clone, Copy, Debug)]
pub struct Panel {
    pub guild: u64,
    pub channel: u64,
    pub message: u64,
    pub bot: u64,
}
impl Repository {
    pub async fn save_panel(&self, p: Panel) -> Result<(), Error> {
        self.db
            .call(move |c| -> rusqlite::Result<()> {
                let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                tx.execute(
                    "UPDATE ticket_panels SET active=0 WHERE guild=?1",
                    [p.guild.to_string()],
                )?;
                tx.execute(
                    "INSERT INTO ticket_panels(guild,channel,message,bot) VALUES(?1,?2,?3,?4)",
                    params![
                        p.guild.to_string(),
                        p.channel.to_string(),
                        p.message.to_string(),
                        p.bot.to_string()
                    ],
                )?;
                tx.commit()?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    pub async fn trusted_panel(&self, p: Panel) -> Result<bool, Error> {
        Ok(self.db.call(move |c| -> rusqlite::Result<bool> { c.query_row("SELECT EXISTS(SELECT 1 FROM ticket_panels WHERE guild=?1 AND channel=?2 AND message=?3 AND bot=?4 AND active=1)",params![p.guild.to_string(),p.channel.to_string(),p.message.to_string(),p.bot.to_string()],|r|r.get(0)) }).await?)
    }
    pub async fn claim(&self, id: i64) -> Result<bool, Error> {
        Ok(self
            .db
            .call(move |c| -> rusqlite::Result<bool> {
                Ok(c.execute(
                    "UPDATE tickets SET status='creating' WHERE id=?1 AND status='reserved'",
                    [id],
                )? == 1)
            })
            .await?)
    }
    pub async fn attach(&self, id: i64, channel: u64) -> Result<(), Error> {
        self.db.call(move |c| -> rusqlite::Result<()> {
   let changed=c.execute("UPDATE tickets SET status='active',channel=?2 WHERE id=?1 AND status='creating'",params![id,channel.to_string()])?;
   if changed!=1 { return Err(rusqlite::Error::InvalidQuery); } Ok(())
  }).await?;
        Ok(())
    }
    pub async fn failed(&self, id: i64) -> Result<(), Error> {
        self.db.call(move |c| -> rusqlite::Result<()> { if c.execute("UPDATE tickets SET status='failed' WHERE id=?1 AND status='creating' AND channel IS NULL",[id])?!=1 { return Err(rusqlite::Error::InvalidQuery); } Ok(()) }).await?;
        Ok(())
    }
    pub async fn greeting_result(&self, id: i64, sent: bool) -> Result<(), Error> {
        self.db
            .call(move |c| -> rusqlite::Result<()> {
                c.execute(
                    "UPDATE tickets SET greeting=?2 WHERE id=?1 AND status='active'",
                    params![id, if sent { "sent" } else { "failed" }],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    pub async fn open(path: &str) -> Result<Self, Error> {
        let db = if path == ":memory:" {
            Connection::open_in_memory().await?
        } else {
            Connection::open(path).await?
        };
        db.call(|c| -> rusqlite::Result<()> {
   c.busy_timeout(std::time::Duration::from_secs(5))?;
   let tx=c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
   tx.execute_batch("CREATE TABLE IF NOT EXISTS ticket_schema(version INTEGER NOT NULL);
    INSERT INTO ticket_schema SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM ticket_schema);")?;
   let version:i64=tx.query_row("SELECT version FROM ticket_schema",[],|r| r.get(0))?;
   if version!=1 { return Err(rusqlite::Error::InvalidQuery); }
   tx.execute_batch("CREATE TABLE IF NOT EXISTS ticket_panels(guild TEXT NOT NULL, channel TEXT NOT NULL, message TEXT PRIMARY KEY, bot TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1);
    CREATE TABLE IF NOT EXISTS tickets(id INTEGER PRIMARY KEY AUTOINCREMENT, operation TEXT NOT NULL UNIQUE DEFAULT (lower(hex(randomblob(16)))), guild TEXT NOT NULL, opener TEXT NOT NULL, kind TEXT NOT NULL CHECK(kind IN ('support','age')), status TEXT NOT NULL CHECK(status IN ('reserved','creating','active','failed','closed')), channel TEXT UNIQUE, greeting TEXT NOT NULL DEFAULT 'pending');
    CREATE UNIQUE INDEX IF NOT EXISTS ticket_active_slot ON tickets(guild,opener,kind) WHERE status IN ('reserved','creating','active');
    CREATE TABLE IF NOT EXISTS ticket_interactions(interaction TEXT PRIMARY KEY, ticket INTEGER NOT NULL REFERENCES tickets(id));")?;
   tx.commit()?; Ok(())
  }).await?;
        Ok(Self { db: Arc::new(db) })
    }
    pub async fn reserve(
        &self,
        interaction: u64,
        guild: u64,
        opener: u64,
        kind: Kind,
    ) -> Result<Ticket, Error> {
        Ok(self.db.call(move |c| -> rusqlite::Result<Ticket> {
   let tx=c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
   let previous:Option<i64>=tx.query_row("SELECT ticket FROM ticket_interactions WHERE interaction=?1",[interaction.to_string()],|r|r.get(0)).optional()?;
   let id=if let Some(id)=previous { id } else {
    let slot:Option<i64>=tx.query_row("SELECT id FROM tickets WHERE guild=?1 AND opener=?2 AND kind=?3 AND status IN ('reserved','creating','active')",params![guild.to_string(),opener.to_string(),kind.key()],|r|r.get(0)).optional()?;
    let id=if let Some(id)=slot { id } else {
     tx.execute("INSERT INTO tickets(guild,opener,kind,status) VALUES(?1,?2,?3,'reserved')",params![guild.to_string(),opener.to_string(),kind.key()])?;
     tx.last_insert_rowid()
    };
    tx.execute("INSERT INTO ticket_interactions(interaction,ticket) VALUES(?1,?2)",params![interaction.to_string(),id])?; id
   };
   let ticket=tx.query_row("SELECT id,guild,opener,kind,status,channel,greeting,operation FROM tickets WHERE id=?1",[id],read_ticket)?;
   tx.commit()?; Ok(ticket)
  }).await?)
    }
}
fn read_ticket(r: &rusqlite::Row<'_>) -> rusqlite::Result<Ticket> {
    fn snowflake(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<u64> {
        let s: String = r.get(i)?;
        s.parse().map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e))
        })
    }
    let channel: Option<String> = r.get(5)?;
    let channel = channel.map(|s| s.parse::<u64>()).transpose().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(Ticket {
        id: r.get(0)?,
        operation: r.get(7)?,
        guild: snowflake(r, 1)?,
        opener: snowflake(r, 2)?,
        kind: if r.get::<_, String>(3)? == "age" {
            Kind::Age
        } else {
            Kind::Support
        },
        status: r.get(4)?,
        channel,
        greeting: r.get(6)?,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn separate_connections_and_restart_preserve_slots_and_opaque_operations() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target-a06-candidate/owned-db-tests");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(format!(
            "{}-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let path = path.to_str().unwrap();
        let a = Repository::open(path).await.unwrap();
        let b = Repository::open(path).await.unwrap();
        let (x, y) = tokio::join!(
            a.reserve(1, 100, u64::MAX, Kind::Support),
            b.reserve(2, 100, u64::MAX, Kind::Support)
        );
        let x = x.unwrap();
        assert_eq!(x.id, y.unwrap().id);
        assert_eq!(x.operation.len(), 32);
        assert!(x.operation.chars().all(|c| c.is_ascii_hexdigit()));
        let (one, two) = tokio::join!(a.claim(x.id), b.claim(x.id));
        assert_ne!(one.unwrap(), two.unwrap());
        let z = b.reserve(3, 100, 999, Kind::Age).await.unwrap();
        assert_ne!(z.operation, x.operation);
        b.claim(z.id).await.unwrap();
        b.attach(z.id, u64::MAX).await.unwrap();
        drop(a);
        drop(b);
        let reopened = Repository::open(path).await.unwrap();
        let x2 = reopened
            .reserve(1, 100, u64::MAX, Kind::Support)
            .await
            .unwrap();
        assert_eq!(x2.status, "creating");
        assert_eq!(x2.operation, x.operation);
        let z2 = reopened.reserve(3, 100, 999, Kind::Age).await.unwrap();
        assert_eq!(z2.status, "active");
        assert_eq!(z2.channel, Some(u64::MAX));
        assert_eq!(z2.greeting, "pending");
        assert!(
            reopened
                .reserve(4, 100, 1000, Kind::Support)
                .await
                .unwrap()
                .id
                > z.id
        );
    }
    #[tokio::test]
    async fn panel_provenance_is_exact() {
        let r = Repository::open(":memory:").await.unwrap();
        let p = Panel {
            guild: 100,
            channel: 200,
            message: u64::MAX,
            bot: 400,
        };
        r.save_panel(p).await.unwrap();
        assert!(r.trusted_panel(p).await.unwrap());
        for altered in [
            Panel { guild: 101, ..p },
            Panel { channel: 201, ..p },
            Panel { message: 1, ..p },
            Panel { bot: 401, ..p },
        ] {
            assert!(!r.trusted_panel(altered).await.unwrap());
        }
        let replacement = Panel { message: 42, ..p };
        r.save_panel(replacement).await.unwrap();
        assert!(
            !r.trusted_panel(p).await.unwrap(),
            "republished panel makes the old panel stale"
        );
        assert!(r.trusted_panel(replacement).await.unwrap());
    }
    #[tokio::test]
    async fn claim_attach_and_known_failure_are_durable_and_concurrent() {
        let r = Repository::open(":memory:").await.unwrap();
        let (a, b) = tokio::join!(
            r.reserve(1, 100, 2, Kind::Support),
            r.reserve(2, 100, 2, Kind::Support)
        );
        let a = a.unwrap();
        assert_eq!(a.id, b.unwrap().id);
        let (a1, a2) = tokio::join!(r.claim(a.id), r.claim(a.id));
        assert_ne!(a1.unwrap(), a2.unwrap());
        assert_eq!(
            r.reserve(3, 100, 2, Kind::Support).await.unwrap().status,
            "creating"
        );
        r.attach(a.id, u64::MAX).await.unwrap();
        r.greeting_result(a.id, false).await.unwrap();
        let active = r.reserve(1, 100, 2, Kind::Support).await.unwrap();
        assert_eq!(active.channel, Some(u64::MAX));
        assert_eq!(active.greeting, "failed");
        assert_eq!(active.status, "active");
        assert!(r.failed(a.id).await.is_err());
        let b = r.reserve(4, 100, 3, Kind::Support).await.unwrap();
        assert!(r.claim(b.id).await.unwrap());
        r.failed(b.id).await.unwrap();
        assert_eq!(
            r.reserve(4, 100, 3, Kind::Support).await.unwrap().status,
            "failed"
        );
        assert!(r.reserve(5, 100, 3, Kind::Support).await.unwrap().id > b.id);
    }
    #[tokio::test]
    async fn reservation_is_idempotent_and_slots_are_per_type_and_user() {
        let r = Repository::open(":memory:").await.unwrap();
        let a = r
            .reserve(u64::MAX, 100, u64::MAX, Kind::Support)
            .await
            .unwrap();
        assert_eq!(
            a,
            r.reserve(u64::MAX, 100, u64::MAX, Kind::Support)
                .await
                .unwrap()
        );
        assert_eq!(
            a.id,
            r.reserve(2, 100, u64::MAX, Kind::Support).await.unwrap().id
        );
        let b = r.reserve(3, 100, u64::MAX, Kind::Age).await.unwrap();
        let c = r.reserve(4, 100, 5, Kind::Support).await.unwrap();
        assert!(a.id < b.id && b.id < c.id);
        assert_eq!(a.opener, u64::MAX);
        assert_eq!(a.status, "reserved");
    }
}
