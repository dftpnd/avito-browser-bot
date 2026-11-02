//! Local-only dashboard and navigation. No browser or model calls.
use super::{
    model::{Assessment, Origin, Status, Verdict},
    store::{Store, now},
};
use crate::domain::bot::{Button, ButtonAction, Reply};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

fn cb(label: impl Into<String>, action: impl Into<String>) -> Button {
    Button {
        label: label.into(),
        action: ButtonAction::Callback(action.into()),
    }
}
fn link(label: &str, url: String) -> Button {
    Button {
        label: label.into(),
        action: ButtonAction::Url(url),
    }
}
fn short(value: &str, n: usize) -> String {
    super::privacy::redact(value).chars().take(n).collect()
}
fn status_label(status: &Status) -> &str {
    match status {
        Status::Matched => "Полностью подходит",
        Status::NeedsDetails => "Нужно уточнить",
        Status::Rejected => "Не подходит",
        Status::Candidate => "Кандидат",
    }
}
fn age(at: i64) -> String {
    let elapsed = (now() - at).max(0);
    if elapsed < 60 {
        "меньше минуты назад".into()
    } else if elapsed < 3600 {
        format!("{} мин назад", elapsed / 60)
    } else if elapsed < 86400 {
        format!("{} ч назад", elapsed / 3600)
    } else {
        format!("{} дн назад", elapsed / 86400)
    }
}
fn group(tier: u8) -> &'static str {
    match tier {
        1 => "В наличии",
        2 => "Заказ без предоплаты",
        3 => "Заказ с частичной предоплатой",
        _ => "Наличие и оплата не выяснены",
    }
}
fn field(key: &str) -> &str {
    match key {
        "model" => "Модель",
        "edition" => "Версия",
        "memory_gb" => "Память, ГБ",
        "availability" => "Наличие",
        "total_price_rub" => "Полная цена, ₽",
        "deposit_percent" => "Предоплата, %",
        "delivery_days" => "Срок, дней",
        "warranty" => "Гарантия",
        other => other,
    }
}
fn value(value: &Value) -> String {
    match value {
        Value::String(s) => match s.as_str() {
            "in_stock" => "в наличии".into(),
            "order" => "под заказ".into(),
            "workstation" => "Workstation Edition".into(),
            "server" => "Server Edition".into(),
            "max_q" => "Max-Q Workstation Edition".into(),
            _ => short(s, 180),
        },
        _ => short(&value.to_string(), 180),
    }
}
#[derive(Debug)]
struct Offer {
    id: String,
    title: String,
    url: Option<String>,
    chat: Option<String>,
    assessment: Option<Assessment>,
    verdict: Option<Verdict>,
    excluded: bool,
}
fn filter(offer: &Offer, name: &str) -> bool {
    if name == "all" {
        return true;
    }
    if name == "excluded" {
        return offer.excluded;
    }
    if offer.excluded {
        return false;
    }
    match name {
        "unrated" => offer.verdict.is_none(),
        "matched" => offer
            .verdict
            .as_ref()
            .is_some_and(|v| v.status == Status::Matched),
        "needs" => offer
            .verdict
            .as_ref()
            .is_some_and(|v| matches!(v.status, Status::NeedsDetails | Status::Candidate)),
        "rejected" => offer
            .verdict
            .as_ref()
            .is_some_and(|v| v.status == Status::Rejected),
        "stock" => offer
            .verdict
            .as_ref()
            .is_some_and(|v| v.status == Status::Matched && v.tier == 1),
        "order" => offer
            .verdict
            .as_ref()
            .is_some_and(|v| v.status == Status::Matched && matches!(v.tier, 2 | 3)),
        "best" => offer
            .verdict
            .as_ref()
            .is_some_and(|v| v.status != Status::Rejected),
        "confirmed" => {
            offer
                .verdict
                .as_ref()
                .is_some_and(|v| v.status != Status::Rejected)
                && offer.assessment.as_ref().is_some_and(|a| {
                    ["model", "availability"].iter().all(|key| {
                        a.facts.get(*key).is_some_and(|f| {
                            f.origin == Origin::Seller
                                && !f.contradiction
                                && (0..=86400).contains(&(now() - f.observed_at))
                        })
                    })
                })
        }
        _ => false,
    }
}
fn price(offer: &Offer) -> Option<i64> {
    offer
        .assessment
        .as_ref()?
        .facts
        .get("total_price_rub")
        .filter(|f| {
            f.origin == Origin::Seller
                && !f.contradiction
                && (0..=86400).contains(&(now() - f.observed_at))
        })?
        .value
        .as_i64()
}
fn chat_link(chat: &str) -> Result<String> {
    let mut url = url::Url::parse("https://www.avito.ru/profile/messenger/channel/")?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Invalid conversation URL"))?
        .push(chat);
    Ok(url.into())
}
impl Store {
    fn dashboard_offers(&self) -> Result<Vec<Offer>> {
        let version = self.task()?.version;
        let mut stmt=self.db.prepare("SELECT ids.id,l.payload,o.chat_id,a.assessment,a.verdict,EXISTS(SELECT 1 FROM offer_exclusions e WHERE e.task_id=1 AND e.listing_id=ids.id) FROM (SELECT listing_id AS id FROM task_offers WHERE task_id=1 UNION SELECT listing_id AS id FROM assessments WHERE task_id=1) ids LEFT JOIN listings l ON l.id=ids.id LEFT JOIN task_offers o ON o.task_id=1 AND o.listing_id=ids.id LEFT JOIN assessments a ON a.task_id=1 AND a.listing_id=ids.id AND a.version=?1 ORDER BY ids.id")?;
        let rows = stmt.query_map([version], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, bool>(5)?,
            ))
        })?;
        rows.map(|r| {
            let (id, payload, chat, a, v, excluded) = r?;
            let listing: Value = payload
                .map(|p| serde_json::from_str(&p))
                .transpose()?
                .unwrap_or_default();
            let url = listing["url"]
                .as_str()
                .filter(|u| crate::domain::SearchUrl::parse(u).is_ok())
                .map(str::to_owned);
            Ok(Offer {
                id: id.clone(),
                title: short(
                    listing["title"]
                        .as_str()
                        .unwrap_or(&format!("Объявление {id}")),
                    160,
                ),
                url,
                chat,
                assessment: a.map(|v| serde_json::from_str(&v)).transpose()?,
                verdict: v.map(|v| serde_json::from_str(&v)).transpose()?,
                excluded,
            })
        })
        .collect()
    }
    pub fn dashboard(&self) -> Result<Reply> {
        let task = self.task()?;
        let offers = self.dashboard_offers()?;
        let count = |name: &str| offers.iter().filter(|o| filter(o, name)).count();
        let sellers:i64=self.db.query_row("SELECT count(DISTINCT seller_id) FROM task_offers WHERE task_id=1 AND seller_id IS NOT NULL",[],|r|r.get(0))?;
        let text = format!(
            "📊 {}\nТребования v{} · {}\n\nУникальных объявлений по задаче: {}\nПродавцов определено: {}\nМодель и наличие подтверждены продавцом: {}\nПолностью подходят: {}\nНужно уточнить: {}\nНе подходят: {}\nНе оценены: {}\nИсключены тобой: {}\n\nСреди совпадений: {} в наличии · {} под заказ.\n\nВыбери счётчик для просмотра списка. Данные из локальной базы; это не количество физических карт.",
            short(&task.title, 180),
            task.version,
            if task.approved {
                "подтверждены"
            } else {
                "черновик"
            },
            count("all"),
            sellers,
            count("confirmed"),
            count("matched"),
            count("needs"),
            count("rejected"),
            count("unrated"),
            count("excluded"),
            count("stock"),
            count("order")
        );
        let text = format!(
            "{text}\n\nАрхив аккаунта отдельно: {} диалогов · {} сообщений · {} ссылок на вложения.",
            self.count("archived_chats")?,
            self.count("archived_messages")?,
            self.count("archived_attachments")?
        );
        let mut rows = vec![];
        for pair in [
            [("Все", "all"), ("Подходят", "matched")],
            [("Нужно уточнить", "needs"), ("Не подходят", "rejected")],
            [("Не оценены", "unrated"), ("Исключённые", "excluded")],
            [("В наличии", "stock"), ("Под заказ", "order")],
        ] {
            rows.push(
                pair.into_iter()
                    .map(|(label, name)| {
                        cb(
                            format!("{} · {}", label, count(name)),
                            format!("ui:list:{name}:0"),
                        )
                    })
                    .collect(),
            );
        }
        rows.push(vec![cb(
            format!("Подтверждено продавцами · {}", count("confirmed")),
            "ui:list:confirmed:0",
        )]);
        rows.push(vec![cb("Обновить сводку", "ui:overview")]);
        Ok(Reply {
            text,
            buttons: rows,
        })
    }
    pub fn dashboard_list(&self, name: &str, page: usize) -> Result<Reply> {
        ensure!(
            [
                "all",
                "matched",
                "needs",
                "rejected",
                "unrated",
                "excluded",
                "stock",
                "order",
                "best",
                "confirmed"
            ]
            .contains(&name),
            "Неизвестный список"
        );
        ensure!(page < 10000, "Неверная страница");
        let mut offers = self
            .dashboard_offers()?
            .into_iter()
            .filter(|o| filter(o, name))
            .collect::<Vec<_>>();
        offers.sort_by_key(|o| {
            (
                (o.verdict
                    .as_ref()
                    .is_none_or(|v| v.status != Status::Matched)) as u8,
                o.verdict.as_ref().map(|v| v.tier).unwrap_or(4),
                price(o).unwrap_or(i64::MAX),
                o.id.clone(),
            )
        });
        let count = offers.len();
        let start = page.checked_mul(5).context("Неверная страница")?;
        let mut text = format!("Предложения: {count} · страница {}\n", page + 1);
        let mut buttons = vec![];
        for (i, o) in offers.iter().skip(start).take(5).enumerate() {
            text.push_str(&format!(
                "\n{}. {}\n{} · {}\nПолная подтверждённая цена: {}\n",
                start + i + 1,
                o.title,
                if o.excluded {
                    "Исключено тобой"
                } else {
                    o.verdict
                        .as_ref()
                        .map(|v| status_label(&v.status))
                        .unwrap_or("Не оценено")
                },
                o.verdict
                    .as_ref()
                    .map(|v| group(v.tier))
                    .unwrap_or("Условия не выяснены"),
                price(o)
                    .map(|p| format!("{p} ₽"))
                    .unwrap_or("неизвестна".into())
            ));
            buttons.push(vec![cb(
                format!("{}. Открыть предложение", start + i + 1),
                format!("ui:offer:{}", o.id),
            )]);
        }
        if start >= count {
            text.push_str("\nНа этой странице предложений нет.");
        }
        let mut nav = vec![];
        if page > 0 {
            nav.push(cb("← Назад", format!("ui:list:{name}:{}", page - 1)));
        }
        if start + 5 < count {
            nav.push(cb("Далее →", format!("ui:list:{name}:{}", page + 1)));
        }
        if !nav.is_empty() {
            buttons.push(nav)
        }
        buttons.push(vec![cb("К сводке", "ui:overview")]);
        Ok(Reply { text, buttons })
    }
    pub fn dashboard_offer(&self, id: &str, proofs: bool) -> Result<Reply> {
        ensure!(
            !id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit()),
            "Неверный ID предложения"
        );
        let offer = self
            .dashboard_offers()?
            .into_iter()
            .find(|o| o.id == id)
            .context("Предложение отсутствует в текущей задаче")?;
        let task = self.task()?;
        let mut text = format!(
            "{}\n{}\n",
            offer.title,
            if offer.excluded {
                "Исключено тобой"
            } else {
                offer
                    .verdict
                    .as_ref()
                    .map(|v| status_label(&v.status))
                    .unwrap_or("Ещё не оценено")
            }
        );
        if let Some(v) = &offer.verdict {
            text.push_str(&format!("{}\n", group(v.tier)));
            for reason in &v.reasons {
                text.push_str(&format!("• {}\n", short(reason, 200)));
            }
        }
        if let Some(a) = &offer.assessment {
            if !proofs {
                text.push_str(&format!("\n{}\n", short(&a.explanation, 700)));
            }
            for (key, f) in a
                .facts
                .iter()
                .filter(|(key, _)| {
                    task.rules.iter().any(|r| r.key == **key)
                        || [
                            "availability",
                            "total_price_rub",
                            "deposit_percent",
                            "delivery_days",
                            "warranty",
                        ]
                        .contains(&key.as_str())
                })
                .take(12)
            {
                text.push_str(&format!(
                    "\n{}: {} · {}{}\n",
                    task.rules
                        .iter()
                        .find(|r| r.key == *key)
                        .map(|r| r.label.as_str())
                        .unwrap_or_else(|| field(key)),
                    value(&f.value),
                    match f.origin {
                        Origin::Seller => "ответ продавца",
                        Origin::Listing => "описание",
                        Origin::Buyer => "твоё сообщение",
                        Origin::Owner => "твоя проверка",
                    },
                    if f.contradiction {
                        " · есть противоречие"
                    } else if now() - f.observed_at > 86400 {
                        " · устарело"
                    } else {
                        ""
                    }
                ));
                if proofs {
                    text.push_str(&format!(
                        "«{}»\nПолучено {}\n",
                        short(&f.quote, 180),
                        age(f.observed_at)
                    ));
                }
            }
        } else {
            text.push_str("\nДанных для оценки пока нет.");
        }
        let mut buttons = vec![vec![cb(
            if proofs {
                "К карточке"
            } else {
                "Подтверждения"
            },
            format!("ui:{}:{id}", if proofs { "offer" } else { "proof" }),
        )]];
        let mut links = vec![];
        if let Some(url) = offer.url {
            links.push(link("Объявление на Авито", url));
        }
        if let Some(chat) = offer.chat {
            links.push(link("Диалог на Авито", chat_link(&chat)?));
        }
        if !links.is_empty() {
            buttons.push(links)
        }
        buttons.push(vec![cb(
            if offer.excluded {
                "Вернуть вариант"
            } else {
                "Исключить вариант"
            },
            format!(
                "ui:{}:{id}:{}",
                if offer.excluded { "include" } else { "exclude" },
                task.version
            ),
        )]);
        buttons.push(vec![cb("К сводке", "ui:overview")]);
        Ok(Reply {
            text: super::privacy::redact(&text).chars().take(3800).collect(),
            buttons,
        })
    }
    pub fn dashboard_events(&self, page: usize) -> Result<Reply> {
        ensure!(page < 10000, "Неверная страница");
        let mut stmt=self.db.prepare("SELECT o.id,o.text,EXISTS(SELECT 1 FROM notification_reads r WHERE r.event_id=o.id),EXISTS(SELECT 1 FROM notification_superseded s WHERE s.event_id=o.id) FROM assistant_outbox o WHERE o.task_id=1 ORDER BY o.id DESC LIMIT 6 OFFSET ?1")?;
        let rows = stmt
            .query_map([page * 5], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, bool>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut reply = Reply::text(format!("История событий · страница {}\n", page + 1));
        for (id, text, seen, superseded) in rows.iter().take(5) {
            reply.text.push_str(&format!(
                "\n{} №{}\n{}\n",
                if *superseded {
                    "Заменено новым событием"
                } else if *seen {
                    "Просмотрено"
                } else {
                    "Новое"
                },
                id,
                short(text, 450)
            ));
            if !seen && !superseded {
                reply.buttons.push(vec![cb(
                    format!("Просмотрено · №{id}"),
                    format!("ui:seen:{id}"),
                )]);
            }
        }
        if rows.is_empty() {
            reply.text.push_str("\nСобытий пока нет.");
        }
        let mut nav = vec![];
        if page > 0 {
            nav.push(cb("← Назад", format!("ui:events:{}", page - 1)));
        }
        if rows.len() > 5 {
            nav.push(cb("Далее →", format!("ui:events:{}", page + 1)));
        }
        if !nav.is_empty() {
            reply.buttons.push(nav)
        }
        reply.buttons.push(vec![cb("К сводке", "ui:overview")]);
        Ok(reply)
    }
    pub fn mark_event_seen(&self, id: i64) -> Result<()> {
        let exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM assistant_outbox WHERE id=?1 AND task_id=1)",
            [id],
            |r| r.get(0),
        )?;
        ensure!(exists, "Событие не найдено");
        self.db.execute(
            "INSERT INTO notification_reads VALUES(?1,?2) ON CONFLICT(event_id) DO NOTHING",
            params![id, now()],
        )?;
        Ok(())
    }
    pub fn exclude_offer(&mut self, id: &str, version: u32, exclude: bool) -> Result<()> {
        ensure!(
            self.task()?.version == version,
            "Требования изменились; открой актуальную карточку"
        );
        ensure!(
            self.dashboard_offers()?.iter().any(|o| o.id == id),
            "Предложение не найдено"
        );
        if exclude {
            self.db.execute(
                "INSERT OR IGNORE INTO offer_exclusions VALUES(1,?1,?2)",
                params![id, now()],
            )?;
            self.db.execute("UPDATE conversation_drafts SET status='excluded' WHERE task_id=1 AND listing_id=?1 AND status='draft'",[id])?;
            self.db.execute("UPDATE outgoing_messages SET status='outdated' WHERE status='queued' AND draft_id IN(SELECT id FROM conversation_drafts WHERE task_id=1 AND listing_id=?1)",[id])?;
            self.db.execute("UPDATE work_jobs SET status='cancelled' WHERE status='queued' AND kind='dispatch' AND json_extract(payload,'$.outgoing_id') IN (SELECT id FROM outgoing_messages WHERE status='outdated')",[])?;
        } else {
            self.db.execute(
                "DELETE FROM offer_exclusions WHERE task_id=1 AND listing_id=?1",
                [id],
            )?;
        }
        self.recalculate()?;
        Ok(())
    }
    pub fn event_reply(&self, id: i64, text: String) -> Result<Reply> {
        let listing: Option<String> = self
            .db
            .query_row(
                "SELECT listing_id FROM assistant_outbox WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let mut reply = Reply::text(super::privacy::redact(&text));
        if let Some(listing) = listing {
            reply.buttons.push(vec![
                cb("Предложение", format!("ui:offer:{listing}")),
                cb("Подтверждения", format!("ui:proof:{listing}")),
            ]);
        }
        reply
            .buttons
            .push(vec![cb("Просмотрено", format!("ui:seen:{id}"))]);
        Ok(reply)
    }
}
