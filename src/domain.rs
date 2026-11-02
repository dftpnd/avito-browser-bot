use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use url::Url;

/// Validated search location. Query parameters are preserved verbatim by Url.
#[derive(Debug, Clone)]
pub struct SearchUrl(Url);

impl SearchUrl {
    pub fn parse(value: &str) -> Result<Self> {
        let url = Url::parse(value)?;
        ensure!(url.scheme() == "https", "Search must use HTTPS");
        ensure!(
            matches!(url.host_str(), Some("avito.ru" | "www.avito.ru")),
            "Search must be on avito.ru"
        );
        ensure!(
            url.username().is_empty() && url.password().is_none() && url.port().is_none(),
            "Credentials and custom ports are not allowed"
        );
        ensure!(
            !["profile", "messenger", "account", "login"]
                .iter()
                .any(|s| url.path().split('/').any(|p| p == *s)),
            "Use a public search URL, not a personal page"
        );
        Ok(Self(url))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn page(&self, page: u32) -> String {
        let mut url = self.0.clone();
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| key != "p")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        url.set_query(None);
        url.query_pairs_mut()
            .extend_pairs(pairs)
            .append_pair("p", &page.to_string());
        url.into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub id: String,
    pub title: String,
    pub url: String,
    pub price: Option<i64>,
    pub location: String,
}

impl Listing {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.is_empty() && self.id.bytes().all(|b| b.is_ascii_digit()),
            "Invalid listing ID"
        );
        ensure!(!self.title.trim().is_empty(), "Empty listing title");
        let url = Url::parse(&self.url)?;
        ensure!(
            url.scheme() == "https" && matches!(url.host_str(), Some("avito.ru" | "www.avito.ru")),
            "Invalid listing URL"
        );
        ensure!(
            url.username().is_empty() && url.password().is_none() && url.port().is_none(),
            "Invalid listing authority"
        );
        ensure!(
            url.path().ends_with(&format!("_{}", self.id)),
            "Listing URL and ID disagree"
        );
        ensure!(self.price.is_none_or(|p| p >= 0), "Negative price");
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    New,
    PriceChanged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub kind: ChangeKind,
    pub listing: Listing,
    pub previous_price: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub search: String,
    pub observed_at: i64,
    pub count: usize,
    pub changes: Vec<Change>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pagination_preserves_filters_and_replaces_page() {
        let search =
            SearchUrl::parse("https://www.avito.ru/moskva?q=steam+deck&p=8&p=9&priceTo=50000")
                .unwrap();
        let page = Url::parse(&search.page(2)).unwrap();
        assert_eq!(
            page.query_pairs()
                .filter(|(k, _)| k == "p")
                .collect::<Vec<_>>(),
            vec![("p".into(), "2".into())]
        );
        assert!(
            page.query_pairs()
                .any(|(k, v)| k == "q" && v == "steam deck")
        );
        assert!(
            page.query_pairs()
                .any(|(k, v)| k == "priceTo" && v == "50000")
        );
    }
    #[test]
    fn refuses_private_and_foreign_urls() {
        for value in [
            "https://evil.example/",
            "https://www.avito.ru.evil.example/",
            "https://www.avito.ru/profile/messenger",
            "http://avito.ru/moskva",
            "https://user@avito.ru/",
            "https://avito.ru:9443/",
        ] {
            assert!(SearchUrl::parse(value).is_err(), "{value}");
        }
    }
}

pub mod bot;
