use std::path::PathBuf;

use anyhow::bail;
use clap::{Parser, Subcommand};

/// Collect my public activity from many platforms into one SQLite file.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
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

    match cli.command {
        Command::Sync { source, full } => {
            bail!("`sync` is not implemented yet (source: {source:?}, full: {full})")
        }
        Command::Export { year, out } => {
            bail!("`export` is not implemented yet (year: {year:?}, out: {out:?})")
        }
        Command::Status => bail!("`status` is not implemented yet"),
        Command::Purge { kind } => bail!("`purge` is not implemented yet (kind: {kind})"),
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
