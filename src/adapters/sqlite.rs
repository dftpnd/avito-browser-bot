use crate::{
    domain::{Change, ChangeKind, Listing, ScanResult, SearchUrl},
    ports::ListingRepository,
};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::{path::Path, time::Duration};

pub struct SqliteRepository {
    conn: Connection,
}

impl SqliteRepository {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        Self::from_connection(Connection::open(path)?)
    }
    fn from_connection(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS bot_state (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS listings (
                id TEXT PRIMARY KEY, payload TEXT NOT NULL, first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS search_listings (
                search TEXT NOT NULL, listing_id TEXT NOT NULL REFERENCES listings(id),
                first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL, last_price INTEGER, PRIMARY KEY(search, listing_id));
            CREATE TABLE IF NOT EXISTS scans (
                id INTEGER PRIMARY KEY, search TEXT NOT NULL, observed_at INTEGER NOT NULL, count INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS changes (
                id INTEGER PRIMARY KEY, scan_id INTEGER NOT NULL REFERENCES scans(id), payload TEXT NOT NULL);")?;
        Ok(Self { conn })
    }
}

impl ListingRepository for SqliteRepository {
    fn record_scan(
        &mut self,
        search: &SearchUrl,
        listings: &[Listing],
        observed_at: i64,
    ) -> Result<ScanResult> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO scans(search,observed_at,count) VALUES(?1,?2,?3)",
            params![search.as_str(), observed_at, listings.len()],
        )?;
        let scan_id = tx.last_insert_rowid();
        let mut changes = Vec::new();
        for listing in listings {
            listing.validate()?;
            let previous: Option<Option<i64>> = tx
                .query_row(
                    "SELECT last_price FROM search_listings WHERE search=?1 AND listing_id=?2",
                    params![search.as_str(), listing.id],
                    |r| r.get(0),
                )
                .optional()?;
            let kind = if previous.is_none() {
                Some(ChangeKind::New)
            } else if previous.is_some_and(|price| price != listing.price) {
                Some(ChangeKind::PriceChanged)
            } else {
                None
            };
            tx.execute("INSERT INTO listings(id,payload,first_seen,last_seen) VALUES(?1,?2,?3,?3)
                ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,last_seen=excluded.last_seen", params![listing.id, serde_json::to_string(listing)?, observed_at])?;
            tx.execute("INSERT INTO search_listings(search,listing_id,first_seen,last_seen,last_price) VALUES(?1,?2,?3,?3,?4)
                ON CONFLICT(search,listing_id) DO UPDATE SET last_seen=excluded.last_seen,last_price=excluded.last_price", params![search.as_str(), listing.id, observed_at, listing.price])?;
            if let Some(kind) = kind {
                let change = Change {
                    kind,
                    listing: listing.clone(),
                    previous_price: previous.flatten(),
                };
                tx.execute(
                    "INSERT INTO changes(scan_id,payload) VALUES(?1,?2)",
                    params![scan_id, serde_json::to_string(&change)?],
                )?;
                changes.push(change);
            }
        }
        tx.commit()?;
        Ok(ScanResult {
            search: search.as_str().into(),
            observed_at,
            count: listings.len(),
            changes,
        })
    }
    fn listings(&self) -> Result<Vec<Listing>> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM listings ORDER BY first_seen DESC,id")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn listing(price: Option<i64>) -> Listing {
        Listing {
            id: "123".into(),
            title: "Test".into(),
            url: "https://www.avito.ru/moskva/test_123".into(),
            price,
            location: "Москва".into(),
        }
    }
    #[test]
    fn records_new_price_changes_and_membership_without_duplicates() {
        let mut repo =
            SqliteRepository::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        let a = SearchUrl::parse("https://www.avito.ru/moskva?q=a").unwrap();
        let b = SearchUrl::parse("https://www.avito.ru/moskva?q=b").unwrap();
        assert_eq!(
            repo.record_scan(&a, &[listing(Some(100))], 1)
                .unwrap()
                .changes[0]
                .kind,
            ChangeKind::New
        );
        assert!(
            repo.record_scan(&a, &[listing(Some(100))], 2)
                .unwrap()
                .changes
                .is_empty()
        );
        let scan = repo.record_scan(&a, &[listing(Some(80))], 3).unwrap();
        assert_eq!(scan.changes[0].kind, ChangeKind::PriceChanged);
        assert_eq!(scan.changes[0].previous_price, Some(100));
        assert_eq!(
            repo.record_scan(&b, &[listing(Some(80))], 4)
                .unwrap()
                .changes[0]
                .kind,
            ChangeKind::New
        );
        assert_eq!(repo.listings().unwrap().len(), 1);
        assert_eq!(
            repo.conn
                .query_row("SELECT count(*) FROM changes", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
    }
    #[test]
    fn overlapping_searches_track_their_own_price_observations() {
        let mut repo =
            SqliteRepository::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        let a = SearchUrl::parse("https://avito.ru/moskva?q=a").unwrap();
        let b = SearchUrl::parse("https://avito.ru/moskva?q=b").unwrap();
        for search in [&a, &b] {
            repo.record_scan(search, &[listing(Some(100))], 1).unwrap();
        }
        for search in [&a, &b] {
            let scan = repo.record_scan(search, &[listing(Some(80))], 2).unwrap();
            assert_eq!(scan.changes[0].kind, ChangeKind::PriceChanged);
            assert_eq!(scan.changes[0].previous_price, Some(100));
        }
    }
    #[test]
    fn invalid_listing_rolls_back_whole_scan() {
        let mut repo =
            SqliteRepository::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        let search = SearchUrl::parse("https://avito.ru/moskva").unwrap();
        let mut invalid = listing(None);
        invalid.id = "bad".into();
        assert!(
            repo.record_scan(&search, &[listing(Some(100)), invalid], 1)
                .is_err()
        );
        assert!(repo.listings().unwrap().is_empty());
        assert_eq!(
            repo.conn
                .query_row("SELECT count(*) FROM scans", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

impl crate::ports::BotRepository for SqliteRepository {
    fn bot_state(&self) -> Result<Option<crate::domain::bot::BotState>> {
        let value: Option<String> = self
            .conn
            .query_row("SELECT payload FROM bot_state WHERE id=1", [], |row| {
                row.get(0)
            })
            .optional()?;
        value.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
    }
    fn save_bot_state(&mut self, state: &crate::domain::bot::BotState) -> Result<()> {
        self.conn.execute("INSERT INTO bot_state(id,payload) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", [serde_json::to_string(state)?])?;
        Ok(())
    }
    fn latest_change_id(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id),0) FROM changes", [], |row| {
                row.get(0)
            })?)
    }
    fn changes_after(&self, after: i64, limit: u32) -> Result<Vec<(i64, Change)>> {
        let mut statement = self
            .conn
            .prepare("SELECT id,payload FROM changes WHERE id>?1 ORDER BY id LIMIT ?2")?;
        let rows = statement.query_map(params![after, limit], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (id, payload) = row?;
            Ok((id, serde_json::from_str(&payload)?))
        })
        .collect()
    }
}

#[cfg(test)]
mod bot_tests {
    use super::*;
    use crate::{
        domain::bot::{BotState, Subscription},
        ports::BotRepository,
    };
    #[test]
    fn resumes_bot_state_and_reads_durable_changes() {
        let mut store =
            SqliteRepository::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        let state = BotState {
            owner_id: 42,
            offset: 7,
            change_cursor: 0,
            subscription: Some(Subscription {
                url: "https://avito.ru/moskva".into(),
                pages: 2,
                interval: 300,
                next_run: 123,
            }),
        };
        store.save_bot_state(&state).unwrap();
        let loaded = store.bot_state().unwrap().unwrap();
        assert_eq!(loaded.offset, 7);
        assert_eq!(loaded.subscription.unwrap().next_run, 123);
        let listing = Listing {
            id: "123".into(),
            title: "Test".into(),
            url: "https://avito.ru/moskva/test_123".into(),
            price: Some(100),
            location: "Москва".into(),
        };
        store
            .record_scan(
                &SearchUrl::parse("https://avito.ru/moskva").unwrap(),
                &[listing],
                1,
            )
            .unwrap();
        let changes = store.changes_after(0, 20).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].1.kind, ChangeKind::New);
        assert!(store.changes_after(changes[0].0, 20).unwrap().is_empty());
    }
}
