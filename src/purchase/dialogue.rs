use super::{
    model::{Assessment, Status},
    store::{Store, now},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
impl Store {
    pub fn history_key(&self, chat: &str) -> Result<String> {
        let mut s=self.db.prepare("SELECT message_id,payload FROM archived_messages WHERE chat_id=?1 ORDER BY created,message_id")?;
        let rows = s
            .query_map([chat], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(serde_json::to_string(&rows)?)
    }
    pub fn draft_questions(&mut self, id: &str) -> Result<String> {
        let task = self.task()?;
        ensure!(task.approved, "Сначала подтвердите требования");
        let(chat,): (String,)=self.db.query_row("SELECT chat_id FROM task_offers WHERE task_id=1 AND listing_id=?1 AND chat_id IS NOT NULL",[id],|r|Ok((r.get(0)?,))).context("Нужна связь объявления с диалогом: /link")?;
        let raw:String=self.db.query_row("SELECT assessment FROM assessments WHERE task_id=1 AND listing_id=?1 AND version=?2",params![id,task.version],|r|r.get(0)).context("Сначала выполните /analyse по актуальным требованиям")?;
        let assessment: Assessment = serde_json::from_str(&raw)?;
        let verdict = super::model::evaluate(&task, &assessment, now());
        ensure!(
            verdict.status != Status::Rejected,
            "Предложение отклонено; писать продавцу не нужно"
        );
        let history = self.history_key(&chat)?;
        let mut questions: Vec<String> = assessment
            .questions
            .into_iter()
            .filter(|q| !q.trim().is_empty() && !history.contains(q))
            .collect();
        questions.dedup();
        ensure!(!questions.is_empty(), "Новых вопросов нет");
        let text = format!(
            "Здравствуйте! Уточните, пожалуйста:\n{}",
            questions
                .iter()
                .enumerate()
                .map(|(i, q)| format!("{}. {}", i + 1, q))
                .collect::<Vec<_>>()
                .join("\n")
        );
        ensure!(
            text.chars().count() <= 3000,
            "Слишком длинный список вопросов"
        );
        let existing:Option<String>=self.db.query_row("SELECT text FROM conversation_drafts WHERE task_id=1 AND listing_id=?1 AND task_version=?2 AND history_key=?3 AND status='draft' LIMIT 1",params![id,task.version,history],|r|r.get(0)).optional()?;
        if let Some(text) = existing {
            return Ok(text);
        }
        self.db.execute("UPDATE conversation_drafts SET status='outdated' WHERE task_id=1 AND listing_id=?1 AND status='draft'",[id])?;
        self.db.execute("INSERT INTO conversation_drafts(task_id,listing_id,chat_id,task_version,history_key,text,status,created) VALUES(1,?1,?2,?3,?4,?5,'draft',?6)",params![id,chat,task.version,history,text,now()])?;
        Ok(text)
    }
    pub fn invalidate_drafts(&self) -> Result<usize> {
        let rows = {
            let mut s=self.db.prepare("SELECT id,chat_id,task_version,history_key FROM conversation_drafts WHERE status='draft'")?;
            s.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u32>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let task = self.task()?;
        let mut changed = 0;
        for (id, chat, version, key) in rows {
            if task.version != version || self.history_key(&chat)? != key {
                changed += self.db.execute(
                    "UPDATE conversation_drafts SET status='outdated' WHERE id=?1",
                    [id],
                )?;
            }
        }
        Ok(changed)
    }
}
