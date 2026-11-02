use crate::{domain::bot::IncomingMessage, ports::BotTransport};
use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

// No Debug implementation: this object contains a credential.
#[derive(Deserialize)]
pub struct TelegramConfig {
    pub token: String,
    pub owner_id: Option<i64>,
}
impl TelegramConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(
            &std::fs::read(path).context("Cannot read local Telegram config")?,
        )
        .map_err(|_| anyhow::anyhow!("Invalid Telegram config JSON"))?;
        let (id, key) = config
            .token
            .split_once(':')
            .context("Invalid Telegram token format")?;
        ensure!(
            !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_digit())
                && !key.is_empty()
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "Invalid Telegram token format"
        );
        Ok(config)
    }
}

pub struct TelegramClient {
    client: Client,
    token: String,
}
impl TelegramClient {
    pub fn new(token: String) -> Result<Self> {
        Ok(Self {
            client: Client::builder().timeout(Duration::from_secs(35)).build()?,
            token,
        })
    }
    fn call(&self, method: &str, payload: Value) -> Result<Value> {
        let response = self
            .client
            .post(format!(
                "https://api.telegram.org/bot{}/{method}",
                self.token
            ))
            .json(&payload)
            .send()
            // reqwest errors include request URLs with the token unless stripped.
            .map_err(|error| anyhow::anyhow!("Telegram {method}: {}", error.without_url()))?;
        let status = response.status();
        let data: Value = response
            .json()
            .map_err(|_| anyhow::anyhow!("Telegram {method}: invalid response (HTTP {status})"))?;
        if data["ok"] != true {
            let description = data["description"]
                .as_str()
                .unwrap_or("API rejected the request")
                .replace(&self.token, "[REDACTED]");
            bail!("Telegram {method}: HTTP {status}: {description}");
        }
        Ok(data["result"].clone())
    }
    pub fn check(&self) -> Result<String> {
        let me = self.call("getMe", json!({}))?;
        let webhook = self.call("getWebhookInfo", json!({}))?;
        ensure!(
            webhook["url"].as_str().unwrap_or("").is_empty(),
            "A webhook is configured; long polling cannot run alongside it"
        );
        Ok(me["username"]
            .as_str()
            .context("Telegram bot has no username")?
            .into())
    }
    pub fn candidates(&self) -> Result<Vec<IncomingMessage>> {
        self.updates(0, 0)
    }
    fn updates(&self, offset: i64, timeout: u32) -> Result<Vec<IncomingMessage>> {
        let updates = self.call(
            "getUpdates",
            json!({"offset":offset,"timeout":timeout,"allowed_updates":["message","callback_query"]}),
        )?;
        let updates = updates
            .as_array()
            .context("Telegram updates must be an array")?;
        Ok(updates.iter().filter_map(normalize_update).collect())
    }
}
impl BotTransport for TelegramClient {
    fn answer_callback(&mut self, id: &str) -> Result<()> {
        self.call("answerCallbackQuery", json!({"callback_query_id":id}))?;
        Ok(())
    }
    fn send_reply(&mut self, chat_id: i64, reply: &crate::domain::bot::Reply) -> Result<()> {
        if reply.buttons.is_empty() {
            return self.send(chat_id, &reply.text);
        }
        ensure!(
            reply.text.chars().count() <= 4000,
            "Telegram message too long"
        );
        let keyboard = inline_keyboard(&reply.buttons)?;
        self.call("sendMessage",json!({"chat_id":chat_id,"text":reply.text,"link_preview_options":{"is_disabled":true},"reply_markup":{"inline_keyboard":keyboard}}))?;
        Ok(())
    }

    fn receive(&mut self, offset: i64) -> Result<Vec<IncomingMessage>> {
        self.updates(offset, 20)
    }
    fn send(&mut self, chat_id: i64, text: &str) -> Result<()> {
        ensure!(text.chars().count() <= 4000, "Telegram message too long");
        self.call(
            "sendMessage",
            json!({"chat_id":chat_id,"text":text,"link_preview_options":{"is_disabled":true},"reply_markup":{"keyboard":[["📊 Сводка","✅ Подходят"],["🏆 Лучшие","💬 Диалоги"],["💰 Цены","🛠 Работа бота"],["🕒 Изменения","⚙️ Требования"],["⏸ Пауза"]],"resize_keyboard":true,"is_persistent":true}}),
        )?;
        Ok(())
    }
}

pub(crate) fn normalize_update(update: &Value) -> Option<IncomingMessage> {
    let callback = update.get("callback_query");
    let message = callback
        .map(|c| &c["message"])
        .unwrap_or(&update["message"]);
    let user = callback.map(|c| &c["from"]).unwrap_or(&message["from"]);
    Some(IncomingMessage {
        update_id: update["update_id"].as_i64()?,
        user_id: user["id"].as_i64().unwrap_or(0),
        chat_id: message["chat"]["id"].as_i64().unwrap_or(0),
        private: message["chat"]["type"] == "private",
        text: callback
            .and_then(|c| c["data"].as_str())
            .or_else(|| message["text"].as_str())
            .unwrap_or("")
            .into(),
        callback_id: callback.and_then(|c| c["id"].as_str()).map(str::to_owned),
    })
}

fn inline_keyboard(rows: &[Vec<crate::domain::bot::Button>]) -> Result<Vec<Vec<Value>>> {
    use crate::domain::bot::ButtonAction;
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|button| {
                    Ok(match &button.action {
                        ButtonAction::Callback(data) => {
                            ensure!(
                                !data.is_empty() && data.len() <= 64,
                                "Callback payload exceeds Telegram limit"
                            );
                            json!({"text":button.label,"callback_data":data})
                        }
                        ButtonAction::Url(url) => {
                            let parsed = url::Url::parse(url)?;
                            ensure!(
                                parsed.scheme() == "https"
                                    && parsed.username().is_empty()
                                    && parsed.password().is_none(),
                                "Button URL must be HTTPS"
                            );
                            json!({"text":button.label,"url":url})
                        }
                    })
                })
                .collect::<Result<Vec<_>>>()
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callbacks_are_authorized_by_actual_clicker_and_private_chat() {
        let u = json!({"update_id":1,"callback_query":{"id":"cb","from":{"id":99},"data":"ui:overview","message":{"from":{"id":42},"chat":{"id":42,"type":"private"}}}});
        let message = normalize_update(&u).unwrap();
        assert!(!message.authorized(42));
        assert_eq!(message.callback_id.as_deref(), Some("cb"));
        assert_eq!(message.user_id, 99);
    }
    #[test]
    fn callback_limit_counts_bytes() {
        use crate::domain::bot::{Button, ButtonAction};
        let button = Button {
            label: "test".into(),
            action: ButtonAction::Callback("я".repeat(33)),
        };
        assert!(inline_keyboard(&[vec![button]]).is_err());
    }
}
