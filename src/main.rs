mod config;

use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};

use crate::config::Config;

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

fn main() -> anyhow::Result<()> {
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
            status(&config);
            Ok(())
        }
        Command::Purge { kind } => bail!("`purge` is not implemented yet (kind: {kind})"),
    }
}

/// Prints the configured sources and their state.
fn status(config: &Config) {
    println!("database: {}", config.database.display());
    println!("timezone: {}", config.timezone);
    println!();

    let sources = config.sources();
    if sources.is_empty() {
        println!("no source configured");
        return;
    }

    let width = sources.iter().map(|s| s.id.len()).max().unwrap_or(0);
    for source in &sources {
        println!("{:width$}  {}", source.id, source.state);
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
