use crate::domain::{Listing, ScanResult, SearchUrl};
use anyhow::Result;

pub trait ListingSource {
    fn collect(&mut self, search: &SearchUrl, max_pages: u32) -> Result<Vec<Listing>>;
}

pub trait ListingRepository {
    /// A scan is committed atomically, including its change history.
    fn record_scan(
        &mut self,
        search: &SearchUrl,
        listings: &[Listing],
        observed_at: i64,
    ) -> Result<ScanResult>;
    fn listings(&self) -> Result<Vec<Listing>>;
}

/// Transport exposes normalized messages; application code does not know Telegram JSON.
pub trait BotTransport {
    fn receive(&mut self, offset: i64) -> Result<Vec<crate::domain::bot::IncomingMessage>>;
    fn send(&mut self, chat_id: i64, text: &str) -> Result<()>;
}

pub trait BotRepository {
    fn bot_state(&self) -> Result<Option<crate::domain::bot::BotState>>;
    fn save_bot_state(&mut self, state: &crate::domain::bot::BotState) -> Result<()>;
    fn latest_change_id(&self) -> Result<i64>;
    fn changes_after(&self, after: i64, limit: u32) -> Result<Vec<(i64, crate::domain::Change)>>;
}
