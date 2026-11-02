use anyhow::{Result, ensure};
use avito_browser_bot::{
    adapters::{browser::BrowserSource, report, sqlite::SqliteRepository},
    application,
    domain::SearchUrl,
    ports::ListingRepository,
};
use clap::{Parser, Subcommand};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(version, about = "Сканирование объявлений через существующий Chromium")]
struct Cli {
    #[arg(long, global = true, default_value = "data/listings.sqlite")]
    database: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Один проход по готовой ссылке поиска
    Scan {
        url: String,
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=100))]
        pages: u32,
    },
    /// Периодический сбор; Ctrl+C завершает после текущего прохода
    Watch {
        url: String,
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=100))]
        pages: u32,
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(60..))]
        interval: u64,
    },
    /// Все сохранённые объявления в JSON, без браузера
    List,
    /// Создать локальный HTML-отчёт, без браузера
    Report {
        #[arg(long, default_value = "data/report.html")]
        output: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Validate input before creating a database or opening a browser tab.
    let search = match &cli.command {
        Command::Scan { url, .. } | Command::Watch { url, .. } => Some(SearchUrl::parse(url)?),
        _ => None,
    };
    let mut repository = SqliteRepository::open(&cli.database)?;
    match cli.command {
        Command::List => println!("{}", serde_json::to_string_pretty(&repository.listings()?)?),
        Command::Report { output } => {
            protect_database(&output, &cli.database)?;
            report::write_html(&output, &repository.listings()?)?;
            eprintln!("Отчёт: {}", output.display());
        }
        command => {
            let (pages, interval) = match command {
                Command::Scan { pages, .. } => (pages, None),
                Command::Watch {
                    pages, interval, ..
                } => (pages, Some(interval)),
                _ => unreachable!(),
            };
            let search = search.expect("validated above");
            let running = Arc::new(AtomicBool::new(true));
            let signal = running.clone();
            ctrlc::set_handler(move || {
                signal.store(false, Ordering::SeqCst);
            })?;
            let mut source = BrowserSource::connect(Duration::from_secs(5))?;
            loop {
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                let result = application::scan(&mut source, &mut repository, &search, pages)?;
                // Notifications are durable in SQLite even if stdout/report delivery fails.
                println!("{}", serde_json::to_string(&result)?);
                let output = cli
                    .database
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .join("report.html");

                protect_database(&output, &cli.database)?;
                report::write_html(&output, &repository.listings()?)?;
                let Some(interval) = interval else {
                    break;
                };
                eprintln!(
                    "Просмотрено: {}; изменений: {}. Следующий проход через {} с",
                    result.count,
                    result.changes.len(),
                    interval
                );
                let deadline = Instant::now() + Duration::from_secs(interval);
                while running.load(Ordering::SeqCst) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }
    Ok(())
}

fn protect_database(output: &std::path::Path, database: &std::path::Path) -> Result<()> {
    ensure!(
        output.canonicalize().ok() != Some(database.canonicalize()?),
        "Report path must differ from database path"
    );
    Ok(())
}
