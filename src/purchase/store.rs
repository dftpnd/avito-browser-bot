use crate::purchase::model::{Assessment, Status, Task, Verdict};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
pub struct Store {
    pub(crate) db: Connection,
}
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(10))?;
        db.execute_batch("PRAGMA foreign_keys=ON;
   CREATE TABLE IF NOT EXISTS work_jobs(id INTEGER PRIMARY KEY,kind TEXT NOT NULL,payload TEXT NOT NULL,priority INTEGER NOT NULL,status TEXT NOT NULL DEFAULT 'queued',available_at INTEGER NOT NULL,created INTEGER NOT NULL,error TEXT);
   CREATE UNIQUE INDEX IF NOT EXISTS work_jobs_identity ON work_jobs(kind,payload);
   CREATE TABLE IF NOT EXISTS work_settings(key TEXT PRIMARY KEY,payload TEXT NOT NULL);
   CREATE TABLE IF NOT EXISTS chat_sync(chat_id TEXT PRIMARY KEY,complete INTEGER NOT NULL DEFAULT 0,last_synced INTEGER NOT NULL DEFAULT 0,head TEXT NOT NULL DEFAULT '[]');
   CREATE TABLE IF NOT EXISTS outgoing_messages(id INTEGER PRIMARY KEY,draft_id INTEGER NOT NULL UNIQUE,chat_id TEXT NOT NULL,text TEXT NOT NULL,random_id TEXT NOT NULL UNIQUE,status TEXT NOT NULL,history_key TEXT NOT NULL,task_version INTEGER NOT NULL,created INTEGER NOT NULL,message_id TEXT);
   CREATE UNIQUE INDEX IF NOT EXISTS outgoing_one_per_chat ON outgoing_messages(chat_id) WHERE status IN ('queued','dispatching','unknown','sent');
   CREATE UNIQUE INDEX IF NOT EXISTS outgoing_one_writer ON outgoing_messages((1)) WHERE status='dispatching';
   CREATE TABLE IF NOT EXISTS trusted_sellers(id TEXT PRIMARY KEY,verified_at INTEGER NOT NULL);
   CREATE TABLE IF NOT EXISTS purchase_tasks(id INTEGER PRIMARY KEY,version INTEGER NOT NULL,payload TEXT NOT NULL);
   CREATE TABLE IF NOT EXISTS task_versions(task_id INTEGER NOT NULL,version INTEGER NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(task_id,version));
   CREATE TABLE IF NOT EXISTS archived_chats(id TEXT PRIMARY KEY,payload TEXT NOT NULL,complete INTEGER NOT NULL,observed_at INTEGER NOT NULL);
   CREATE TABLE IF NOT EXISTS archived_messages(chat_id TEXT NOT NULL,message_id TEXT NOT NULL,payload TEXT NOT NULL,created REAL,observed_at INTEGER NOT NULL,PRIMARY KEY(chat_id,message_id));
   CREATE TABLE IF NOT EXISTS message_versions(chat_id TEXT NOT NULL,message_id TEXT NOT NULL,payload TEXT NOT NULL,observed_at INTEGER NOT NULL,PRIMARY KEY(chat_id,message_id,payload));
   CREATE TABLE IF NOT EXISTS archived_attachments(chat_id TEXT NOT NULL,message_id TEXT NOT NULL,reference TEXT NOT NULL,payload TEXT NOT NULL,local_path TEXT,status TEXT NOT NULL DEFAULT 'pending',PRIMARY KEY(chat_id,message_id,reference));
   CREATE TABLE IF NOT EXISTS attachment_blobs(chat_id TEXT NOT NULL,message_id TEXT NOT NULL,reference TEXT NOT NULL,digest TEXT NOT NULL,path TEXT NOT NULL,mime TEXT NOT NULL,size INTEGER NOT NULL,downloaded_at INTEGER NOT NULL,PRIMARY KEY(chat_id,message_id,reference));
   CREATE TABLE IF NOT EXISTS offer_details(id TEXT PRIMARY KEY,payload TEXT NOT NULL,observed_at INTEGER NOT NULL);
   CREATE TABLE IF NOT EXISTS task_offers(task_id INTEGER NOT NULL,listing_id TEXT NOT NULL,chat_id TEXT,seller_id TEXT,PRIMARY KEY(task_id,listing_id));
   CREATE TABLE IF NOT EXISTS assessments(task_id INTEGER NOT NULL,listing_id TEXT NOT NULL,version INTEGER NOT NULL,assessment TEXT NOT NULL,verdict TEXT NOT NULL,input_key TEXT NOT NULL,observed_at INTEGER NOT NULL,PRIMARY KEY(task_id,listing_id));
   CREATE TABLE IF NOT EXISTS assistant_outbox(id INTEGER PRIMARY KEY,task_id INTEGER NOT NULL,listing_id TEXT NOT NULL,text TEXT NOT NULL,delivered INTEGER NOT NULL DEFAULT 0,created INTEGER NOT NULL);
   CREATE TABLE IF NOT EXISTS model_usage(id INTEGER PRIMARY KEY,created INTEGER NOT NULL,input_tokens INTEGER NOT NULL,output_tokens INTEGER NOT NULL,model TEXT NOT NULL);
   CREATE TABLE IF NOT EXISTS conversation_drafts(id INTEGER PRIMARY KEY,task_id INTEGER NOT NULL,listing_id TEXT NOT NULL,chat_id TEXT NOT NULL,task_version INTEGER NOT NULL,history_key TEXT NOT NULL,text TEXT NOT NULL,status TEXT NOT NULL,created INTEGER NOT NULL);")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self { db })
    }
    pub fn ensure_task(&self) -> Result<()> {
        let task = Task::gpu();
        let payload = serde_json::to_string(&task)?;
        self.db.execute(
            "INSERT OR IGNORE INTO purchase_tasks VALUES(1,1,?1)",
            [&payload],
        )?;
        self.db.execute("INSERT OR IGNORE INTO task_versions SELECT id,version,payload FROM purchase_tasks WHERE id=1",[])?;
        Ok(())
    }
    pub fn set_task_pause(&self, paused: bool) -> Result<()> {
        self.ensure_task()?;
        let mut task = self.task()?;
        task.paused = paused;
        self.db.execute(
            "UPDATE purchase_tasks SET payload=?1 WHERE id=1",
            [serde_json::to_string(&task)?],
        )?;
        Ok(())
    }
    pub fn task(&self) -> Result<Task> {
        let payload: Option<String> = self
            .db
            .query_row("SELECT payload FROM purchase_tasks WHERE id=1", [], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(payload
            .map(|p| serde_json::from_str(&p))
            .transpose()?
            .unwrap_or_else(Task::gpu))
    }
    pub fn save_task(&mut self, mut task: Task) -> Result<Task> {
        task.validate()?;
        task.id = 1;
        task.version = self.task()?.version + 1;
        let payload = serde_json::to_string(&task)?;
        let tx = self.db.transaction()?;
        tx.execute("INSERT INTO purchase_tasks VALUES(1,?1,?2) ON CONFLICT(id) DO UPDATE SET version=excluded.version,payload=excluded.payload",params![task.version,payload])?;
        tx.execute(
            "INSERT INTO task_versions VALUES(1,?1,?2)",
            params![task.version, payload],
        )?;
        tx.execute("UPDATE work_jobs SET status='cancelled' WHERE status='queued' AND kind IN ('search','detail','analyse','dispatch','open_chat')",[])?;
        tx.execute(
            "UPDATE outgoing_messages SET status='outdated' WHERE status='queued'",
            [],
        )?;
        tx.execute(
            "UPDATE conversation_drafts SET status='outdated' WHERE status='draft'",
            [],
        )?;
        tx.commit()?;
        Ok(task)
    }
    pub fn count(&self, table: &str) -> Result<i64> {
        ensure!(
            [
                "archived_chats",
                "archived_messages",
                "archived_attachments",
                "offer_details",
                "model_usage",
                "assistant_outbox",
                "conversation_drafts"
            ]
            .contains(&table),
            "Invalid table"
        );
        Ok(self
            .db
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
    }
    pub fn record_assessment(&mut self, id: &str, a: &Assessment, key: &str) -> Result<Verdict> {
        let task = self.task()?;
        let mut effective = a.clone();
        let verified:Option<i64>=self.db.query_row("SELECT t.verified_at FROM trusted_sellers t JOIN task_offers o ON o.seller_id=t.id WHERE o.task_id=1 AND o.listing_id=?1",[id],|r|r.get(0)).optional()?;
        if let Some(at) = verified {
            effective.facts.insert(
                "shop_verified_by_owner".into(),
                super::model::Fact {
                    value: true.into(),
                    origin: super::model::Origin::Owner,
                    reference: "owner".into(),
                    quote: "Подтверждено владельцем".into(),
                    observed_at: at,
                    contradiction: false,
                },
            );
        }
        let verdict = super::model::evaluate(&task, &effective, now());
        let tx = self.db.transaction()?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT verdict FROM assessments WHERE task_id=1 AND listing_id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let was_match = previous
            .as_ref()
            .and_then(|p| serde_json::from_str::<Verdict>(p).ok())
            .is_some_and(|v| v.status == Status::Matched);
        tx.execute("INSERT INTO assessments VALUES(1,?1,?2,?3,?4,?5,?6) ON CONFLICT(task_id,listing_id) DO UPDATE SET version=excluded.version,assessment=excluded.assessment,verdict=excluded.verdict,input_key=excluded.input_key,observed_at=excluded.observed_at",params![id,task.version,serde_json::to_string(a)?,serde_json::to_string(&verdict)?,key,now()])?;
        if (verdict.status == Status::Matched) != was_match {
            let mut text = if verdict.status == Status::Matched {
                format!(
                    "✅ Совпадение: предложение {id}\nГруппа приоритета: {}\n{}\n\nПодтверждения:\n{}",
                    verdict.tier,
                    a.explanation,
                    a.facts
                        .iter()
                        .take(8)
                        .map(|(key, f)| format!(
                            "{}: {} · источник {}\n«{}»",
                            key,
                            f.value,
                            f.reference,
                            f.quote.chars().take(150).collect::<String>()
                        ))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            } else {
                format!(
                    "⚠️ Предложение {id} больше не соответствует требованиям:\n{}",
                    verdict.reasons.join("\n")
                )
            };
            let listing: Option<String> = tx
                .query_row("SELECT payload FROM listings WHERE id=?1", [id], |r| {
                    r.get(0)
                })
                .optional()?;
            if let Some(listing) = listing {
                let listing: serde_json::Value = serde_json::from_str(&listing)?;
                if let Some(url) = listing["url"].as_str() {
                    text.push_str(&format!("\n{url}"));
                }
            }
            let text: String = text.chars().take(3900).collect();
            tx.execute(
                "INSERT INTO assistant_outbox(task_id,listing_id,text,created) VALUES(1,?1,?2,?3)",
                params![id, text, now()],
            )?;
        }
        tx.commit()?;
        Ok(verdict)
    }
    pub fn pending(&self) -> Result<Vec<(i64, String)>> {
        let mut s = self.db.prepare(
            "SELECT id,text FROM assistant_outbox WHERE delivered=0 ORDER BY id LIMIT 10",
        )?;
        Ok(s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn acknowledge(&self, id: i64) -> Result<()> {
        self.db
            .execute("UPDATE assistant_outbox SET delivered=1 WHERE id=?1", [id])?;
        Ok(())
    }
    pub fn recalculate(&mut self) -> Result<()> {
        let rows = {
            let mut s = self.db.prepare(
                "SELECT listing_id,assessment,input_key FROM assessments WHERE task_id=1",
            )?;
            s.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (id, a, key) in rows {
            self.record_assessment(&id, &serde_json::from_str(&a)?, &key)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::purchase::model::{Fact, Origin};
    #[test]
    fn match_notifications_are_durable_and_not_repeated() {
        let dir = std::env::temp_dir().join(format!("avito-match-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("match.sqlite");
        let _base = crate::adapters::sqlite::SqliteRepository::open(&path).unwrap();
        let mut store = Store::open(&path).unwrap();
        let mut task = Task::gpu();
        task.approved = true;
        store.save_task(task).unwrap();
        let mut a = Assessment::default();
        for rule in store.task().unwrap().rules {
            a.facts.insert(
                rule.key.clone(),
                Fact {
                    value: if rule.key == "edition" {
                        "workstation".into()
                    } else {
                        rule.expected
                    },
                    origin: Origin::Seller,
                    reference: "seller-message".into(),
                    quote: "confirmed".into(),
                    observed_at: now(),
                    contradiction: false,
                },
            );
        }
        a.facts.insert(
            "availability".into(),
            Fact {
                value: "in_stock".into(),
                origin: Origin::Seller,
                reference: "seller-message".into(),
                quote: "confirmed".into(),
                observed_at: now(),
                contradiction: false,
            },
        );
        assert_eq!(
            store.record_assessment("123", &a, "key").unwrap().status,
            Status::Matched
        );
        store.record_assessment("123", &a, "key").unwrap();
        assert_eq!(store.pending().unwrap().len(), 1);
        let event = store.pending().unwrap()[0].0;
        store.acknowledge(event).unwrap();
        store.recalculate().unwrap();
        assert!(store.pending().unwrap().is_empty());
        a.facts.get_mut("edition").unwrap().value = "server".into();
        a.facts.get_mut("edition").unwrap().contradiction = true;
        assert_eq!(
            store
                .record_assessment("123", &a, "changed")
                .unwrap()
                .status,
            Status::Rejected
        );
        assert_eq!(store.pending().unwrap().len(), 1);
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.pending().unwrap().len(), 1);
        drop(store);
        drop(_base);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
