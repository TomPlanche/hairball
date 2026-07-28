use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use hairball::{config::Config, db};
use jiff::Timestamp;
use sqlx::SqlitePool;

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

    /// Delete all rows of one kind (manual cleanup).
    Purge {
        /// The kind to delete (for example `app.bsky.feed.like`).
        #[arg(long)]
        kind: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)
        .with_context(|| format!("cannot load the config file {}", cli.config.display()))?;

    match cli.command {
        Command::Sync { source, full } => {
            bail!("`sync` is not implemented yet (source: {source:?}, full: {full})")
        }
        Command::Export { year, out } => {
            bail!("`export` is not implemented yet (year: {year:?}, out: {out:?})")
        }
        Command::Status => {
            let db = open_db(&config).await?;
            status(&config, &db).await
        }
        Command::Purge { kind } => bail!("`purge` is not implemented yet (kind: {kind})"),
    }
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
