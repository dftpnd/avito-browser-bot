use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryPage {
    pub messages: Vec<Value>,
    pub has_more: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatPage {
    pub channels: Vec<Value>,
    pub has_more: bool,
}
pub trait ConversationSource {
    fn open_for_listing(&mut self, _: &str) -> Result<String> {
        anyhow::bail!("Opening conversations unsupported by this source")
    }
    fn self_user_ids(&self) -> Vec<String> {
        vec![]
    }
    fn chats(&mut self, before: Option<f64>) -> Result<ChatPage>;
    fn history(&mut self, chat: &str, before: Option<f64>) -> Result<HistoryPage>;
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendIntent {
    pub chat_id: String,
    pub text: String,
    pub random_id: String,
    pub expected_head: Vec<(String, String)>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DispatchResult {
    Changed { messages: Vec<Value> },
    Sent { message: Value },
}
pub trait MessageSender {
    fn send_if_unchanged(&mut self, intent: &SendIntent) -> Result<DispatchResult>;
}

pub fn message_key(message: &Value) -> Result<(String, String)> {
    let id = message["id"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| message["id"].as_i64().map(|v| v.to_string()))
        .context("Message has no ID")?;
    // Normalize only the stable content/status fields; delivery/read receipts are not new content.
    let stable = serde_json::json!({"id":id,"fromUid":message["fromUid"],"body":message["body"],"type":message["type"],"created":message["created"],"isDeleted":message["isDeleted"]});
    Ok((id, serde_json::to_string(&stable)?))
}
pub fn next_history_cursor(page: &HistoryPage, before: Option<f64>) -> Result<Option<f64>> {
    if page.messages.is_empty() || page.has_more == Some(false) {
        return Ok(None);
    }
    let cursor = page
        .messages
        .iter()
        .map(|m| m["created"].as_f64().context("Message timestamp missing"))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .reduce(f64::min)
        .context("No cursor")?;
    ensure!(cursor.is_finite(), "Invalid history timestamp");
    match before {
        Some(previous) if cursor >= previous => {
            ensure!(
                page.messages.len() < 100,
                "Full repeated history boundary; completeness not proven"
            );
            Ok(Some(previous.next_down()))
        }
        _ => Ok(Some(cursor)),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn full_page_requires_probe_and_boundary_loop_is_rejected() {
        let page = HistoryPage {
            messages: (0..100).map(|i| json!({"id":i,"created":10.0})).collect(),
            has_more: None,
        };
        assert_eq!(next_history_cursor(&page, None).unwrap(), Some(10.0));
        assert!(next_history_cursor(&page, Some(10.0)).is_err());
        let end = HistoryPage {
            messages: vec![],
            has_more: None,
        };
        assert_eq!(next_history_cursor(&end, Some(10.0)).unwrap(), None);
    }
    #[test]
    fn manual_edits_change_key_but_read_receipts_do_not() {
        let mut a = json!({"id":"1","body":{"text":"hello"},"created":1});
        let key = message_key(&a).unwrap();
        a["isRead"] = true.into();
        assert_eq!(message_key(&a).unwrap(), key);
        a["body"]["text"] = "changed".into();
        assert_ne!(message_key(&a).unwrap(), key);
    }
}
