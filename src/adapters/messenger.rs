//! Uses the existing signed-in page's native client. Never owns or closes that page.
use super::browser::{Cdp, verify_profile};
use crate::purchase::sync::{
    ChatPage, ConversationSource, DispatchResult, HistoryPage, MessageSender, SendIntent,
};
use anyhow::{Context, Result, ensure};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::time::Duration;
pub struct Messenger {
    page: Cdp,
    self_ids: Vec<String>,
}
impl Messenger {
    pub fn connect() -> Result<Self> {
        super::network::require_direct()?;
        verify_profile()?;
        let http = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()?;
        let tabs: Vec<Value> = http
            .get("http://127.0.0.1:9222/json/list")
            .send()?
            .error_for_status()?
            .json()?;
        let tab = tabs
            .iter()
            .find(|t| {
                t["type"] == "page"
                    && t["url"]
                        .as_str()
                        .is_some_and(|u| u.starts_with("https://www.avito.ru/profile/messenger"))
            })
            .context(
                "Открой сообщения Авито в указанном Chromium; существующая вкладка не найдена",
            )?;
        let mut page = Cdp::connect(
            tab["webSocketDebuggerUrl"]
                .as_str()
                .context("No messenger CDP endpoint")?,
        )?;
        let ready = page.evaluate(include_str!("messenger_init.js"))?;
        ensure!(
            ready["ok"] == true,
            "Native messenger client unavailable; no request sent"
        );
        let identity=page.evaluate("(()=>{const s=window.__avitoAssistant.store.getState().system;return [s.userId,s.userIdNum].filter(v=>v!=null).map(String)})()")?;
        let self_ids =
            serde_json::from_value(identity).context("Cannot identify signed-in account")?;
        Ok(Self { page, self_ids })
    }
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        super::network::require_direct()?;
        let script = format!(
            r#"(async()=>{{const a=window.__avitoAssistant;if(!a)throw Error('Messenger session expired');const text=(document.body?.innerText||'').toLowerCase();if(/доступ ограничен|подтвердите, что вы не робот|captcha/.test(text))throw Error('Avito verification required');return await a.client.request({method},{params},{{cache:false}})}})()"#,
            method = serde_json::to_string(method)?,
            params = params
        );
        let response = self
            .page
            .evaluate(&script)
            .map_err(|_| anyhow::anyhow!("Messenger request failed or its result is unknown"))?;
        ensure!(
            response.get("error").is_none(),
            "Messenger rejected the request; operation stopped"
        );
        response
            .get("result")
            .cloned()
            .context("Messenger returned an unexpected response")
    }
}
impl ConversationSource for Messenger {
    fn open_for_listing(&mut self, listing: &str) -> Result<String> {
        let id: u64 = listing.parse().context("Invalid listing identifier")?;
        let result = self.request("avito.chatCreateByItemId.v2", json!({"itemId":id}))?;
        result["channelId"]
            .as_str()
            .or_else(|| result["channel"]["channelId"].as_str())
            .map(str::to_owned)
            .context("Conversation creation result unrecognized; no message sent")
    }

    fn self_user_ids(&self) -> Vec<String> {
        self.self_ids.clone()
    }
    fn chats(&mut self, before: Option<f64>) -> Result<ChatPage> {
        let mut params = json!({"limit":30,"filters":{}});
        if let Some(before) = before {
            params["offsetTimestamp"] = before.into();
        }
        let result = self.request("avito.getChats.v5", params)?;
        Ok(ChatPage {
            channels: serde_json::from_value(result["channels"].clone())
                .context("Missing channels")?,
            has_more: result["hasMore"]
                .as_bool()
                .context("Missing chat pagination flag")?,
        })
    }
    fn history(&mut self, chat: &str, before: Option<f64>) -> Result<HistoryPage> {
        let mut params = json!({"channelId":chat,"limit":100});
        if let Some(before) = before {
            params["before"] = before.into();
        }
        let result = self.request("messenger.history.v2", params)?;
        Ok(HistoryPage {
            messages: serde_json::from_value(result["items"].clone())
                .context("Missing history items")?,
            has_more: result["hasMore"].as_bool(),
        })
    }
}
impl MessageSender for Messenger {
    fn send_if_unchanged(&mut self, intent: &SendIntent) -> Result<DispatchResult> {
        super::network::require_direct()?;
        ensure!(
            !intent.text.trim().is_empty() && intent.text.chars().count() <= 3000,
            "Invalid outgoing message"
        );
        // Fresh read and conditional send occur together inside the page. No blind send or cached read.
        let script = conditional_send_script(intent)?;
        let value = self.page.evaluate(&script).map_err(|_| {
            anyhow::anyhow!(
                "Message send outcome is unknown; inspect conversation before any retry"
            )
        })?;
        serde_json::from_value(value).context("Message send response not recognized")
    }
}

pub(crate) fn conditional_send_script(intent: &SendIntent) -> Result<String> {
    Ok(include_str!("send_if_current.js").replace("__INTENT__", &serde_json::to_string(intent)?))
}
