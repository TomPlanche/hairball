use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use hairball::{
    check::{self, Endpoints, Status},
    config::Config,
    db, http,
    sources::{self, Planned, SyncContext},
};
use jiff::Timestamp;
use sqlx::SqlitePool;
use tracing::{error, info};
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
    init_logs();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?
        .block_on(run(Cli::parse()))
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
fn init_logs() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let config = Config::load(&cli.config)
        .with_context(|| format!("cannot load the config file {}", cli.config.display()))?;

    match cli.command {
        Command::Sync { source, full } => {
            let db = open_db(&config).await?;
            sync(&config, &db, source.as_deref(), full).await
        }
        Command::Export { year, out } => {
            bail!("`export` is not implemented yet (year: {year:?}, out: {out:?})")
        }
        Command::Status => {
            let db = open_db(&config).await?;
            status(&config, &db).await
        }
        Command::Check => check(&config).await,
        Command::Purge { kind } => bail!("`purge` is not implemented yet (kind: {kind})"),
    }
}

/// Runs each planned source, or only `only`. One failing source does not stop
/// the others: the command fails at the end.
async fn sync(
    config: &Config,
    db: &SqlitePool,
    only: Option<&str>,
    full: bool,
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
    let ctx = SyncContext {
        db,
        client: &client,
        now: Timestamp::now(),
        full,
    };

    let mut failed = Vec::new();
    for planned in &planned {
        match planned {
            Planned::Skip { id, reason } => info!(source = %id, "skipped: {reason}"),
            Planned::Run(source) => {
                let id = source.id();
                info!(source = %id, full, "sync started");
                match source.sync(&ctx).await {
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
