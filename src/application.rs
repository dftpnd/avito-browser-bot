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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Listing;

    struct Source {
        fail: bool,
    }
    impl ListingSource for Source {
        fn collect(&mut self, _: &SearchUrl, _: u32) -> Result<Vec<Listing>> {
            anyhow::ensure!(!self.fail, "blocked");
            let item = Listing {
                id: "123".into(),
                title: "Test".into(),
                url: "https://avito.ru/moskva/test_123".into(),
                price: Some(100),
                location: "Москва".into(),
            };
            Ok(vec![item.clone(), item])
        }
    }
    #[derive(Default)]
    struct Store {
        calls: usize,
    }
    impl ListingRepository for Store {
        fn record_scan(
            &mut self,
            search: &SearchUrl,
            items: &[Listing],
            observed_at: i64,
        ) -> Result<ScanResult> {
            self.calls += 1;
            Ok(ScanResult {
                search: search.as_str().into(),
                count: items.len(),
                observed_at,
                changes: vec![],
            })
        }
        fn listings(&self) -> Result<Vec<Listing>> {
            Ok(vec![])
        }
    }
    #[test]
    fn failed_source_never_records_a_successful_scan() {
        let search = SearchUrl::parse("https://avito.ru/moskva?q=a").unwrap();
        let mut store = Store::default();
        assert!(scan(&mut Source { fail: true }, &mut store, &search, 1).is_err());
        assert_eq!(store.calls, 0);
    }
    #[test]
    fn repeated_cards_are_stored_once() {
        let search = SearchUrl::parse("https://avito.ru/moskva?q=a").unwrap();
        let mut store = Store::default();
        let result = scan(&mut Source { fail: false }, &mut store, &search, 2).unwrap();
        assert_eq!(result.count, 1);
        assert_eq!(store.calls, 1);
    }
}
