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
            json!({"offset":offset,"timeout":timeout,"allowed_updates":["message"]}),
        )?;
        let updates = updates
            .as_array()
            .context("Telegram updates must be an array")?;
        Ok(updates
            .iter()
            .filter_map(|update| {
                // Include unsupported updates as unauthorized empty envelopes so the offset advances.
                Some(IncomingMessage {
                    update_id: update["update_id"].as_i64()?,
                    user_id: update["message"]["from"]["id"].as_i64().unwrap_or(0),
                    chat_id: update["message"]["chat"]["id"].as_i64().unwrap_or(0),
                    private: update["message"]["chat"]["type"] == "private",
                    text: update["message"]["text"].as_str().unwrap_or("").to_owned(),
                })
            })
            .collect())
    }
}
impl BotTransport for TelegramClient {
    fn receive(&mut self, offset: i64) -> Result<Vec<IncomingMessage>> {
        self.updates(offset, 20)
    }
    fn send(&mut self, chat_id: i64, text: &str) -> Result<()> {
        ensure!(text.chars().count() <= 4000, "Telegram message too long");
        self.call(
            "sendMessage",
            json!({"chat_id":chat_id,"text":text,"link_preview_options":{"is_disabled":true}}),
        )?;
        Ok(())
    }
}
