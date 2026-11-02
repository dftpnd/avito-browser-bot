use crate::{
    domain::{Listing, SearchUrl},
    ports::ListingSource,
};
use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    net::TcpStream,
    path::Path,
    thread,
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket, client};

const CDP: &str = "http://127.0.0.1:9222";
const PROFILE: &str = "/home/mg/kinobot/chrome-profile";
const EXTRACT: &str = include_str!("extract.js");

/// This adapter owns only the tab it creates; it never closes the browser.
pub struct BrowserSource {
    browser: Cdp,
    page: Cdp,
    target: String,
    page_delay: Duration,
}

pub(crate) struct Cdp {
    socket: WebSocket<TcpStream>,
    next_id: u64,
}
impl Cdp {
    pub(crate) fn connect(address: &str) -> Result<Self> {
        let url = url::Url::parse(address)?;
        ensure!(
            url.scheme() == "ws" && url.host_str() == Some("127.0.0.1") && url.port() == Some(9222),
            "Unexpected CDP websocket address"
        );
        let socket =
            TcpStream::connect_timeout(&"127.0.0.1:9222".parse()?, Duration::from_secs(5))?;
        socket.set_read_timeout(Some(Duration::from_secs(15)))?;
        socket.set_write_timeout(Some(Duration::from_secs(15)))?;
        let (socket, _) =
            client(address, socket).map_err(|e| anyhow::anyhow!("CDP handshake failed: {e}"))?;
        Ok(Self { socket, next_id: 0 })
    }
    pub(crate) fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.socket.send(Message::Text(
            json!({"id":id,"method":method,"params":params})
                .to_string()
                .into(),
        ))?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            ensure!(Instant::now() < deadline, "CDP command timed out: {method}");
            match self.socket.read()? {
                Message::Text(value) => {
                    let message: Value = serde_json::from_str(&value)?;
                    if message["id"].as_u64() == Some(id) {
                        if let Some(error) = message.get("error") {
                            bail!("CDP {method}: {error}");
                        }
                        return Ok(message["result"].clone());
                    }
                }
                Message::Close(_) => bail!("Browser closed the CDP connection"),
                _ => {}
            }
        }
    }
    pub(crate) fn evaluate(&mut self, expression: &str) -> Result<Value> {
        let result = self.call(
            "Runtime.evaluate",
            json!({"expression":expression,"returnByValue":true,"awaitPromise":true}),
        )?;
        ensure!(
            result.get("exceptionDetails").is_none(),
            "Page evaluation failed: {}",
            result["exceptionDetails"]
        );
        Ok(result["result"]["value"].clone())
    }
}

#[derive(Deserialize)]
struct PageData {
    state: String,
    items: Vec<Listing>,
    next: Option<String>,
}

impl BrowserSource {
    pub fn connect(page_delay: Duration) -> Result<Self> {
        verify_profile()?;
        let http = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()?;
        let version: Value = http
            .get(format!("{CDP}/json/version"))
            .send()
            .context("Chromium CDP unavailable; start the browser as described in README.md")?
            .error_for_status()?
            .json()?;
        let mut browser = Cdp::connect(
            version["webSocketDebuggerUrl"]
                .as_str()
                .context("No browser websocket")?,
        )?;
        let target = browser.call(
            "Target.createTarget",
            json!({"url":"about:blank","background":true}),
        )?["targetId"]
            .as_str()
            .context("No target ID")?
            .to_owned();
        let attach = (|| -> Result<Cdp> {
            let pages: Vec<Value> = http
                .get(format!("{CDP}/json/list"))
                .send()?
                .error_for_status()?
                .json()?;
            let page = pages
                .iter()
                .find(|p| p["id"].as_str() == Some(&target))
                .context("Created tab not found")?;
            Cdp::connect(
                page["webSocketDebuggerUrl"]
                    .as_str()
                    .context("No page websocket")?,
            )
        })();
        match attach {
            Ok(page) => Ok(Self {
                browser,
                page,
                target,
                page_delay,
            }),
            Err(error) => {
                let _ = browser.call("Target.closeTarget", json!({"targetId":target}));
                Err(error)
            }
        }
    }

    fn load(&mut self, url: &str) -> Result<PageData> {
        crate::adapters::network::require_direct()?;
        let result = self.page.call("Page.navigate", json!({"url":url}))?;
        ensure!(
            result.get("errorText").is_none(),
            "Navigation failed: {}",
            result["errorText"]
        );
        let loader = result["loaderId"].as_str().map(str::to_owned);
        let deadline = Instant::now() + Duration::from_secs(45);
        let mut last_items = None;
        let mut stable_since = Instant::now();
        loop {
            thread::sleep(Duration::from_millis(750));
            // Page.navigate returns before the new document is installed. Never read old cards.
            if let Some(expected) = &loader {
                let frame = self.page.call("Page.getFrameTree", json!({}))?;
                if frame["frameTree"]["frame"]["loaderId"].as_str() != Some(expected) {
                    ensure!(
                        Instant::now() < deadline,
                        "New search document did not load; no successful scan recorded"
                    );
                    continue;
                }
            }
            let current = self.page.evaluate("location.href")?;
            if current.as_str() == Some(url) {
                let data: PageData = serde_json::from_value(self.page.evaluate(EXTRACT)?)?;
                match data.state.as_str() {
                    "blocked" => bail!(
                        "Avito requests verification or has restricted access. Resolve it in the browser; automatic scanning stopped"
                    ),
                    "login" => bail!("Avito requests login; sign in manually in Chromium"),
                    "ready" | "empty" => {
                        if last_items.as_ref() != Some(&data.items) {
                            stable_since = Instant::now();
                        }
                        if last_items.as_ref() == Some(&data.items)
                            && stable_since.elapsed() >= Duration::from_secs(2)
                        {
                            return Ok(data);
                        }
                        last_items = Some(data.items);
                    }
                    _ => {
                        last_items = None;
                    }
                }
            } else if current
                .as_str()
                .is_some_and(|s| s.contains("/login") || s.contains("/registration"))
            {
                bail!("Avito requests login; sign in manually in Chromium");
            }
            ensure!(
                Instant::now() < deadline,
                "Search did not load or redirected to a different URL; no successful scan recorded"
            );
        }
    }
}

impl ListingSource for BrowserSource {
    fn collect(&mut self, search: &SearchUrl, max_pages: u32) -> Result<Vec<Listing>> {
        ensure!(max_pages > 0, "Page limit must be positive");
        let mut items = Vec::new();
        let mut seen = HashSet::new();
        for number in 1..=max_pages {
            if number > 1 {
                thread::sleep(self.page_delay);
            }
            let data = self.load(&search.page(number))?;
            let mut added = 0;
            for listing in data.items {
                listing.validate()?;
                if seen.insert(listing.id.clone()) {
                    added += 1;
                    items.push(listing);
                }
            }
            if added == 0 || data.next.is_none() {
                break;
            }
        }
        Ok(items)
    }
}

impl Drop for BrowserSource {
    fn drop(&mut self) {
        let _ = self
            .browser
            .call("Target.closeTarget", json!({"targetId": self.target}));
    }
}

pub(crate) fn verify_profile() -> Result<()> {
    // Do not silently attach to another Chrome or a different signed-in profile.
    let found = std::fs::read_dir("/proc")?
        .filter_map(Result::ok)
        .any(|entry| {
            if !entry
                .file_name()
                .to_string_lossy()
                .bytes()
                .all(|b| b.is_ascii_digit())
            {
                return false;
            }
            let Ok(bytes) = std::fs::read(entry.path().join("cmdline")) else {
                return false;
            };
            let raw_args: Vec<_> = bytes
                .split(|b| *b == 0)
                .filter_map(|b| std::str::from_utf8(b).ok())
                .collect();
            // Snap can expose the full command as a single argv entry in /proc.
            let args: Vec<&str> = if raw_args.len() <= 2 {
                raw_args
                    .first()
                    .copied()
                    .unwrap_or("")
                    .split_ascii_whitespace()
                    .collect()
            } else {
                raw_args
            };
            args.iter()
                .any(|a| a.starts_with("/snap/chromium/") || *a == "/snap/bin/chromium")
                && args.contains(&format!("--user-data-dir={PROFILE}").as_str())
                && args.contains(&"--remote-debugging-port=9222")
                && !args.iter().any(|a| a.starts_with("--type="))
        });
    ensure!(
        found && Path::new(PROFILE).is_dir(),
        "Expected Snap Chromium profile is not running. Start it using README.md; the bot will not substitute another browser"
    );
    Ok(())
}

impl BrowserSource {
    pub fn describe(&mut self, listing: &Listing) -> Result<Value> {
        crate::adapters::network::require_direct()?;
        listing.validate()?;
        let result = self
            .page
            .call("Page.navigate", json!({"url":listing.url}))?;
        ensure!(result.get("errorText").is_none(), "Cannot load listing");
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            thread::sleep(Duration::from_secs(1));
            let result=self.page.evaluate(r#"(()=>{const body=document.body?.innerText||'';if(/доступ ограничен|подтвердите, что вы не робот|captcha/i.test(body+' '+document.title))return {blocked:true};const node=document.querySelector('[data-marker="item-view/item-description"], [itemprop="description"]');return {url:location.href,description:node?.innerText||'',title:document.querySelector('h1')?.innerText||'',ready:!!node}})()"#)?;
            ensure!(
                result["blocked"] != true,
                "Авито ограничил доступ; требуется ручная проверка"
            );
            if result["ready"] == true && result["url"].as_str() == Some(&listing.url) {
                return Ok(result);
            }
            ensure!(
                Instant::now() < deadline,
                "Listing description unavailable; no details recorded"
            );
        }
    }
}

impl crate::purchase::jobs::CatalogSource for BrowserSource {
    fn search_page(&mut self, url: &str, page: u32) -> Result<(Vec<Listing>, bool)> {
        let search = SearchUrl::parse(url)?;
        let data = self.load(&search.page(page))?;
        Ok((data.items, data.next.is_some()))
    }
    fn detail(&mut self, id: &str, url: &str) -> Result<Value> {
        self.describe(&Listing {
            id: id.into(),
            title: "Карточка".into(),
            url: url.into(),
            price: None,
            location: String::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires existing Snap Chromium; all messenger calls are local fakes in a new blank tab"]
    fn changed_message_body_prevents_send_in_javascript() {
        use crate::purchase::sync::{DispatchResult, SendIntent, message_key};
        let mut source = BrowserSource::connect(Duration::ZERO).unwrap();
        let original = json!({"id":"1","body":{"text":"old"},"fromUid":"seller","type":"text","created":1,"isDeleted":false});
        let intent = SendIntent {
            chat_id: "fake-chat".into(),
            text: "question".into(),
            random_id: "fake-random".into(),
            expected_head: vec![message_key(&original).unwrap()],
        };
        source.page.evaluate(&format!("window.__head=[{}];window.__calls=0;window.__avitoAssistant={{client:{{request:async()=>({{result:{{items:window.__head}}}})}},store:{{getState:()=>({{}})}}}};window.__avitoAssistantRequire=()=>({{W:async()=>{{window.__calls++;return {{result:{{id:'sent-fake'}}}}}}}})",original)).unwrap();
        source
            .page
            .evaluate("window.__head[0].body.text='manual edit'")
            .unwrap();
        let result = source
            .page
            .evaluate(&super::super::messenger::conditional_send_script(&intent).unwrap())
            .unwrap();
        assert!(matches!(
            serde_json::from_value::<DispatchResult>(result).unwrap(),
            DispatchResult::Changed { .. }
        ));
        assert_eq!(source.page.evaluate("window.__calls").unwrap(), json!(0));
        source
            .page
            .evaluate("window.__head[0].body.text='old'")
            .unwrap();
        let result = source
            .page
            .evaluate(&super::super::messenger::conditional_send_script(&intent).unwrap())
            .unwrap();
        assert!(matches!(
            serde_json::from_value::<DispatchResult>(result).unwrap(),
            DispatchResult::Sent { .. }
        ));
        assert_eq!(source.page.evaluate("window.__calls").unwrap(), json!(1));
    }
    #[test]
    #[ignore = "requires the running authorized Snap Chromium; touches only a new fixture tab"]
    fn extracts_fixture_and_detects_blocking_in_real_chromium() {
        let mut source = BrowserSource::connect(Duration::ZERO).unwrap();
        source.page.call("Page.enable", json!({})).unwrap();
        let frame = source.page.call("Page.getFrameTree", json!({})).unwrap()["frameTree"]["frame"]
            ["id"]
            .clone();
        source
            .page
            .call(
                "Page.setDocumentContent",
                json!({"frameId": frame, "html": include_str!("../../tests/fixtures/search.html")}),
            )
            .unwrap();
        let data: PageData =
            serde_json::from_value(source.page.evaluate(EXTRACT).unwrap()).unwrap();
        assert_eq!(data.state, "ready");
        assert_eq!(data.items.len(), 2);
        assert_eq!(data.items[0].id, "123456789");
        assert_eq!(data.items[0].price, Some(42900));
        assert_eq!(data.items[1].price, Some(0));
        assert!(!data.items[0].url.contains('?'));
        assert!(data.next.is_some());
        source
            .page
            .evaluate("document.body.innerHTML = '<h1>Доступ ограничен</h1>'")
            .unwrap();
        let data: PageData =
            serde_json::from_value(source.page.evaluate(EXTRACT).unwrap()).unwrap();
        assert_eq!(data.state, "blocked");
        source
            .page
            .evaluate("document.body.innerHTML = '<p>Ничего не найдено</p>'")
            .unwrap();
        let data: PageData =
            serde_json::from_value(source.page.evaluate(EXTRACT).unwrap()).unwrap();
        assert_eq!(data.state, "empty");
    }
}
