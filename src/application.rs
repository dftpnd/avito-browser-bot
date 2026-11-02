use crate::{
    domain::{ScanResult, SearchUrl},
    ports::{ListingRepository, ListingSource},
};
use anyhow::Result;
use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

pub fn scan(
    source: &mut impl ListingSource,
    repository: &mut impl ListingRepository,
    search: &SearchUrl,
    max_pages: u32,
) -> Result<ScanResult> {
    // Source errors never turn into an empty successful scan.
    let listings = source.collect(search, max_pages)?;
    let mut unique = BTreeMap::new();
    for listing in listings {
        listing.validate()?;
        unique.entry(listing.id.clone()).or_insert(listing);
    }
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    repository.record_scan(search, &unique.into_values().collect::<Vec<_>>(), timestamp)
}
