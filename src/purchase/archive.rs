use super::store::{Store, now};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Default, serde::Serialize)]
pub struct ImportSummary {
    pub chats: usize,
    pub messages: usize,
    pub revisions: usize,
    pub attachments: usize,
}
fn urls(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::String(s) if s.starts_with("https://") => output.push(s.clone()),
        Value::Array(a) => {
            for v in a {
                urls(v, output)
            }
        }
        Value::Object(o) => {
            for (_, v) in o {
                urls(v, output)
            }
        }
        _ => {}
    }
}
impl Store {
    pub fn import_archive(&mut self, path: &Path) -> Result<ImportSummary> {
        let root: Value = serde_json::from_slice(&std::fs::read(path)?)?;
        self.import_records(&root)
    }
    pub fn import_records(&mut self, root: &Value) -> Result<ImportSummary> {
        let chats = root["chats"]
            .as_array()
            .context("Archive has no chats array")?;
        let mut report = ImportSummary::default();
        for chat in chats {
            let id = chat["channel"]["channelId"]
                .as_str()
                .context("Missing channel ID")?;
            let tx = self.db.transaction()?;
            report.chats+=tx.execute("INSERT INTO archived_chats VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,complete=MAX(archived_chats.complete,excluded.complete),observed_at=excluded.observed_at",params![id,serde_json::to_string(&chat["channel"])?,chat["complete"].as_bool().unwrap_or(false),now()])?;
            for message in chat["messages"]
                .as_array()
                .context("Missing messages array")?
            {
                let mid = message["id"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| message["id"].as_i64().map(|v| v.to_string()))
                    .context("Missing message ID")?;
                let payload = serde_json::to_string(message)?;
                let previous: Option<String> = tx
                    .query_row(
                        "SELECT payload FROM archived_messages WHERE chat_id=?1 AND message_id=?2",
                        params![id, mid],
                        |r| r.get(0),
                    )
                    .optional()?;
                if previous.as_ref() != Some(&payload) {
                    report.revisions += tx.execute(
                        "INSERT OR IGNORE INTO message_versions VALUES(?1,?2,?3,?4)",
                        params![id, mid, payload, now()],
                    )?;
                    tx.execute("INSERT INTO archived_messages VALUES(?1,?2,?3,?4,?5) ON CONFLICT(chat_id,message_id) DO UPDATE SET payload=excluded.payload,created=excluded.created,observed_at=excluded.observed_at",params![id,mid,payload,message["created"].as_f64(),now()])?;
                    if previous.is_none() {
                        report.messages += 1;
                    }
                }
                let mut attachments = vec![];
                urls(&message["body"]["chunks"], &mut attachments);
                urls(&message["attachments"], &mut attachments);
                attachments.sort();
                attachments.dedup();
                for url in attachments {
                    report.attachments+=tx.execute("INSERT OR IGNORE INTO archived_attachments(chat_id,message_id,reference,payload) VALUES(?1,?2,?3,?4)",params![id,mid,url,serde_json::to_string(message)?])?;
                }
            }
            tx.commit()?;
        }
        Ok(report)
    }
    pub fn backup(&self, path: &Path) -> Result<()> {
        ensure!(!path.exists(), "Backup already exists");
        self.db.execute(
            "VACUUM INTO ?1",
            [path.to_str().context("Invalid backup path")?],
        )?;
        let check = rusqlite::Connection::open(path)?;
        let integrity: String = check.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        ensure!(integrity == "ok", "Backup integrity failure");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_import_preserves_identical_messages_with_distinct_ids_and_edits() {
        let dir = std::env::temp_dir().join(format!(
            "avito-archive-test-{}-{}",
            std::process::id(),
            now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("source.json");
        let mut root = serde_json::json!({"chats":[{"channel":{"channelId":"chat"},"complete":true,"messages":[{"id":"a","created":1,"body":{"text":"same"}},{"id":"b","created":2,"body":{"text":"same"}}]}]});
        std::fs::write(&path, root.to_string()).unwrap();
        let mut store = Store::open(&dir.join("test.sqlite")).unwrap();
        assert_eq!(store.import_archive(&path).unwrap().messages, 2);
        assert_eq!(store.import_archive(&path).unwrap().messages, 0);
        root["chats"][0]["messages"][0]["body"]["text"] = "edited".into();
        std::fs::write(&path, root.to_string()).unwrap();
        assert_eq!(store.import_archive(&path).unwrap().revisions, 1);
        assert_eq!(store.count("archived_messages").unwrap(), 2);
        store.backup(&dir.join("backup.sqlite")).unwrap();
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
