//! Persistent sequential work queue. Network policy is checked before leasing any Avito action.
use super::{
    store::{Store, now},
    sync::{
        ConversationSource, DispatchResult, MessageSender, SendIntent, message_key,
        next_history_cursor,
    },
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Job {
    Chats {
        before: Option<f64>,
        cycle: i64,
    },
    History {
        chat: String,
        before: Option<f64>,
        cycle: i64,
    },
    Search {
        search: String,
        page: u32,
        limit: u32,
        version: u32,
    },
    Detail {
        listing_id: String,
    },
    Analyse {
        listing_id: String,
    },
    Dispatch {
        outgoing_id: i64,
    },
    OpenChat {
        listing_id: String,
    },
    Attachment {
        chat: String,
        message: String,
        reference: String,
    },
}
impl Job {
    fn kind(&self) -> &str {
        match self {
            Self::Chats { .. } => "chats",
            Self::History { .. } => "history",
            Self::Search { .. } => "search",
            Self::Detail { .. } => "detail",
            Self::Analyse { .. } => "analyse",
            Self::Dispatch { .. } => "dispatch",
            Self::OpenChat { .. } => "open_chat",
            Self::Attachment { .. } => "attachment",
        }
    }
}
#[derive(Debug, Clone)]
pub struct Lease {
    pub id: i64,
    pub job: Job,
}
pub trait CatalogSource {
    fn search_page(&mut self, url: &str, page: u32) -> Result<(Vec<crate::domain::Listing>, bool)>;
    fn detail(&mut self, id: &str, url: &str) -> Result<Value>;
}
pub trait AccessPolicy {
    fn verify(&self) -> Result<()>;
}
pub struct DirectPolicy;
impl AccessPolicy for DirectPolicy {
    fn verify(&self) -> Result<()> {
        crate::adapters::network::require_direct()
    }
}

impl Store {
    pub fn setting(&self, key: &str) -> Result<Option<Value>> {
        let payload: Option<String> = self
            .db
            .query_row(
                "SELECT payload FROM work_settings WHERE key=?1",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        payload.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
    }
    pub fn set_setting(&self, key: &str, value: Value) -> Result<()> {
        self.db.execute("INSERT INTO work_settings VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET payload=excluded.payload",params![key,value.to_string()])?;
        Ok(())
    }
    pub fn enqueue(&self, job: Job, priority: i64) -> Result<i64> {
        self.db.execute("INSERT INTO work_jobs(kind,payload,priority,available_at,created) VALUES(?1,?2,?3,?4,?4) ON CONFLICT(kind,payload) DO UPDATE SET status=CASE WHEN work_jobs.status IN ('done','cancelled') AND work_jobs.kind!='dispatch' THEN 'queued' ELSE work_jobs.status END, available_at=CASE WHEN work_jobs.status IN ('done','cancelled') THEN excluded.available_at ELSE work_jobs.available_at END,priority=MAX(work_jobs.priority,excluded.priority)",params![job.kind(),serde_json::to_string(&job)?,priority,now()])?;
        Ok(self.db.query_row(
            "SELECT id FROM work_jobs WHERE kind=?1 AND payload=?2",
            params![job.kind(), serde_json::to_string(&job)?],
            |r| r.get(0),
        )?)
    }
    pub fn recover_work(&self) -> Result<()> {
        self.db.execute(
            "UPDATE outgoing_messages SET status='unknown' WHERE status='dispatching'",
            [],
        )?;
        self.db.execute(
            "UPDATE work_jobs SET status='queued' WHERE status='running' AND kind!='dispatch'",
            [],
        )?;
        self.db.execute("UPDATE work_jobs SET status='unknown',error='Process stopped during possible dispatch; reconcile first' WHERE status='running' AND kind='dispatch'",[])?;
        Ok(())
    }
    pub fn claim(&mut self, policy: &impl AccessPolicy) -> Result<Option<Lease>> {
        if self.setting("paused")? == Some(Value::Bool(true)) {
            return Ok(None);
        }
        let candidate: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_jobs WHERE status='queued' AND available_at<=?1)",
            [now()],
            |r| r.get(0),
        )?;
        if !candidate {
            return Ok(None);
        }
        policy.verify()?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let next: Option<String> = tx
            .query_row(
                "SELECT payload FROM work_settings WHERE key='next_avito_action'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if next
            .and_then(|v| serde_json::from_str::<i64>(&v).ok())
            .unwrap_or(0)
            > now()
        {
            return Ok(None);
        }
        let row:Option<(i64,String)>=tx.query_row("SELECT id,payload FROM work_jobs WHERE status='queued' AND available_at<=?1 ORDER BY priority DESC,id LIMIT 1",[now()],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((id, payload)) = row else {
            return Ok(None);
        };
        let job: Job = serde_json::from_str(&payload)?;
        tx.execute(
            "UPDATE work_jobs SET status='running' WHERE id=?1 AND status='queued'",
            [id],
        )?;
        let next = now()
            + if matches!(job, Job::Dispatch { .. }) {
                45
            } else {
                20
            };
        tx.execute("INSERT INTO work_settings VALUES('next_avito_action',?1) ON CONFLICT(key) DO UPDATE SET payload=excluded.payload",[next.to_string()])?;
        tx.commit()?;
        Ok(Some(Lease { id, job }))
    }
    pub fn finish(&self, id: i64) -> Result<()> {
        self.db.execute(
            "UPDATE work_jobs SET status='done',error=NULL WHERE id=?1",
            [id],
        )?;
        Ok(())
    }
    pub fn defer_failure(&self, id: i64, error: &str) -> Result<()> {
        self.db.execute(
            "UPDATE work_jobs SET status='error',error=?2 WHERE id=?1",
            params![id, error.chars().take(500).collect::<String>()],
        )?;
        self.set_setting("paused", true.into())?;
        self.set_setting("pause_reason", error.into())?;
        Ok(())
    }
    pub fn sync_summary(&self) -> Result<String> {
        let queued: i64 = self.db.query_row(
            "SELECT count(*) FROM work_jobs WHERE status='queued'",
            [],
            |r| r.get(0),
        )?;
        let done: i64 = self.db.query_row(
            "SELECT count(*) FROM work_jobs WHERE status='done'",
            [],
            |r| r.get(0),
        )?;
        let errors: i64 = self.db.query_row(
            "SELECT count(*) FROM work_jobs WHERE status IN ('error','unknown')",
            [],
            |r| r.get(0),
        )?;
        let complete: i64 =
            self.db
                .query_row("SELECT count(*) FROM chat_sync WHERE complete=1", [], |r| {
                    r.get(0)
                })?;
        let reason = self
            .setting("pause_reason")?
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let waiting: i64 = self.db.query_row(
            "SELECT count(*) FROM outgoing_messages WHERE status='sent'",
            [],
            |r| r.get(0),
        )?;
        let uncertain: i64 = self.db.query_row(
            "SELECT count(*) FROM outgoing_messages WHERE status='unknown'",
            [],
            |r| r.get(0),
        )?;
        let answered: i64 = self.db.query_row(
            "SELECT count(*) FROM outgoing_messages WHERE status='answered'",
            [],
            |r| r.get(0),
        )?;
        Ok(format!(
            "Очередь: {queued} ожидают · {done} выполнено · {errors} ошибок/неопределённых операций\nПолностью синхронизировано диалогов: {complete}\nПауза: {}\nДиалоги: {waiting} ждут ответа · {answered} получили ответ · {uncertain} требуют проверки отправки\n{reason}",
            self.setting("paused")? == Some(true.into())
        ))
    }
    pub fn queue_sync(&self) -> Result<i64> {
        let pending:bool=self.db.query_row("SELECT EXISTS(SELECT 1 FROM work_jobs WHERE kind='chats' AND status IN ('queued','running'))",[],|r|r.get(0))?;
        ensure!(!pending, "Синхронизация списка чатов уже в очереди");
        self.enqueue(
            Job::Chats {
                before: None,
                cycle: now(),
            },
            60,
        )
    }
    pub fn history_head(&self, chat: &str) -> Result<Vec<Value>> {
        let raw: String = self
            .db
            .query_row("SELECT head FROM chat_sync WHERE chat_id=?1", [chat], |r| {
                r.get(0)
            })
            .context("Сначала синхронизируйте диалог")?;
        Ok(serde_json::from_str(&raw)?)
    }
    pub fn queue_dispatch(&mut self, listing_id: &str) -> Result<i64> {
        let excluded: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM offer_exclusions WHERE task_id=1 AND listing_id=?1)",
            [listing_id],
            |r| r.get(0),
        )?;
        ensure!(!excluded, "Предложение исключено владельцем");
        let task = self.task()?;
        ensure!(
            task.approved && !task.paused,
            "Подтвердите требования и запустите задачу перед отправкой"
        );
        self.invalidate_drafts()?;
        let(draft,chat,text,history,version):(i64,String,String,String,u32)=self.db.query_row("SELECT id,chat_id,text,history_key,task_version FROM conversation_drafts WHERE task_id=1 AND listing_id=?1 AND status='draft' ORDER BY id DESC LIMIT 1",[listing_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).context("Нужен актуальный черновик /draft")?;
        let (last, complete): (i64, bool) = self
            .db
            .query_row(
                "SELECT last_synced,complete FROM chat_sync WHERE chat_id=?1",
                [&chat],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .context("Сначала полностью синхронизируйте диалог")?;
        ensure!(
            complete && now() - last <= 300,
            "Нужна полная свежая синхронизация диалога"
        );
        let outstanding:bool=self.db.query_row("SELECT EXISTS(SELECT 1 FROM outgoing_messages WHERE chat_id=?1 AND status IN ('queued','dispatching','unknown','sent'))",[&chat],|r|r.get(0))?;
        ensure!(
            !outstanding,
            "В этом диалоге уже ожидается ответ или проверка отправки"
        );
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO outgoing_messages(draft_id,chat_id,text,random_id,status,history_key,task_version,created) VALUES(?1,?2,?3,lower(hex(randomblob(16))),'queued',?4,?5,?6)",params![draft,chat,text,history,version,now()])?;
        let id = tx.last_insert_rowid();
        let job = Job::Dispatch { outgoing_id: id };
        tx.execute("INSERT INTO work_jobs(kind,payload,priority,available_at,created) VALUES('dispatch',?1,70,?2,?2)",params![serde_json::to_string(&job)?,now()])?;
        tx.commit()?;
        Ok(id)
    }
}

pub fn perform_read(
    store: &mut Store,
    job: &Job,
    source: &mut impl ConversationSource,
) -> Result<()> {
    match job {
        Job::Chats { before, cycle } => {
            let page = source.chats(*before)?;
            for channel in &page.channels {
                let chat = channel["channelId"].as_str().context("Chat ID missing")?;
                store.import_records(
                    &json!({"chats":[{"channel":channel,"messages":[],"complete":false}]}),
                )?;
                let pending:bool=store.db.query_row("SELECT EXISTS(SELECT 1 FROM work_jobs WHERE kind='history' AND status IN ('queued','running') AND json_extract(payload,'$.chat')=?1)",[chat],|r|r.get(0))?;
                if !pending {
                    let active: bool = store.db.query_row(
                        "SELECT EXISTS(SELECT 1 FROM task_offers WHERE task_id=1 AND chat_id=?1)",
                        [chat],
                        |r| r.get(0),
                    )?;
                    store.enqueue(
                        Job::History {
                            chat: chat.into(),
                            before: None,
                            cycle: *cycle,
                        },
                        if active {
                            80
                        } else if before.is_none() {
                            40
                        } else {
                            10
                        },
                    )?;
                }
            }
            if page.has_more {
                let cursor = page
                    .channels
                    .last()
                    .and_then(|c| c["updated"].as_f64())
                    .context("Chat cursor missing")?;
                ensure!(
                    before.is_none_or(|b| cursor < b),
                    "Chat pagination stopped advancing"
                );
                store.enqueue(
                    Job::Chats {
                        before: Some(cursor),
                        cycle: *cycle,
                    },
                    50,
                )?;
            }
        }
        Job::History {
            chat,
            before,
            cycle,
        } => {
            let incremental_key = format!("history_baseline:{chat}:{cycle}");
            let incremental = match store.setting(&incremental_key)? {
                Some(v) => v == true,
                None => {
                    let complete:bool=store.db.query_row("SELECT COALESCE((SELECT complete FROM chat_sync WHERE chat_id=?1),(SELECT complete FROM archived_chats WHERE id=?1),0)",[chat],|r|r.get(0))?;
                    store.set_setting(&incremental_key, complete.into())?;
                    complete
                }
            };
            let page = source.history(chat, *before)?;
            let mut anchored = false;
            if incremental {
                for message in &page.messages {
                    let (id, _) = message_key(message)?;
                    let known:bool=store.db.query_row("SELECT EXISTS(SELECT 1 FROM archived_messages WHERE chat_id=?1 AND message_id=?2)",params![chat,id],|r|r.get(0))?;
                    if known {
                        anchored = true;
                        break;
                    }
                }
            }
            let next = if anchored {
                None
            } else {
                next_history_cursor(&page, *before)?
            };
            let raw: String = store.db.query_row(
                "SELECT payload FROM archived_chats WHERE id=?1",
                [chat],
                |r| r.get(0),
            )?;
            let channel: Value = serde_json::from_str(&raw)?;
            store.import_records(&json!({"chats":[{"channel":channel,"messages":page.messages,"complete":next.is_none()}]}))?;
            if before.is_none() {
                store.db.execute("INSERT INTO chat_sync(chat_id,complete,last_synced,head) VALUES(?1,0,0,?2) ON CONFLICT(chat_id) DO UPDATE SET complete=0,head=excluded.head",params![chat,serde_json::to_string(&page.messages)?])?;
            }
            if let Some(before) = next {
                store.enqueue(
                    Job::History {
                        chat: chat.clone(),
                        before: Some(before),
                        cycle: *cycle,
                    },
                    30,
                )?;
            } else {
                store.db.execute(
                    "UPDATE chat_sync SET complete=1,last_synced=?2 WHERE chat_id=?1",
                    params![chat, now()],
                )?;
                auto_link(store, chat, &channel, &source.self_user_ids())?;
                reconcile(store, chat)?;
                let mut stmt = store
                    .db
                    .prepare("SELECT listing_id FROM task_offers WHERE task_id=1 AND chat_id=?1")?;
                let ids = stmt
                    .query_map([chat], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(stmt);
                let active = !ids.is_empty();
                for listing_id in ids {
                    let has_detail: bool = store.db.query_row(
                        "SELECT EXISTS(SELECT 1 FROM offer_details WHERE id=?1)",
                        [&listing_id],
                        |r| r.get(0),
                    )?;
                    if has_detail {
                        store.enqueue(Job::Analyse { listing_id }, 55)?;
                    } else {
                        store.enqueue(Job::Detail { listing_id }, 45)?;
                    }
                }
                if active && store.setting("monitor_dialogues")? == Some(true.into()) {
                    let id = store.enqueue(
                        Job::History {
                            chat: chat.clone(),
                            before: None,
                            cycle: now() + 180,
                        },
                        45,
                    )?;
                    store.db.execute(
                        "UPDATE work_jobs SET available_at=?2 WHERE id=?1",
                        params![id, now() + 180],
                    )?;
                }
            }
            store.invalidate_drafts()?;
        }
        _ => anyhow::bail!("Not a messenger read job"),
    }
    Ok(())
}
fn auto_link(store: &Store, chat: &str, channel: &Value, self_ids: &[String]) -> Result<()> {
    let aliases = |user: &Value| {
        ["id", "originalId"]
            .iter()
            .filter_map(|key| {
                user[*key]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| user[*key].as_i64().map(|v| v.to_string()))
            })
            .collect::<Vec<_>>()
    };
    let participants = channel["users"].as_array().cloned().unwrap_or_default();
    let own = participants
        .iter()
        .filter(|u| aliases(u).iter().any(|id| self_ids.contains(id)))
        .count();
    let other = participants
        .iter()
        .filter(|u| !aliases(u).iter().any(|id| self_ids.contains(id)))
        .collect::<Vec<_>>();
    let seller = if own == 1 && other.len() == 1 {
        other[0]["id"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| other[0]["id"].as_i64().map(|v| v.to_string()))
    } else {
        None
    };
    if let Some(seller) = &seller {
        store.db.execute("UPDATE task_offers SET seller_id=COALESCE(seller_id,?2) WHERE task_id=1 AND chat_id=?1",params![chat,seller])?;
    }
    let mut stmt = store.db.prepare("SELECT id,payload FROM listings")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    let actions = channel["context"]["value"]["actions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for (id, raw) in rows {
        let listing: Value = serde_json::from_str(&raw)?;
        let suffix = format!("_{id}");
        let matched = actions
            .iter()
            .filter_map(|a| a["uri"].as_str())
            .filter_map(|a| url::Url::parse(a).ok())
            .any(|u| {
                u.path().ends_with(&suffix)
                    || (u.scheme() == "avito"
                        && u.path().contains("/items/")
                        && u.path().ends_with(&format!("/{id}")))
            })
            || channel["context"]["value"]["itemId"].as_str() == Some(&id);
        if matched {
            // Do not guess which user is the seller. Preserve a manually verified sender mapping.
            store.db.execute("INSERT INTO task_offers(task_id,listing_id,chat_id) VALUES(1,?1,?2) ON CONFLICT(task_id,listing_id) DO UPDATE SET chat_id=COALESCE(task_offers.chat_id,excluded.chat_id)",params![id,chat])?;
            if let Some(seller) = &seller {
                store.db.execute("UPDATE task_offers SET seller_id=COALESCE(seller_id,?3) WHERE task_id=1 AND listing_id=?1 AND chat_id=?2",params![id,chat,seller])?;
            }
            let _ = listing;
        }
    }
    Ok(())
}

fn dispatch_inner(store: &mut Store, id: i64, sender: &mut impl MessageSender) -> Result<()> {
    let row:(i64,String,String,String,String,u32,String)=store.db.query_row("SELECT draft_id,chat_id,text,random_id,history_key,task_version,status FROM outgoing_messages WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?;
    let (draft, chat, text, random_id, history, version, status) = row;
    ensure!(status == "queued", "Outgoing request is not dispatchable");
    let task = store.task()?;
    ensure!(
        task.approved && !task.paused && task.version == version,
        "Task changed or is paused"
    );
    ensure!(
        store.history_key(&chat)? == history,
        "Conversation changed; regenerate draft"
    );
    let draft_status: String = store.db.query_row(
        "SELECT status FROM conversation_drafts WHERE id=?1",
        [draft],
        |r| r.get(0),
    )?;
    ensure!(draft_status == "draft", "Draft no longer valid");
    let excluded:bool=store.db.query_row("SELECT EXISTS(SELECT 1 FROM offer_exclusions e JOIN conversation_drafts d ON d.listing_id=e.listing_id AND d.task_id=e.task_id WHERE d.id=?1)",[draft],|r|r.get(0))?;
    ensure!(!excluded, "Предложение исключено владельцем");
    let head = store.history_head(&chat)?;
    let expected_head = head.iter().map(message_key).collect::<Result<Vec<_>>>()?;
    let tx = store
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let unknown: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM outgoing_messages WHERE status IN ('dispatching','unknown'))",
        [],
        |r| r.get(0),
    )?;
    ensure!(!unknown, "Another send outcome is not reconciled");
    let changed = tx.execute(
        "UPDATE outgoing_messages SET status='dispatching' WHERE id=?1 AND status='queued'",
        [id],
    )?;
    ensure!(changed == 1, "Outgoing operation already claimed");
    tx.commit()?;
    let outcome = sender.send_if_unchanged(&SendIntent {
        chat_id: chat.clone(),
        text,
        random_id,
        expected_head,
    });
    match outcome {
        Ok(DispatchResult::Changed { messages }) => {
            store.db.execute(
                "UPDATE outgoing_messages SET status='outdated' WHERE id=?1",
                [id],
            )?;
            store.db.execute(
                "UPDATE conversation_drafts SET status='outdated' WHERE id=?1",
                [draft],
            )?;
            let channel: String = store.db.query_row(
                "SELECT payload FROM archived_chats WHERE id=?1",
                [&chat],
                |r| r.get(0),
            )?;
            store.import_records(&json!({"chats":[{"channel":serde_json::from_str::<Value>(&channel)?,"messages":messages,"complete":false}]}))?;
            store.enqueue(
                Job::History {
                    chat,
                    before: None,
                    cycle: now(),
                },
                80,
            )?;
        }
        Ok(DispatchResult::Sent { message }) => {
            let message_id = message["id"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| message["id"].as_i64().map(|v| v.to_string()))
                .context("Sent message ID missing; outcome unknown")?;
            let channel: String = store.db.query_row(
                "SELECT payload FROM archived_chats WHERE id=?1",
                [&chat],
                |r| r.get(0),
            )?;
            store.import_records(&json!({"chats":[{"channel":serde_json::from_str::<Value>(&channel)?,"messages":[message],"complete":false}]}))?;
            let tx = store.db.transaction()?;
            tx.execute(
                "UPDATE outgoing_messages SET status='sent',message_id=?2 WHERE id=?1",
                params![id, message_id],
            )?;
            tx.execute(
                "UPDATE conversation_drafts SET status='sent' WHERE id=?1",
                [draft],
            )?;
            tx.commit()?;
        }
        Err(error) => {
            store.db.execute(
                "UPDATE outgoing_messages SET status='unknown' WHERE id=?1",
                [id],
            )?;
            return Err(error);
        }
    }
    Ok(())
}

pub fn perform_dispatch(store: &mut Store, id: i64, sender: &mut impl MessageSender) -> Result<()> {
    let result = dispatch_inner(store, id, sender);
    if result.is_err() {
        store.db.execute(
            "UPDATE outgoing_messages SET status='unknown' WHERE id=?1 AND status='dispatching'",
            [id],
        )?;
    }
    result
}

fn reconcile(store: &Store, chat: &str) -> Result<()> {
    let rows = {
        let mut s=store.db.prepare("SELECT id,random_id,status,CAST(COALESCE((SELECT created FROM archived_messages m WHERE m.chat_id=outgoing_messages.chat_id AND m.message_id=outgoing_messages.message_id),created) AS REAL) FROM outgoing_messages WHERE chat_id=?1 AND status IN ('unknown','sent')")?;
        s.query_map([chat], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, f64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let seller: Option<String> = store
        .db
        .query_row(
            "SELECT seller_id FROM task_offers WHERE chat_id=?1 AND seller_id IS NOT NULL LIMIT 1",
            [chat],
            |r| r.get(0),
        )
        .optional()?;
    for (id, random, status, created) in rows {
        let found:Option<String>=store.db.query_row("SELECT message_id FROM archived_messages WHERE chat_id=?1 AND json_extract(payload,'$.body.randomId')=?2",params![chat,random],|r|r.get(0)).optional()?;
        if let Some(message) = found {
            store.db.execute(
                "UPDATE outgoing_messages SET status='sent',message_id=?2 WHERE id=?1",
                params![id, message],
            )?;
        }
        if status == "sent"
            && let Some(seller) = &seller
        {
            let replied:bool=store.db.query_row("SELECT EXISTS(SELECT 1 FROM archived_messages WHERE chat_id=?1 AND CAST(json_extract(payload,'$.fromUid') AS TEXT)=?2 AND created>?3 AND json_extract(payload,'$.type')='text')",params![chat,seller,created],|r|r.get(0))?;
            if replied {
                store.db.execute(
                    "UPDATE outgoing_messages SET status='answered' WHERE id=?1",
                    [id],
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::purchase::{
        model::{Assessment, Task},
        sync::{ChatPage, HistoryPage},
    };
    struct Fixture {
        store: Store,
        path: std::path::PathBuf,
    }
    impl Fixture {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("avito-jobs-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            let db = path.join("test.sqlite");
            crate::adapters::sqlite::SqliteRepository::open(&db).unwrap();
            let mut store = Store::open(&db).unwrap();
            let mut task = Task::gpu();
            task.approved = true;
            task.paused = false;
            store.save_task(task).unwrap();
            let old = json!({"id":"old","created":now()-10,"type":"text","fromUid":"seller","body":{"text":"hello"}});
            store.import_records(&json!({"chats":[{"channel":{"channelId":"chat"},"messages":[old],"complete":true}]})).unwrap();
            store
                .db
                .execute(
                    "INSERT INTO chat_sync VALUES('chat',1,?1,?2)",
                    params![now(), json!([old]).to_string()],
                )
                .unwrap();
            store
                .db
                .execute(
                    "INSERT INTO task_offers VALUES(1,'123','chat','seller')",
                    [],
                )
                .unwrap();
            let a = Assessment {
                questions: vec!["Какая версия карты?".into()],
                ..Assessment::default()
            };
            let key=json!({"version":store.task().unwrap().version,"chat_id":"chat","seller_id":"seller","history_key":store.history_key("chat").unwrap()}).to_string();
            store.record_assessment("123", &a, &key).unwrap();
            store.draft_questions("123").unwrap();
            Self { store, path }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
    struct Allow;
    impl AccessPolicy for Allow {
        fn verify(&self) -> Result<()> {
            Ok(())
        }
    }
    struct Deny;
    impl AccessPolicy for Deny {
        fn verify(&self) -> Result<()> {
            anyhow::bail!("VPN not verified")
        }
    }
    struct Sender {
        calls: usize,
        changed: bool,
        fail: bool,
    }
    impl MessageSender for Sender {
        fn send_if_unchanged(&mut self, _: &SendIntent) -> Result<DispatchResult> {
            self.calls += 1;
            if self.fail {
                anyhow::bail!("timeout")
            }
            if self.changed {
                Ok(DispatchResult::Changed {
                    messages: vec![
                        json!({"id":"manual","created":now(),"type":"text","body":{"text":"already asked manually"}}),
                    ],
                })
            } else {
                Ok(DispatchResult::Sent {
                    message: json!({"id":"sent","created":now(),"type":"text","body":{"text":"question"}}),
                })
            }
        }
    }
    #[test]
    fn network_denial_never_leases_work_and_next_process_obeys_cooldown() {
        let mut f = Fixture::new("policy");
        f.store
            .enqueue(
                Job::Chats {
                    before: None,
                    cycle: 1,
                },
                1,
            )
            .unwrap();
        assert!(f.store.claim(&Deny).is_err());
        assert_eq!(
            f.store
                .db
                .query_row("SELECT status FROM work_jobs", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "queued"
        );
        let lease = f.store.claim(&Allow).unwrap().unwrap();
        f.store.finish(lease.id).unwrap();
        f.store
            .enqueue(
                Job::Chats {
                    before: None,
                    cycle: 2,
                },
                1,
            )
            .unwrap();
        assert!(f.store.claim(&Allow).unwrap().is_none());
    }
    #[test]
    fn unknown_send_is_not_retried_after_restart() {
        let mut f = Fixture::new("unknown");
        let id = f.store.queue_dispatch("123").unwrap();
        let mut sender = Sender {
            calls: 0,
            changed: false,
            fail: true,
        };
        assert!(perform_dispatch(&mut f.store, id, &mut sender).is_err());
        f.store.recover_work().unwrap();
        assert!(perform_dispatch(&mut f.store, id, &mut sender).is_err());
        assert_eq!(sender.calls, 1);
        assert_eq!(
            f.store
                .db
                .query_row(
                    "SELECT status FROM outgoing_messages WHERE id=?1",
                    [id],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "unknown"
        );
    }
    #[test]
    fn changed_history_cancels_send_and_schedules_sync() {
        let mut f = Fixture::new("changed");
        let id = f.store.queue_dispatch("123").unwrap();
        let mut sender = Sender {
            calls: 0,
            changed: true,
            fail: false,
        };
        perform_dispatch(&mut f.store, id, &mut sender).unwrap();
        assert_eq!(sender.calls, 1);
        assert_eq!(
            f.store
                .db
                .query_row(
                    "SELECT status FROM outgoing_messages WHERE id=?1",
                    [id],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "outdated"
        );
        assert_eq!(f.store.count("archived_messages").unwrap(), 2);
    }
    #[test]
    fn manual_message_and_requirement_change_require_a_new_question_analysis() {
        let mut f = Fixture::new("fresh-analysis");
        f.store.import_records(&json!({"chats":[{"channel":{"channelId":"chat"},"messages":[{"id":"manual-new","created":now(),"type":"text","fromUid":"buyer","body":{"text":"Какой срок доставки?"}}]}]})).unwrap();
        assert!(f.store.draft_questions("123").is_err());

        let a = Assessment {
            questions: vec!["Какой срок доставки?".into(), "Какая версия карты?".into()],
            ..Assessment::default()
        };
        let key=json!({"version":f.store.task().unwrap().version,"chat_id":"chat","seller_id":"seller","history_key":f.store.history_key("chat").unwrap()}).to_string();
        f.store.record_assessment("123", &a, &key).unwrap();
        let draft = f.store.draft_questions("123").unwrap();
        assert!(!draft.contains("Какой срок доставки?"));
        assert!(draft.contains("Какая версия карты?"));

        f.store.save_task(f.store.task().unwrap()).unwrap();
        // Recalculating the verdict must not make an old model analysis current.
        f.store.recalculate().unwrap();
        assert!(f.store.draft_questions("123").is_err());
    }
    #[test]
    fn excluding_an_offer_cancels_queued_sending_and_blocks_new_drafts() {
        let mut f = Fixture::new("exclude");
        let outgoing = f.store.queue_dispatch("123").unwrap();
        let version = f.store.task().unwrap().version;
        f.store.exclude_offer("123", version, true).unwrap();
        let status: String = f
            .store
            .db
            .query_row(
                "SELECT status FROM work_jobs WHERE kind='dispatch'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "cancelled");
        assert!(f.store.draft_questions("123").is_err());
        let mut sender = Sender {
            calls: 0,
            changed: false,
            fail: false,
        };
        assert!(perform_dispatch(&mut f.store, outgoing, &mut sender).is_err());
        assert_eq!(sender.calls, 0);
        assert!(f.store.exclude_offer("123", version - 1, false).is_err());
    }
    #[test]
    fn changing_requirements_invalidates_the_queued_message() {
        let mut f = Fixture::new("requirements");
        let id = f.store.queue_dispatch("123").unwrap();
        let mut task = f.store.task().unwrap();
        task.approved = false;
        f.store.save_task(task).unwrap();
        let mut sender = Sender {
            calls: 0,
            changed: false,
            fail: false,
        };
        assert!(perform_dispatch(&mut f.store, id, &mut sender).is_err());
        assert_eq!(sender.calls, 0);
    }
    struct ReadSource {
        calls: usize,
    }
    impl ConversationSource for ReadSource {
        fn chats(&mut self, _: Option<f64>) -> Result<ChatPage> {
            unreachable!()
        }
        fn history(&mut self, _: &str, _: Option<f64>) -> Result<HistoryPage> {
            self.calls += 1;
            Ok(HistoryPage {
                messages: vec![
                    json!({"id":"new","created":now(),"type":"text","body":{"text":"reply"}}),
                    json!({"id":"old","created":now()-10,"type":"text","fromUid":"seller","body":{"text":"hello"}}),
                ],
                has_more: Some(true),
            })
        }
    }
    #[test]
    fn incremental_sync_stops_at_known_complete_baseline() {
        let mut f = Fixture::new("anchor");
        let mut source = ReadSource { calls: 0 };
        perform_read(
            &mut f.store,
            &Job::History {
                chat: "chat".into(),
                before: None,
                cycle: 42,
            },
            &mut source,
        )
        .unwrap();
        assert_eq!(source.calls, 1);
        assert_eq!(f.store.count("archived_messages").unwrap(), 2);
        assert!(
            f.store
                .db
                .query_row(
                    "SELECT complete FROM chat_sync WHERE chat_id='chat'",
                    [],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap()
        );
    }
}
