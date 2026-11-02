use crate::domain::SearchUrl;
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

pub const HELP: &str = "Бот поиска объявлений Авито.\n\n/scan ССЫЛКА [страницы] — один проход\n/watch ССЫЛКА [интервал_сек] [страницы] — мониторинг\n/stop — остановить мониторинг\n/status — состояние\n/help — помощь\n\nПо умолчанию: 1 страница, интервал 300 секунд. Первый проход уведомляет обо всех впервые найденных объявлениях. /stats — статистика\n/requirements — требования\n/task текст — изменить требования\n/approve — подтвердить требования\n/card ID — подробная карточка\n/analyse ID — анализ DeepSeek\n/network — маршрут Авито\n/research — поиск по запросам задачи\n/sync [диалог] — синхронизация переписок\n/queue — очередь и прогресс\n/outreach on|off — последовательные уточнения\n/attachments — сохранить вложения\n/pause, /resume — управление очередью\n\nСтатистика доступна кнопками.";

#[derive(Debug, Clone, Deserialize)]
pub struct IncomingMessage {
    pub update_id: i64,
    pub user_id: i64,
    pub chat_id: i64,
    pub private: bool,
    pub text: String,
}
impl IncomingMessage {
    pub fn authorized(&self, owner_id: i64) -> bool {
        self.private && self.user_id == owner_id && self.chat_id == owner_id
    }
}

pub enum BotCommand {
    Help,
    Status,
    Stop,
    Scan {
        search: SearchUrl,
        pages: u32,
    },
    Watch {
        search: SearchUrl,
        pages: u32,
        interval: u64,
    },
}
impl BotCommand {
    pub fn parse(text: &str) -> Result<Self> {
        let args: Vec<&str> = text.split_whitespace().collect();
        let name = args
            .first()
            .copied()
            .unwrap_or("")
            .split('@')
            .next()
            .unwrap_or("");
        match name {
            "/start" | "/help" if args.len() == 1 => Ok(Self::Help),
            "/status" if args.len() == 1 => Ok(Self::Status),
            "/stop" if args.len() == 1 => Ok(Self::Stop),
            "/scan" if (2..=3).contains(&args.len()) => {
                let pages = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(1);
                ensure!((1..=10).contains(&pages), "Число страниц: от 1 до 10");
                Ok(Self::Scan {
                    search: SearchUrl::parse(args[1])?,
                    pages,
                })
            }
            "/watch" if (2..=4).contains(&args.len()) => {
                let interval = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(1800);
                let pages = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(1);
                ensure!(
                    (1800..=86400).contains(&interval),
                    "Интервал: от 1800 до 86400 секунд"
                );
                ensure!((1..=10).contains(&pages), "Число страниц: от 1 до 10");
                Ok(Self::Watch {
                    search: SearchUrl::parse(args[1])?,
                    pages,
                    interval,
                })
            }
            _ => bail!("Неизвестная команда или неверные аргументы. /help — помощь"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub url: String,
    pub pages: u32,
    pub interval: u64,
    pub next_run: i64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BotState {
    pub owner_id: i64,
    pub offset: i64,
    pub change_cursor: i64,
    pub subscription: Option<Subscription>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_owner_in_private_chat_is_authorized() {
        let message = IncomingMessage {
            update_id: 1,
            user_id: 42,
            chat_id: 42,
            private: true,
            text: "/start".into(),
        };
        assert!(message.authorized(42));
        assert!(!message.authorized(43));
        let group = IncomingMessage {
            chat_id: -100,
            private: false,
            ..message
        };
        assert!(!group.authorized(42));
    }
    #[test]
    fn parses_commands_and_rejects_bad_links_and_limits() {
        assert!(matches!(
            BotCommand::parse("/scan@my_bot https://avito.ru/moskva?q=x 2").unwrap(),
            BotCommand::Scan { pages: 2, .. }
        ));
        assert!(matches!(
            BotCommand::parse("/watch https://avito.ru/moskva").unwrap(),
            BotCommand::Watch {
                pages: 1,
                interval: 1800,
                ..
            }
        ));
        for value in [
            "/scan",
            "/scan https://evil.example",
            "/scan https://avito.ru 0",
            "/scan https://avito.ru 11",
            "/watch https://avito.ru 1",
            "/watch https://avito.ru 999999",
            "/stop junk",
        ] {
            assert!(BotCommand::parse(value).is_err(), "{value}");
        }
    }
}
