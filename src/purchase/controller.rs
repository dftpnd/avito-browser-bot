use super::{
    deepseek::{DeepSeek, FactExtractor, Source},
    model::{Origin, Status, Task, Verdict},
    store::{Store, now},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::path::Path;

pub trait Assistant {
    fn handle(&mut self, text: &str) -> Result<Option<String>>;
    fn pending(&mut self) -> Result<Vec<(i64, String)>>;
    fn acknowledge(&mut self, id: i64) -> Result<()>;
    fn allows_avito(&self) -> bool {
        true
    }
}
pub struct NoAssistant;
impl Assistant for NoAssistant {
    fn handle(&mut self, _: &str) -> Result<Option<String>> {
        Ok(None)
    }
    fn pending(&mut self) -> Result<Vec<(i64, String)>> {
        Ok(vec![])
    }
    fn acknowledge(&mut self, _: i64) -> Result<()> {
        Ok(())
    }
}
pub struct Controller {
    pub store: Store,
    model: Option<DeepSeek>,
    messenger: Option<crate::adapters::messenger::Messenger>,
    catalog: Option<crate::adapters::browser::BrowserSource>,
    attachment_dir: std::path::PathBuf,
}
impl Controller {
    pub fn new(path: &Path) -> Result<Self> {
        let store = Store::open(path)?;
        store.ensure_task()?;
        store.recover_work()?;
        Ok(Self {
            store,
            model: None,
            messenger: None,
            catalog: None,
            attachment_dir: path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("attachments"),
        })
    }

    fn listing_count(&self) -> Result<i64> {
        Ok(self
            .store
            .db
            .query_row("SELECT count(*) FROM listings", [], |r| r.get(0))?)
    }
    fn summary(&self) -> Result<String> {
        let task = self.store.task()?;
        let mut counts = [0; 4];
        let mut seller_confirmed = 0;
        let mut stmt = self
            .store
            .db
            .prepare("SELECT verdict,assessment FROM assessments WHERE task_id=1 AND version=?1")?;
        for row in stmt.query_map([task.version], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (raw, assessment) = row?;
            let v: Verdict = serde_json::from_str(&raw)?;
            let assessment: super::model::Assessment = serde_json::from_str(&assessment)?;
            if v.status != Status::Rejected
                && ["model", "availability"].iter().all(|key| {
                    assessment.facts.get(*key).is_some_and(|f| {
                        f.origin == Origin::Seller
                            && !f.contradiction
                            && now() - f.observed_at <= 86400
                    })
                })
            {
                seller_confirmed += 1;
            }
            counts[match v.status {
                Status::Candidate => 0,
                Status::NeedsDetails => 1,
                Status::Rejected => 2,
                Status::Matched => 3,
            }] += 1;
        }
        Ok(format!(
            "📊 {}\nТребования: v{}, {}\nВсего объявлений в базе: {}\nОценено для задачи: {}\nПредложение подтверждено продавцом: {}\nПолностью подходят: {}\nНужно уточнить: {}\nНе подходят: {}\n\nАрхив аккаунта (отдельно от задачи):\nДиалогов: {}\nСообщений: {}\nВложений с метаданными: {}\n\nСчётчики из локальной базы, не количество физических карт.",
            task.title,
            task.version,
            if task.approved {
                "подтверждены"
            } else {
                "черновик"
            },
            self.listing_count()?,
            counts.iter().sum::<i32>(),
            seller_confirmed,
            counts[3],
            counts[1],
            counts[2],
            self.store.count("archived_chats")?,
            self.store.count("archived_messages")?,
            self.store.count("archived_attachments")?
        ))
    }
    fn best(&self, only_matches: bool) -> Result<String> {
        let task = self.store.task()?;
        let mut s=self.store.db.prepare("SELECT a.listing_id,a.verdict,a.assessment,l.payload FROM assessments a LEFT JOIN listings l ON l.id=a.listing_id WHERE a.task_id=1 AND a.version=?1")?;
        let rows = s.query_map([task.version], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        let mut results = vec![];
        for row in rows {
            let (id, v, assessment, payload) = row?;
            let assessment: super::model::Assessment = serde_json::from_str(&assessment)?;
            let v: Verdict = serde_json::from_str(&v)?;
            if v.status == Status::Rejected || (only_matches && v.status != Status::Matched) {
                continue;
            }
            let title = payload
                .and_then(|p| serde_json::from_str::<Value>(&p).ok())
                .and_then(|p| p["title"].as_str().map(str::to_owned))
                .unwrap_or(id.clone());
            results.push((
                (
                    (v.status != Status::Matched) as u8,
                    v.tier,
                    assessment
                        .facts
                        .get("total_price_rub")
                        .filter(|f| {
                            f.origin == Origin::Seller
                                && !f.contradiction
                                && now() - f.observed_at <= 86400
                        })
                        .and_then(|f| f.value.as_i64())
                        .unwrap_or(i64::MAX),
                ),
                format!(
                    "{}\nОбъявление {} · {} · {}\n{}",
                    title,
                    id,
                    if v.status == Status::Matched {
                        "Все обязательные условия подтверждены"
                    } else {
                        "Нужно уточнить"
                    },
                    match v.tier {
                        1 => "в наличии",
                        2 => "заказ без предоплаты",
                        3 => "заказ с частичной предоплатой",
                        _ => "условия не выяснены",
                    },
                    v.reasons.join("; ")
                ),
            ));
        }
        results.sort_by_key(|r| r.0);
        Ok(if results.is_empty() {
            "Оценённых предложений для текущих требований пока нет. /requirements — требования."
                .into()
        } else {
            results
                .into_iter()
                .take(8)
                .map(|r| r.1)
                .collect::<Vec<_>>()
                .join("\n\n")
                .chars()
                .take(3800)
                .collect()
        })
    }
    fn analyse(&mut self, id: &str) -> Result<String> {
        let task = self.store.task()?;
        let detail: Option<(String, i64)> = self
            .store
            .db
            .query_row(
                "SELECT payload,observed_at FROM offer_details WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (detail, observed) = detail.context(
            "Нет подробной карточки. Сначала нужен сбор карточки по прямому маршруту к Авито.",
        )?;
        let mut sources = vec![Source {
            reference: format!("listing:{id}"),
            origin: Origin::Listing,
            observed_at: observed,
            text: super::privacy::redact(&detail),
        }];
        let linked:Option<(String,String)>=self.store.db.query_row("SELECT chat_id,seller_id FROM task_offers WHERE task_id=1 AND listing_id=?1 AND chat_id IS NOT NULL AND seller_id IS NOT NULL",[id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((chat, seller)) = linked {
            let mut stmt=self.store.db.prepare("SELECT payload FROM archived_messages WHERE chat_id=?1 ORDER BY created DESC, message_id DESC")?;
            let mut rows = stmt
                .query_map([chat], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.reverse();
            for raw in rows {
                let m: Value = serde_json::from_str(&raw)?;
                let author = m["fromUid"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| m["fromUid"].as_i64().map(|n| n.to_string()))
                    .unwrap_or_default();
                if m["type"] == "text" {
                    let text = super::privacy::redact(m["body"]["text"].as_str().unwrap_or(""));
                    if !text.is_empty() {
                        sources.push(Source {
                            reference: m["id"].as_str().unwrap_or("").into(),
                            origin: if author == seller {
                                Origin::Seller
                            } else {
                                Origin::Buyer
                            },
                            observed_at: m["created"].as_f64().unwrap_or(0.0) as i64,
                            text,
                        });
                    }
                }
            }
        }
        ensure!(
            sources
                .iter()
                .map(|s| s.text.chars().count())
                .sum::<usize>()
                <= 24000,
            "Слишком большой контекст; нужен отбор относящихся к покупке сообщений"
        );
        let key = serde_json::to_string(&json!({"version":task.version,"sources":sources}))?;
        let cached:bool=self.store.db.query_row("SELECT EXISTS(SELECT 1 FROM assessments WHERE task_id=1 AND listing_id=?1 AND input_key=?2)",params![id,key],|r|r.get(0))?;
        if cached {
            self.store.recalculate()?;
            return Ok("Использована сохранённая оценка; DeepSeek повторно не вызывался.".into());
        }
        let recent: i64 = self.store.db.query_row(
            "SELECT count(*) FROM model_usage WHERE created>?1",
            [now() - 86400],
            |r| r.get(0),
        )?;
        ensure!(recent < 30, "Дневной лимит: 30 оценок DeepSeek");
        if self.model.is_none() {
            self.model = Some(DeepSeek::load(Path::new("config.local.deepseek.json"))?);
        }
        self.store.db.execute("INSERT INTO model_usage(created,input_tokens,output_tokens,model) VALUES(?1,0,0,'pending')",[now()])?;
        let attempt = self.store.db.last_insert_rowid();
        let result = self.model.as_mut().unwrap().extract(&task, &sources)?;
        self.store.db.execute(
            "UPDATE model_usage SET input_tokens=?1,output_tokens=?2,model=?3 WHERE id=?4",
            params![
                result.input_tokens,
                result.output_tokens,
                result.model,
                attempt
            ],
        )?;
        let verdict = self.store.record_assessment(id, &result.assessment, &key)?;
        Ok(format!(
            "Оценка: {:?}\n{}\n{}",
            verdict.status,
            verdict.reasons.join("\n"),
            result.assessment.explanation
        )
        .chars()
        .take(3800)
        .collect())
    }
}
impl Assistant for Controller {
    fn allows_avito(&self) -> bool {
        self.store
            .setting("paused")
            .map(|v| v != Some(true.into()))
            .unwrap_or(false)
            && self.store.task().is_ok_and(|t| !t.paused)
    }
    fn handle(&mut self, text: &str) -> Result<Option<String>> {
        let text = text.trim();
        let (command, argument) = text.split_once(' ').unwrap_or((text, ""));
        let response = match command {
            "/stats" | "📊 Сводка" => self.summary()?,
            "📊" => self.summary()?,
            "/best" | "🏆" => self.best(false)?,
            "/matches" | "✅" => self.best(true)?,
            "/requirements" | "⚙️" => {
                let task = self.store.task()?;
                format!(
                    "{}\nВерсия {} · {}\n\n{}\n\nДля изменения: /task опиши новые условия обычным текстом.\nДля подтверждения: /approve.",
                    task.title,
                    task.version,
                    if task.approved {
                        "подтверждены"
                    } else {
                        "черновик"
                    },
                    task.rules
                        .iter()
                        .map(|r| format!(
                            "• {}: {}{}",
                            r.label,
                            r.expected,
                            if r.mandatory {
                                " (обязательно)"
                            } else {
                                ""
                            }
                        ))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            }
            "/task" => {
                ensure!(
                    !argument.trim().is_empty(),
                    "После /task опиши требования к покупке"
                );
                let recent: i64 = self.store.db.query_row(
                    "SELECT count(*) FROM model_usage WHERE created>?1",
                    [now() - 86400],
                    |r| r.get(0),
                )?;
                ensure!(recent < 30, "Дневной лимит DeepSeek исчерпан");
                if self.model.is_none() {
                    self.model = Some(DeepSeek::load(Path::new("config.local.deepseek.json"))?);
                }
                self.store.db.execute("INSERT INTO model_usage(created,input_tokens,output_tokens,model) VALUES(?1,0,0,'requirements')",[now()])?;
                let task = self
                    .model
                    .as_mut()
                    .unwrap()
                    .propose_task(&self.store.task()?, argument)?;
                self.store.save_task(task)?;
                self.store.recalculate()?;
                "Подготовлен новый черновик. Нажми «⚙️ Требования», проверь условия и отправь /approve, чтобы применить их.".into()
            }
            "/criteria" => {
                let mut t: Task = serde_json::from_str(argument)
                    .map_err(|_| anyhow::anyhow!("Нужен JSON задачи из /requirements"))?;
                t.approved = false;
                t.paused = true;
                let t = self.store.save_task(t)?;
                self.store.recalculate()?;
                format!(
                    "Сохранён черновик v{}. Проверь /requirements, затем /approve.",
                    t.version
                )
            }
            "/approve" => {
                let mut t = self.store.task()?;
                t.approved = true;
                let t = self.store.save_task(t)?;
                self.store.recalculate()?;
                format!(
                    "Требования v{} подтверждены. /stats — статистика.",
                    t.version
                )
            }
            "/pause" | "⏸" => {
                self.store.set_setting("paused", true.into())?;
                self.store.set_task_pause(true)?;
                "Вся очередь Авито приостановлена. /resume — продолжить.".into()
            }
            "/resume" => {
                self.store.set_setting("paused", false.into())?;
                self.store.set_task_pause(false)?;
                self.store.db.execute("UPDATE work_jobs SET status='queued',error=NULL WHERE status='error' AND kind!='dispatch'",[])?;
                "Очередь продолжит работу после подтверждения прямого маршрута. Неопределённые отправки автоматически не повторяются.".into()
            }
            "/sync" => {
                self.store.set_setting("monitor_dialogues", true.into())?;
                if argument.trim().is_empty() {
                    self.store.queue_sync()?;
                } else {
                    let exists: bool = self.store.db.query_row(
                        "SELECT EXISTS(SELECT 1 FROM archived_chats WHERE id=?1)",
                        [argument],
                        |r| r.get(0),
                    )?;
                    ensure!(exists, "Диалог отсутствует в архиве");
                    self.store.enqueue(
                        super::jobs::Job::History {
                            chat: argument.into(),
                            before: None,
                            cycle: now(),
                        },
                        80,
                    )?;
                }
                "Синхронизация поставлена в очередь. /queue — прогресс.".into()
            }
            "/research" => {
                let task = self.store.task()?;
                ensure!(
                    !task.searches.is_empty(),
                    "Добавь запросы в требования задачи"
                );
                let pending:bool=self.store.db.query_row("SELECT EXISTS(SELECT 1 FROM work_jobs WHERE kind='search' AND status IN ('queued','running'))",[],|r|r.get(0))?;
                ensure!(!pending, "Поиск уже в очереди");
                self.store.set_task_pause(false)?;
                for search in task.searches {
                    self.store.enqueue(
                        super::jobs::Job::Search {
                            search,
                            page: 1,
                            limit: 10,
                            version: task.version,
                        },
                        20,
                    )?;
                }
                "Запросы поставлены в очередь. За один шаг просматривается одна страница, до 10 страниц на запрос. /queue — прогресс.".into()
            }
            "/queue" => self.store.sync_summary()?,
            "/attachments" => format!(
                "В очередь добавлено файлов: {} (до 50 за запуск). Приватные/устаревшие ссылки будут отмечены как недоступные.",
                super::attachments::queue_pending(&self.store)?
            ),
            "/outreach" => {
                ensure!(
                    argument == "on" || argument == "off",
                    "/outreach on или /outreach off"
                );
                if argument == "on" {
                    ensure!(
                        self.store.task()?.approved,
                        "Сначала подтвердите требования"
                    );
                }
                self.store
                    .set_setting("outreach", (argument == "on").into())?;
                if argument == "off" {
                    self.store.db.execute(
                        "UPDATE outgoing_messages SET status='cancelled' WHERE status='queued'",
                        [],
                    )?;
                    self.store.db.execute("UPDATE conversation_drafts SET status='outdated' WHERE id IN (SELECT draft_id FROM outgoing_messages WHERE status='cancelled')",[])?;
                    self.store.db.execute("UPDATE work_jobs SET status='cancelled' WHERE status='queued' AND kind IN ('dispatch','open_chat')",[])?;
                }
                "Режим последовательных уточнений обновлён. /queue — состояние.".into()
            }
            "/dismiss_send" => {
                let id:i64=argument.parse().context("/dismiss_send ID — снять неопределённую отправку с автоматики после ручной проверки диалога")?;
                let updated=self.store.db.execute("UPDATE outgoing_messages SET status='dismissed' WHERE id=?1 AND status='unknown'",[id])?;
                ensure!(updated == 1, "Неопределённая операция не найдена");
                "Операция снята с автоматики; сообщение не удалялось и повторно не отправлялось. Перед новым уточнением требуется свежая синхронизация диалога.".into()
            }
            "/send" => {
                let id = self.store.queue_dispatch(argument)?;
                format!(
                    "Уточнение №{id} поставлено в очередь. Перед отправкой история будет проверена заново."
                )
            }
            "/network" | "🛠" => {
                let audit = crate::adapters::network::audit()?;
                format!(
                    "Прямой маршрут Авито: {}\n{}\n\n{}\nОценок DeepSeek: {}",
                    audit.direct,
                    audit.reasons.join("\n"),
                    self.store.sync_summary()?,
                    self.store.count("model_usage")?
                )
            }
            "/dialogs" | "💬" => {
                let mut stmt = self.store.db.prepare(
                    "SELECT id,payload FROM archived_chats ORDER BY observed_at DESC,id LIMIT 8",
                )?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut chats = vec![];
                for (id, raw) in rows {
                    let c: Value = serde_json::from_str(&raw)?;
                    let title = c["context"]["value"]["name"].as_str().unwrap_or("Диалог");
                    chats.push(format!(
                        "{}\n/history {}",
                        super::privacy::redact(title),
                        id
                    ));
                }
                format!(
                    "В архиве {} диалогов и {} сообщений. Это локальный снимок, не свежая синхронизация.\n\n{}",
                    self.store.count("archived_chats")?,
                    self.store.count("archived_messages")?,
                    chats.join("\n\n")
                )
            }
            "/card" => {
                crate::adapters::network::require_direct()?;
                let raw: String = self
                    .store
                    .db
                    .query_row(
                        "SELECT payload FROM listings WHERE id=?1",
                        [argument],
                        |r| r.get(0),
                    )
                    .context("Объявление не найдено в базе")?;
                let listing: crate::domain::Listing = serde_json::from_str(&raw)?;
                let mut browser = crate::adapters::browser::BrowserSource::connect(
                    std::time::Duration::from_secs(20),
                )?;
                let detail = browser.describe(&listing)?;
                self.store.db.execute("INSERT INTO offer_details VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,observed_at=excluded.observed_at",params![listing.id,detail.to_string(),now()])?;
                self.store.db.execute(
                    "INSERT OR IGNORE INTO task_offers(task_id,listing_id) VALUES(1,?1)",
                    [&listing.id],
                )?;
                "Карточка сохранена. /analyse ID — оценить её по требованиям.".into()
            }
            "/link" => {
                let args: Vec<&str> = argument.split_whitespace().collect();
                ensure!(
                    args.len() == 3,
                    "/link ID_объявления ID_диалога ID_продавца"
                );
                let exists: bool = self.store.db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM archived_chats WHERE id=?1)",
                    [args[1]],
                    |r| r.get(0),
                )?;
                ensure!(exists, "Диалог не найден в архиве");
                self.store.db.execute("INSERT INTO task_offers(task_id,listing_id,chat_id,seller_id) VALUES(1,?1,?2,?3) ON CONFLICT(task_id,listing_id) DO UPDATE SET chat_id=excluded.chat_id,seller_id=excluded.seller_id",params![args[0],args[1],args[2]])?;
                "Связь с диалогом сохранена. История будет учтена целиком в пределах контекста, подтверждения — только от указанного продавца.".into()
            }
            "/trust" => {
                ensure!(
                    !argument.trim().is_empty(),
                    "/trust ID_продавца — отметить проверенный магазин"
                );
                self.store.db.execute("INSERT INTO trusted_sellers VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET verified_at=excluded.verified_at",params![argument,now()])?;
                self.store.recalculate()?;
                "Магазин отмечен как проверенный тобой. Оценки пересчитаны.".into()
            }
            "/history" => {
                let args: Vec<&str> = argument.split_whitespace().collect();
                ensure!(
                    !args.is_empty() && args.len() <= 2,
                    "/history ID_диалога [страница]"
                );
                let page: u32 = args.get(1).map(|p| p.parse()).transpose()?.unwrap_or(1);
                ensure!((1..=10000).contains(&page), "Неверная страница");
                let mut stmt=self.store.db.prepare("SELECT payload FROM archived_messages WHERE chat_id=?1 ORDER BY created DESC,message_id DESC LIMIT 8 OFFSET ?2")?;
                let rows = stmt
                    .query_map(params![args[0], (page - 1) * 8], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut messages = vec![];
                for raw in rows.iter().rev() {
                    let m: Value = serde_json::from_str(raw)?;
                    let text = super::privacy::redact(
                        m["body"]["text"]
                            .as_str()
                            .unwrap_or("[вложение/системное сообщение]"),
                    );
                    messages.push(format!(
                        "{} · {}\n{}",
                        m["created"],
                        m["fromUid"],
                        text.chars().take(330).collect::<String>()
                    ));
                }
                if messages.is_empty() {
                    "На этой странице нет сообщений.".into()
                } else {
                    format!(
                        "Архив диалога {} · страница {}\n\n{}\n\nДанные доступа и контакты скрыты. /history {} {} — следующая страница.",
                        args[0],
                        page,
                        messages.join("\n\n"),
                        args[0],
                        page + 1
                    )
                }
            }
            "/analyse" => self.analyse(argument)?,
            "/draft" => format!(
                "Сохранён черновик, отправка не выполнялась:\n{}",
                self.store.draft_questions(argument)?
            ),
            "/changes" | "🕒" => {
                let mut stmt = self
                    .store
                    .db
                    .prepare("SELECT text FROM assistant_outbox ORDER BY id DESC LIMIT 8")?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if rows.is_empty() {
                    "Событий совпадения пока нет.".into()
                } else {
                    rows.join("\n\n")
                }
            }
            "/prices" | "💰" => {
                let mut stmt = self
                    .store
                    .db
                    .prepare("SELECT assessment FROM assessments WHERE task_id=1 AND version=?1 AND json_extract(verdict,'$.status')='matched'")?;
                let mut prices = vec![];
                for row in
                    stmt.query_map([self.store.task()?.version], |r| r.get::<_, String>(0))?
                {
                    let a: super::model::Assessment = serde_json::from_str(&row?)?;
                    if let Some(p) = a
                        .facts
                        .get("total_price_rub")
                        .filter(|f| {
                            f.origin == Origin::Seller
                                && !f.contradiction
                                && now() - f.observed_at <= 86400
                        })
                        .and_then(|f| f.value.as_i64())
                    {
                        prices.push(p);
                    }
                }
                prices.sort();
                if prices.is_empty() {
                    "Полных цен среди подтверждённых совпадений пока нет.".into()
                } else {
                    format!(
                        "Известных цен: {}\nМинимум: {} ₽\nМедиана: {} ₽",
                        prices.len(),
                        prices[0],
                        prices[prices.len() / 2]
                    )
                }
            }
            _ => return Ok(None),
        };
        Ok(Some(response.chars().take(3900).collect()))
    }
    fn pending(&mut self) -> Result<Vec<(i64, String)>> {
        self.store.invalidate_drafts()?;
        self.work_step()?;
        self.store.recalculate()?;
        self.store.pending()
    }
    fn acknowledge(&mut self, id: i64) -> Result<()> {
        self.store.acknowledge(id)
    }
}

impl Controller {
    fn work_step(&mut self) -> Result<()> {
        use super::jobs::{CatalogSource, Job};
        let lease = match self.store.claim(&super::jobs::DirectPolicy) {
            Ok(lease) => lease,
            Err(error) => {
                self.store
                    .set_setting("network_blocked", error.to_string().into())?;
                return Ok(());
            }
        };
        let Some(lease) = lease else { return Ok(()) };
        let result = (|| -> Result<()> {
            match &lease.job {
                Job::Attachment {
                    chat,
                    message,
                    reference,
                } => {
                    let mut source = super::attachments::HttpAttachments::new()?;
                    super::attachments::save(
                        &self.store,
                        chat,
                        message,
                        reference,
                        &self.attachment_dir,
                        &mut source,
                    )?;
                }
                Job::Chats { .. } | Job::History { .. } => {
                    if self.messenger.is_none() {
                        self.messenger = Some(crate::adapters::messenger::Messenger::connect()?);
                    }
                    super::jobs::perform_read(
                        &mut self.store,
                        &lease.job,
                        self.messenger.as_mut().unwrap(),
                    )?;
                }
                Job::OpenChat { listing_id } => {
                    ensure!(
                        self.store.task()?.approved && !self.store.task()?.paused,
                        "Требования не подтверждены или задача остановлена"
                    );
                    ensure!(
                        self.store.setting("outreach")? == Some(true.into()),
                        "Уточнения продавцам отключены"
                    );
                    if self.messenger.is_none() {
                        self.messenger = Some(crate::adapters::messenger::Messenger::connect()?);
                    }
                    use super::sync::ConversationSource;
                    let chat = self
                        .messenger
                        .as_mut()
                        .unwrap()
                        .open_for_listing(listing_id)?;
                    self.store.db.execute("INSERT INTO task_offers(task_id,listing_id,chat_id) VALUES(1,?1,?2) ON CONFLICT(task_id,listing_id) DO UPDATE SET chat_id=excluded.chat_id",params![listing_id,chat])?;
                    self.store.enqueue(
                        Job::Chats {
                            before: None,
                            cycle: now(),
                        },
                        90,
                    )?;
                }
                Job::Dispatch { outgoing_id } => {
                    if self.messenger.is_none() {
                        self.messenger = Some(crate::adapters::messenger::Messenger::connect()?);
                    }
                    super::jobs::perform_dispatch(
                        &mut self.store,
                        *outgoing_id,
                        self.messenger.as_mut().unwrap(),
                    )?;
                }
                Job::Search {
                    search,
                    page,
                    limit,
                    version,
                } => {
                    ensure!(
                        self.store.task()?.version == *version,
                        "Требования изменились; запусти поиск заново"
                    );
                    if self.catalog.is_none() {
                        self.catalog = Some(crate::adapters::browser::BrowserSource::connect(
                            std::time::Duration::from_secs(20),
                        )?);
                    }
                    let (items, has_more) =
                        self.catalog.as_mut().unwrap().search_page(search, *page)?;

                    for listing in items {
                        listing.validate()?;
                        let existing:bool=self.store.db.query_row("SELECT EXISTS(SELECT 1 FROM task_offers WHERE task_id=1 AND listing_id=?1)",[&listing.id],|r|r.get(0))?;
                        self.store.db.execute("INSERT INTO listings(id,payload,first_seen,last_seen) VALUES(?1,?2,?3,?3) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,last_seen=excluded.last_seen",params![listing.id,serde_json::to_string(&listing)?,now()])?;
                        self.store.db.execute(
                            "INSERT OR IGNORE INTO task_offers(task_id,listing_id) VALUES(1,?1)",
                            [&listing.id],
                        )?;
                        let fresh:bool=self.store.db.query_row("SELECT EXISTS(SELECT 1 FROM offer_details WHERE id=?1 AND observed_at>?2)",params![listing.id,now()-86400],|r|r.get(0))?;
                        if !existing || !fresh {
                            self.store.enqueue(
                                Job::Detail {
                                    listing_id: listing.id,
                                },
                                10,
                            )?;
                        }
                    }
                    if has_more && page < limit {
                        self.store.enqueue(
                            Job::Search {
                                search: search.clone(),
                                page: page + 1,
                                limit: *limit,
                                version: *version,
                            },
                            20,
                        )?;
                    }
                }
                Job::Detail { listing_id } => {
                    let raw: String = self.store.db.query_row(
                        "SELECT payload FROM listings WHERE id=?1",
                        [listing_id],
                        |r| r.get(0),
                    )?;
                    let listing: crate::domain::Listing = serde_json::from_str(&raw)?;
                    if self.catalog.is_none() {
                        self.catalog = Some(crate::adapters::browser::BrowserSource::connect(
                            std::time::Duration::from_secs(20),
                        )?);
                    }
                    let detail = self
                        .catalog
                        .as_mut()
                        .unwrap()
                        .detail(&listing.id, &listing.url)?;
                    self.store.db.execute("INSERT INTO offer_details VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,observed_at=excluded.observed_at",params![listing.id,detail.to_string(),now()])?;
                    self.store.enqueue(
                        Job::Analyse {
                            listing_id: listing.id,
                        },
                        25,
                    )?;
                }
                Job::Analyse { listing_id } => {
                    self.analyse(listing_id)?;
                    if self.store.setting("outreach")? == Some(true.into())
                        && self.store.task()?.approved
                        && !self.store.task()?.paused
                    {
                        // A linked fully synchronized conversation is required; incomplete ones wait.
                        let ready:bool=self.store.db.query_row("SELECT EXISTS(SELECT 1 FROM task_offers o JOIN chat_sync s ON s.chat_id=o.chat_id WHERE o.listing_id=?1 AND o.seller_id IS NOT NULL AND s.complete=1 AND s.last_synced>?2)",params![listing_id,now()-300],|r|r.get(0))?;
                        let linked:bool=self.store.db.query_row("SELECT EXISTS(SELECT 1 FROM task_offers WHERE task_id=1 AND listing_id=?1 AND chat_id IS NOT NULL)",[listing_id],|r|r.get(0))?;
                        let rejected:bool=self.store.db.query_row("SELECT json_extract(verdict,'$.status')='rejected' FROM assessments WHERE task_id=1 AND listing_id=?1",[listing_id],|r|r.get(0))?;
                        if !linked && !rejected {
                            self.store.enqueue(
                                Job::OpenChat {
                                    listing_id: listing_id.clone(),
                                },
                                30,
                            )?;
                        }
                        if ready && self.store.draft_questions(listing_id).is_ok() {
                            let _ = self.store.queue_dispatch(listing_id);
                        }
                    }
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => self.store.finish(lease.id)?,
            Err(error) => {
                self.store.defer_failure(lease.id, &error.to_string())?;
                self.messenger = None;
                self.catalog = None;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabling_outreach_cancels_waiting_dispatches_without_sending() {
        let dir = std::env::temp_dir().join(format!("avito-outreach-off-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.sqlite");
        let base = crate::adapters::sqlite::SqliteRepository::open(&path).unwrap();
        let mut controller = Controller::new(&path).unwrap();
        controller.store.db.execute("INSERT INTO conversation_drafts VALUES(1,1,'123','chat',1,'history','question','draft',0)",[]).unwrap();
        controller.store.db.execute("INSERT INTO outgoing_messages(draft_id,chat_id,text,random_id,status,history_key,task_version,created) VALUES(1,'chat','question','random','queued','history',1,0)",[]).unwrap();
        controller
            .store
            .enqueue(super::super::jobs::Job::Dispatch { outgoing_id: 1 }, 70)
            .unwrap();
        controller.handle("/outreach off").unwrap();
        let status: String = controller
            .store
            .db
            .query_row("SELECT status FROM outgoing_messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "cancelled");
        let status: String = controller
            .store
            .db
            .query_row("SELECT status FROM work_jobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "cancelled");
        assert!(controller.messenger.is_none());
        drop(controller);
        drop(base);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn buttons_use_local_data_and_history_does_not_echo_credentials() {
        let dir = std::env::temp_dir().join(format!("avito-controller-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("test.sqlite");
        let _base = crate::adapters::sqlite::SqliteRepository::open(&db).unwrap();
        let mut controller = Controller::new(&db).unwrap();
        let file = dir.join("archive.json");
        std::fs::write(&file,json!({"chats":[{"channel":{"channelId":"test"},"complete":true,"messages":[{"id":"1","fromUid":"seller","created":1,"type":"text","body":{"text":"RTX PRO 6000\nпароль: sensitive"}}]}]}).to_string()).unwrap();
        controller.store.import_archive(&file).unwrap();
        assert!(
            controller
                .handle("📊 Сводка")
                .unwrap()
                .unwrap()
                .contains("Сообщений: 1")
        );
        let history = controller.handle("/history test").unwrap().unwrap();
        assert!(history.contains("RTX PRO 6000"));
        assert!(!history.contains("sensitive"));
        assert!(controller.model.is_none());
        drop(controller);
        drop(_base);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
