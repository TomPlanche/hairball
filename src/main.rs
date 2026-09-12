use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use hairball::{
    check::{self, Endpoints, Status},
    config::Config,
    db, export, http,
    progress::{self, Progress},
    sources::{self, Planned, SyncContext, atproto::Atproto},
    time,
};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use jiff::{Timestamp, civil::Date};
use sqlx::SqlitePool;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

/// Collect my public activity from many platforms into one SQLite file.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(long, global = true, default_value = "config.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run all enabled sources, or only one.
    Sync {
        /// Run only the source with this id (for example `gitea:codeberg`).
        #[arg(long)]
        source: Option<String>,

        /// Ignore the cursors and reconcile deletions.
        #[arg(long)]
        full: bool,
    },

    /// Write the calendar JSON.
    Export {
        /// Export only this year.
        #[arg(long)]
        year: Option<i16>,

        /// Output file (default: stdout).
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Show each source: last run, cursor and row counts.
    Status,

    /// Check that the secrets are set, valid and accepted by each API.
    Check,

    /// Delete all rows of one kind (manual cleanup).
    Purge {
        /// The kind to delete (for example `app.bsky.feed.like`).
        #[arg(long)]
        kind: String,
    },
}

fn main() -> anyhow::Result<()> {
    // Load `.env` before the runtime starts: setting environment variables is
    // only safe while the process has a single thread.
    load_dotenv()?;

    // Progress bars only in a terminal: not under systemd, not in a pipe.
    let bars = std::io::stderr().is_terminal().then(MultiProgress::new);
    init_logs(bars.clone());

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?
        .block_on(run(Cli::parse(), bars))
}

/// Reads `.env` (in the current directory or a parent), if it exists.
///
/// A variable that is already set keeps its value: systemd's `EnvironmentFile`
/// and the shell win over `.env`.
fn load_dotenv() -> anyhow::Result<()> {
    match dotenvy::dotenv() {
        Ok(_) => Ok(()),
        Err(error) if error.not_found() => Ok(()),
        Err(error) => Err(error).context("cannot read the .env file"),
    }
}

/// Logs go to stderr, so that `export` can write JSON to stdout.
/// `RUST_LOG` sets the level (default: `info`), for example `RUST_LOG=hairball=debug`.
fn init_logs(bars: Option<MultiProgress>) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(move || LogWriter { bars: bars.clone() })
        .init();
}

/// Writes the logs to stderr. With progress bars, it hides them during the
/// write, so that a log line never mixes with a bar.
struct LogWriter {
    bars: Option<MultiProgress>,
}

impl io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &self.bars {
            Some(bars) => bars.suspend(|| io::stderr().write(buf)),
            None => io::stderr().write(buf),
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match &self.bars {
            Some(bars) => bars.suspend(|| io::stderr().write_all(buf)),
            None => io::stderr().write_all(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

/// Draws the progress of one source as a bar (known length) or a spinner.
#[derive(Debug)]
struct BarProgress {
    bar: ProgressBar,
}

impl BarProgress {
    fn new(bars: &MultiProgress, id: &str) -> Self {
        let bar = bars.add(ProgressBar::new_spinner());
        bar.set_prefix(id.to_owned());
        bar.set_style(Self::spinner_style());
        bar.enable_steady_tick(Duration::from_millis(120));
        Self { bar }
    }

    fn spinner_style() -> ProgressStyle {
        ProgressStyle::with_template("{spinner} {prefix:.bold} [{elapsed}] {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner())
    }

    fn bar_style() -> ProgressStyle {
        ProgressStyle::with_template(
            "{spinner} {prefix:.bold} [{elapsed}] [{bar:30}] {pos}/{len} {msg}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> ")
    }
}

impl Progress for BarProgress {
    fn set_length(&self, length: Option<u64>) {
        match length {
            Some(length) => {
                self.bar.set_length(length);
                self.bar.set_style(Self::bar_style());
            }
            None => {
                self.bar.unset_length();
                self.bar.set_style(Self::spinner_style());
            }
        }
    }

    fn set_position(&self, position: u64) {
        self.bar.set_position(position);
    }

    fn set_message(&self, message: String) {
        self.bar.set_message(message);
    }
}

async fn run(cli: Cli, bars: Option<MultiProgress>) -> anyhow::Result<()> {
    let config = Config::load(&cli.config)
        .with_context(|| format!("cannot load the config file {}", cli.config.display()))?;

    match cli.command {
        Command::Sync { source, full } => {
            let db = open_db(&config).await?;
            sync(&config, &db, source.as_deref(), full, bars.as_ref()).await
        }
        Command::Export { year, out } => {
            let db = open_db(&config).await?;
            export(&config, &db, year, out.as_deref()).await
        }
        Command::Status => {
            let db = open_db(&config).await?;
            status(&config, &db).await
        }
        Command::Check => check(&config).await,
        Command::Purge { kind } => {
            let db = open_db(&config).await?;
            purge(&config, &db, &kind).await
        }
    }
}

/// Deletes all the events of one kind, in all sources.
async fn purge(config: &Config, db: &SqlitePool, kind: &str) -> anyhow::Result<()> {
    let rows = db::purge_kind(db, None, kind).await?;
    println!("deleted {rows} events of kind `{kind}`");

    // A purge of a kind that the config still wants only lasts until the next sync.
    if let Some(atproto) = &config.atproto
        && Atproto::new(atproto).wants(kind)
        && kind.contains('.')
    {
        warn!(
            %kind,
            "the next sync adds this kind again: add it to `exclude` in `[atproto]` to keep it out"
        );
    }
    Ok(())
}

/// Runs each planned source, or only `only`. One failing source does not stop
/// the others: the command fails at the end.
async fn sync(
    config: &Config,
    db: &SqlitePool,
    only: Option<&str>,
    full: bool,
    bars: Option<&MultiProgress>,
) -> anyhow::Result<()> {
    let mut planned = sources::plan(config);
    if let Some(only) = only {
        let ids: Vec<_> = planned.iter().map(Planned::id).collect();
        if !ids.iter().any(|id| id == only) {
            bail!(
                "unknown source `{only}`: the configured sources are {}",
                ids.join(", ")
            );
        }
        planned.retain(|p| p.id() == only);
    }

    let client = http::client().context("cannot build the HTTP client")?;
    let now = Timestamp::now();

    let mut failed = Vec::new();
    for planned in &planned {
        match planned {
            Planned::Skip { id, reason } => info!(source = %id, "skipped: {reason}"),
            Planned::Run(source) => {
                let id = source.id();
                info!(source = %id, full, "sync started");
                let bar = bars.map(|bars| Arc::new(BarProgress::new(bars, &id)));
                let progress: Arc<dyn Progress> = match &bar {
                    Some(bar) => Arc::clone(bar) as Arc<dyn Progress>,
                    None => progress::none(),
                };
                let ctx = SyncContext {
                    db,
                    client: &client,
                    now,
                    full,
                    progress: &progress,
                };
                let result = source.sync(&ctx).await;
                // The log line below keeps the result: the bar can go.
                if let Some(bar) = &bar {
                    bar.bar.finish_and_clear();
                }
                match result {
                    Ok(report) => info!(source = %id, "sync done: {report}"),
                    Err(e) => {
                        error!(source = %id, "sync failed: {}", error_chain(&e));
                        failed.push(id);
                    }
                }
            }
        }
    }

    if !failed.is_empty() {
        bail!("the sync failed for: {}", failed.join(", "));
    }
    Ok(())
}

/// Joins an error and all its sources: `a: b: c`.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Runs the credential checks and fails if one of them fails.
async fn check(config: &Config) -> anyhow::Result<()> {
    let client = http::client().context("cannot build the HTTP client")?;
    let env = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    let checks = check::run(config, env, &client, &Endpoints::default()).await;

    if checks.is_empty() {
        println!("no source with a secret is configured");
        return Ok(());
    }

    let mut rows = vec![["SOURCE", "RESULT", "DETAIL"].map(str::to_owned)];
    rows.extend(
        checks
            .iter()
            .map(|c| [c.source.to_owned(), c.status.to_string(), c.detail.clone()]),
    );
    print_table(&rows);

    let failed = checks.iter().filter(|c| c.status == Status::Failed).count();
    if failed > 0 {
        bail!("{failed} check(s) failed");
    }
    Ok(())
}

/// Writes the calendar JSON of one year (default: the current year) to `out`,
/// or to stdout.
async fn export(
    config: &Config,
    db: &SqlitePool,
    year: Option<i16>,
    out: Option<&Path>,
) -> anyhow::Result<()> {
    let year = year.unwrap_or_else(|| time::day(Timestamp::now(), &config.time_zone).year());
    let from = Date::new(year, 1, 1).with_context(|| format!("invalid year {year}"))?;
    let to = Date::new(year, 12, 31).with_context(|| format!("invalid year {year}"))?;

    let calendar = export::build(db, &config.time_zone, &config.timezone, from, to)
        .await
        .context("cannot build the calendar")?;
    let json = serde_json::to_string(&calendar)?;

    match out {
        None => println!("{json}"),
        Some(path) => {
            // Write a temporary file, then rename it: a reader never sees a
            // half-written file.
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, json)
                .with_context(|| format!("cannot write {}", tmp.display()))?;
            std::fs::rename(&tmp, path)
                .with_context(|| format!("cannot replace {}", path.display()))?;
            info!(
                path = %path.display(),
                days = calendar.days.len(),
                sources = calendar.sources.len(),
                "calendar written"
            );
        }
    }
    Ok(())
}

async fn open_db(config: &Config) -> anyhow::Result<SqlitePool> {
    db::open(&config.database)
        .await
        .with_context(|| format!("cannot open the database {}", config.database.display()))
}

/// Prints each configured source: state, last run, row counts and cursor.
async fn status(config: &Config, db: &SqlitePool) -> anyhow::Result<()> {
    println!("database: {}", config.database.display());
    println!("timezone: {}", config.timezone);
    println!();

    let sources = config.sources();
    if sources.is_empty() {
        println!("no source configured");
        return Ok(());
    }

    let mut rows = vec![
        [
            "SOURCE",
            "STATE",
            "LAST RUN (UTC)",
            "EVENTS",
            "DELETED",
            "DAYS",
            "CURSOR",
        ]
        .map(str::to_owned),
    ];
    for source in &sources {
        let stats = db::source_stats(db, &source.id).await?;
        rows.push([
            source.id.clone(),
            source.state.to_string(),
            stats
                .state
                .last_run
                .map_or_else(|| "never".to_owned(), format_time),
            stats.events.to_string(),
            stats.deleted.to_string(),
            stats.days.to_string(),
            stats.state.cursor.unwrap_or_else(|| "-".to_owned()),
        ]);
    }
    print_table(&rows);
    Ok(())
}

/// Formats unix seconds as an RFC 3339 UTC time.
fn format_time(seconds: i64) -> String {
    Timestamp::from_second(seconds).map_or_else(|_| seconds.to_string(), |t| t.to_string())
}

/// Prints rows as left-aligned columns.
fn print_table<const N: usize>(rows: &[[String; N]]) {
    let mut widths = [0; N];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    for row in rows {
        let line: Vec<_> = row
            .iter()
            .zip(widths)
            .map(|(cell, width)| format!("{cell:width$}"))
            .collect();
        println!("{}", line.join("  ").trim_end());
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::Cli;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
