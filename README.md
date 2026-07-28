# hairball

hairball is a Rust CLI that collects my public activity from many platforms into one SQLite file. It then exports a JSON file for a GitHub-style calendar heatmap.

The planned sources are ATProto (Bluesky, Tangled, standard.site), Gitea/Forgejo/Codeberg, GitHub, Last.fm and Bluesky bookmarks.

## Status

The project is at an early stage. The CLI loads and validates the config file, then opens the SQLite database and runs the migrations. The `status` command lists the configured sources with their last run, row counts and cursor. The other commands return a "not implemented yet" error.

## Requirements

- Rust 1.85 or later (edition 2024).

## Build

```bash
cargo build --release
```

The binary is `target/release/hairball`.

## Usage

```
hairball [--config <path>] <command>

  sync [--source <id>] [--full]   Run all enabled sources, or only one.
  export [--year <YYYY>] [--out <path>]
                                  Write the calendar JSON (default: stdout).
  status                          Show each source: last run, cursor and row counts.
  purge --kind <kind>             Delete all rows of one kind.
```

Run `hairball --help` or `hairball <command> --help` for the full list of options.

## Configuration

The CLI reads `config.toml` in the current directory. Use `--config <path>` to read another file.

1. Copy `config.example.toml` to `config.toml`.
2. Edit the sections of the sources you use. Remove the other sections.

The CLI validates the config at startup. It reports all the errors at once, with the TOML path of each error. Git ignores `config.toml`.

## Database

The CLI stores all data in one SQLite file: `database` in the config (default: `hairball.db`). The CLI creates the file if it does not exist, and applies the migrations of `migrations/` at each start. A backup is a copy of the file. Git ignores `*.db` files.

The database has 3 tables:

- `events`: one row per record, for the sources that give single events (ATProto, Last.fm, bookmarks).
- `daily_counts`: one count per day, for the sources that give only aggregates (GitHub, Gitea).
- `sync_state`: the cursor and the last run of each source.

All times are unix seconds, UTC.

## Secrets

The CLI reads secrets from environment variables only. The config file holds the variable names, never the values.

1. Copy `.env.example` to `.env`.
2. Fill in the values you need. Leave the others empty.
3. Export the variables before you run the CLI. The CLI does not read `.env` itself.

```bash
set -a; source .env; set +a
```

Do not commit `.env`. Git ignores it.

## Test

Run the checks:

```bash
cargo test
```

```bash
cargo clippy --all-targets -- -D warnings
```

Then try the CLI by hand:

```bash
cargo run -- --help
```

```bash
cargo run -- --config config.example.toml status
```

This command creates `hairball.db` in the current directory. It must list 6 sources: `atproto`, `bsky_bookmarks` (disabled), `gitea:codeberg`, `gitea:forge-perso` (skipped, mirror), `github` and `lastfm`. Each source shows `never` as last run and `0` rows.

To see the stats change, add rows by hand, then run `status` again:

```bash
sqlite3 hairball.db "INSERT INTO daily_counts VALUES ('github', '2026-07-20', 3); INSERT INTO sync_state VALUES ('github', '2026', 1753450000);"
```

The `github` line must then show `2025-07-25T13:26:40Z`, `1` day and the cursor `2026`. Delete `hairball.db` after the test.

To see the validation, put a bad value in a copy of the config, for example `timezone = ""`. The command must then print the error with its TOML path and exit with code 1.
