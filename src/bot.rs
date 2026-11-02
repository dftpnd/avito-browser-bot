//! Telegram-independent command controller and monitoring scheduler.
use crate::{
    application,
    domain::{
        Change, ChangeKind, SearchUrl,
        bot::{BotCommand, BotState, HELP, Subscription},
    },
    ports::{BotRepository, BotTransport, ListingRepository, ListingSource},
};
use anyhow::{Result, ensure};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn cut(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

pub fn format_change(change: &Change) -> String {
    let title = match change.kind {
        ChangeKind::New => "Новое объявление",
        ChangeKind::PriceChanged => "Изменение цены",
    };
    let price = change
        .listing
        .price
        .map(|p| format!("{p} ₽"))
        .unwrap_or_else(|| "не указана".into());
    let old = if change.kind == ChangeKind::PriceChanged {
        format!(
            " (было: {})",
            change
                .previous_price
                .map(|p| format!("{p} ₽"))
                .unwrap_or_else(|| "не указана".into())
        )
    } else {
        String::new()
    };
    format!(
        "{title}\n{}\nЦена: {price}{old}\n{}\n{}",
        cut(&change.listing.title, 500),
        cut(&change.listing.location, 300),
        cut(&change.listing.url, 2000)
    )
}

/// Acknowledge only after delivery. A crash after send may cause a duplicate, never silent loss.
pub fn deliver_pending(
    transport: &mut impl BotTransport,
    repository: &mut impl BotRepository,
    state: &mut BotState,
) -> Result<()> {
    for (id, change) in repository.changes_after(state.change_cursor, 20)? {
        transport.send(state.owner_id, &format_change(&change))?;
        let mut acknowledged = state.clone();
        acknowledged.change_cursor = id;
        repository.save_bot_state(&acknowledged)?;
        *state = acknowledged;
        thread::sleep(Duration::from_millis(1100));
    }
    Ok(())
}

pub fn run<T, R, S>(
    transport: &mut T,
    repository: &mut R,
    owner_id: i64,
    make_source: impl FnMut() -> Result<S>,
    running: Arc<AtomicBool>,
) -> Result<()>
where
    T: BotTransport,
    R: ListingRepository + BotRepository,
    S: ListingSource,
{
    run_with_assistant(
        transport,
        repository,
        owner_id,
        make_source,
        running,
        &mut crate::purchase::controller::NoAssistant,
    )
}

pub fn run_with_assistant<T, R, S>(
    transport: &mut T,
    repository: &mut R,
    owner_id: i64,
    mut make_source: impl FnMut() -> Result<S>,
    running: Arc<AtomicBool>,
    assistant: &mut impl crate::purchase::controller::Assistant,
) -> Result<()>
where
    T: BotTransport,
    R: ListingRepository + BotRepository,
    S: ListingSource,
{
    ensure!(owner_id > 0, "Telegram owner ID must be positive");
    let mut state = match repository.bot_state()? {
        Some(state) => {
            ensure!(
                state.owner_id == owner_id,
                "Owner differs from stored bot state; use a separate database"
            );
            state
        }
        None => BotState {
            owner_id,
            change_cursor: repository.latest_change_id()?,
            ..BotState::default()
        },
    };
    repository.save_bot_state(&state)?;
    let mut source = None;
    while running.load(Ordering::SeqCst) {
        match assistant.pending_replies() {
            Ok(events) => {
                for (id, text) in events {
                    if let Err(error) = transport
                        .send_reply(owner_id, &text)
                        .and_then(|_| assistant.acknowledge(id))
                    {
                        eprintln!("Assistant notification deferred: {error}");
                        break;
                    }
                    thread::sleep(Duration::from_millis(1100));
                }
            }
            Err(error) => eprintln!("Assistant notifications unavailable: {error}"),
        }

        if let Err(error) = deliver_pending(transport, repository, &mut state) {
            eprintln!("Notification delivery failed; retrying: {error}");
            pause(&running, 5);
            continue;
        }
        match transport.receive(state.offset) {
            Ok(updates) => {
                for message in updates {
                    if !running.load(Ordering::SeqCst) {
                        break;
                    }
                    if message.authorized(owner_id) && !message.text.is_empty() {
                        if let Some(callback) = &message.callback_id
                            && let Err(error) = transport.answer_callback(callback)
                        {
                            eprintln!("Callback acknowledgment deferred: {error}");
                        }

                        let result = match assistant.reply(&message.text) {
                            Ok(Some(reply)) => transport.send_reply(owner_id, &reply),
                            Err(error) => Err(error),
                            Ok(None) => BotCommand::parse(&message.text).and_then(|command| match command {
                        BotCommand::Help => transport.send(owner_id, HELP),
                        BotCommand::Status => transport.send(owner_id, &state.subscription.as_ref().map(|s| format!("Мониторинг активен: {}\nСтраниц: {}, интервал: {} с", s.url, s.pages, s.interval)).unwrap_or_else(|| "Мониторинг остановлен. /help — команды".into())),
                        BotCommand::Stop => {
                            state.subscription = None;
                            repository.save_bot_state(&state)?;
                            transport.send(owner_id, "Мониторинг остановлен.")
                        }
                        BotCommand::Watch { search, pages, interval } => {
                            ensure!(assistant.allows_avito(),"Очередь Авито на паузе; /resume — продолжить");
                            state.subscription = Some(Subscription { url: search.as_str().into(), pages, interval, next_run: now() });
                            repository.save_bot_state(&state)?;
                            transport.send(owner_id, "Мониторинг включён. Начинаю первый проход.")
                        }
                        BotCommand::Scan { search, pages } => {
                            ensure!(assistant.allows_avito(),"Очередь Авито на паузе; /resume — продолжить");
                            transport.send(owner_id, "Начинаю сканирование.")?;
                            let result = perform_scan(&mut source, &mut make_source, repository, &search, pages)?;
                            transport.send(owner_id, &result)
                        }
                    }),
                        };
                        if let Err(error) = result {
                            // Notification failures do not acknowledge the command: retry after reconnect.
                            if let Err(error) = transport.send(
                                owner_id,
                                &format!("Ошибка: {}", cut(&error.to_string(), 3000)),
                            ) {
                                eprintln!("Cannot send command result: {error}");
                                pause(&running, 5);
                                break;
                            }
                        }
                    }
                    state.offset = message.update_id + 1;
                    repository.save_bot_state(&state)?;
                }
            }
            Err(error) => {
                eprintln!("Telegram polling failed; retrying: {error}");
                pause(&running, 5);
                continue;
            }
        }
        if !running.load(Ordering::SeqCst) {
            break;
        }
        if !assistant.allows_avito() {
            continue;
        }
        if let Some(subscription) = state.subscription.clone().filter(|s| s.next_run <= now()) {
            let result = SearchUrl::parse(&subscription.url).and_then(|search| {
                perform_scan(
                    &mut source,
                    &mut make_source,
                    repository,
                    &search,
                    subscription.pages,
                )
            });
            match result {
                Ok(_) => {
                    if let Some(subscription) = &mut state.subscription {
                        subscription.next_run = now() + subscription.interval as i64;
                    }
                    repository.save_bot_state(&state)?;
                }
                Err(error) => {
                    state.subscription = None;
                    repository.save_bot_state(&state)?;
                    transport.send(owner_id, &format!("Мониторинг остановлен: {}\nПосле устранения причины запусти /watch снова.", cut(&error.to_string(), 2800)))?;
                }
            }
        }
    }
    Ok(())
}
fn perform_scan<R: ListingRepository, S: ListingSource>(
    source: &mut Option<S>,
    make_source: &mut impl FnMut() -> Result<S>,
    repository: &mut R,
    search: &SearchUrl,
    pages: u32,
) -> Result<String> {
    if source.is_none() {
        *source = Some(make_source()?);
    }
    match application::scan(
        source.as_mut().expect("created above"),
        repository,
        search,
        pages,
    ) {
        Ok(result) => Ok(format!(
            "Просмотрено: {}. Новых объявлений и изменений цены: {}.",
            result.count,
            result.changes.len()
        )),
        Err(error) => {
            *source = None;
            Err(error)
        }
    }
}
fn pause(running: &AtomicBool, seconds: u32) {
    for _ in 0..seconds * 5 {
        if !running.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Listing, ScanResult, bot::IncomingMessage};
    #[derive(Default)]
    struct Memory {
        state: Option<BotState>,
        changes: Vec<(i64, Change)>,
        scan_calls: usize,
    }
    impl BotRepository for Memory {
        fn bot_state(&self) -> Result<Option<BotState>> {
            Ok(self.state.clone())
        }
        fn save_bot_state(&mut self, state: &BotState) -> Result<()> {
            self.state = Some(state.clone());
            Ok(())
        }
        fn latest_change_id(&self) -> Result<i64> {
            Ok(self.changes.last().map(|c| c.0).unwrap_or(0))
        }
        fn changes_after(&self, after: i64, limit: u32) -> Result<Vec<(i64, Change)>> {
            Ok(self
                .changes
                .iter()
                .filter(|c| c.0 > after)
                .take(limit as usize)
                .cloned()
                .collect())
        }
    }
    impl ListingRepository for Memory {
        fn record_scan(&mut self, _: &SearchUrl, _: &[Listing], _: i64) -> Result<ScanResult> {
            self.scan_calls += 1;
            anyhow::bail!("unexpected scan")
        }
        fn listings(&self) -> Result<Vec<Listing>> {
            Ok(vec![])
        }
    }
    struct FakeTransport {
        fail: bool,
        sent: Vec<String>,
        message: Option<IncomingMessage>,
        running: Arc<AtomicBool>,
    }
    impl BotTransport for FakeTransport {
        fn receive(&mut self, _: i64) -> Result<Vec<IncomingMessage>> {
            if let Some(message) = self.message.take() {
                Ok(vec![message])
            } else {
                self.running.store(false, Ordering::SeqCst);
                Ok(vec![])
            }
        }
        fn send(&mut self, _: i64, text: &str) -> Result<()> {
            ensure!(!self.fail, "network unavailable");
            self.sent.push(text.into());
            Ok(())
        }
    }
    fn transport(fail: bool) -> FakeTransport {
        FakeTransport {
            fail,
            sent: vec![],
            message: None,
            running: Arc::new(AtomicBool::new(true)),
        }
    }
    fn event() -> Change {
        Change {
            kind: ChangeKind::New,
            listing: Listing {
                id: "123".into(),
                title: "A < B & C".into(),
                url: "https://avito.ru/moskva/test_123".into(),
                price: None,
                location: "Москва".into(),
            },
            previous_price: None,
        }
    }
    #[test]
    fn failed_delivery_keeps_event_pending_and_retry_acknowledges_it() {
        let mut store = Memory {
            changes: vec![(1, event())],
            ..Memory::default()
        };
        let mut state = BotState {
            owner_id: 42,
            ..BotState::default()
        };
        let mut transport = transport(true);
        assert!(deliver_pending(&mut transport, &mut store, &mut state).is_err());
        assert_eq!(state.change_cursor, 0);
        assert!(store.state.is_none());
        transport.fail = false;
        deliver_pending(&mut transport, &mut store, &mut state).unwrap();
        assert_eq!(store.state.as_ref().unwrap().change_cursor, 1);
        deliver_pending(&mut transport, &mut store, &mut state).unwrap();
        assert_eq!(transport.sent.len(), 1);
        assert!(transport.sent[0].contains("A < B & C"));
    }
    struct NeverSource;
    impl ListingSource for NeverSource {
        fn collect(&mut self, _: &SearchUrl, _: u32) -> Result<Vec<Listing>> {
            panic!("unauthorized scan")
        }
    }
    #[test]
    fn unauthorized_command_is_acknowledged_without_reply_or_browser_access() {
        let mut store = Memory::default();
        let mut transport = transport(false);
        transport.message = Some(IncomingMessage {
            update_id: 6,
            callback_id: None,
            user_id: 99,
            chat_id: 99,
            private: true,
            text: "/scan https://avito.ru/moskva".into(),
        });
        let running = transport.running.clone();
        run(
            &mut transport,
            &mut store,
            42,
            || -> Result<NeverSource> { panic!("browser must not open") },
            running,
        )
        .unwrap();
        assert!(transport.sent.is_empty());
        assert_eq!(store.scan_calls, 0);
        assert_eq!(store.state.unwrap().offset, 7);
    }
}
