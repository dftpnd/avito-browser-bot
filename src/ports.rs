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
