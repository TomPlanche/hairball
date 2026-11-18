# hairball

hairball is a Rust CLI. It collects my public activity from many platforms into one SQLite file. Then it exports a JSON file for a GitHub-style calendar heatmap.

The sources are:

- ATProto (Bluesky, Tangled, standard.site, and other apps).
- Gitea, Forgejo and Codeberg.
- GitHub.
- Last.fm.
- Bluesky bookmarks.

## Requirements

- Rust 1.85 or later (edition 2024).

## Build

```bash
cargo build --release
```

The binary is `target/release/hairball`.

## Quick start

1. Copy `config.example.toml` to `config.toml`. Keep only the sections of the sources that you use.
2. Copy `.env.example` to `.env`. Write the secrets of your sources in it.
3. Make sure that the secrets are correct:

   ```bash
   cargo run -- check
   ```

4. Read all the history of your sources:

   ```bash
   cargo run --release -- sync --full
   ```

5. Write the calendar:

   ```bash
   cargo run -- export --year 2026 --out calendar.json
   ```

## Usage

```
hairball [--config <path>] <command>

  sync [--source <id>] [--full] [--dry-run]
                                  Run all enabled sources, or only one.
  export [--year <YYYY>] [--out <path>] [--dry-run]
                                  Write the calendar JSON (default: `out` in
                                  `[export]`, else stdout).
  status                          Show each source: last run, cursor and row counts.
  check                           Check that the secrets are set, valid and accepted.
  purge --kind <kind|pattern> [--dry-run]
                                  Delete all rows of one kind, or of the kinds
                                  that match a pattern with `*`.
```

Run `hairball --help` or `hairball <command> --help` for the full list of options.

The 3 commands that write (`sync`, `export` and `purge`) have a `--dry-run` option. A dry run shows what the command will do, and changes nothing.

The CLI writes its logs to stderr. Set the level with `RUST_LOG` (default: `info`), for example `RUST_LOG=hairball=debug`.

## Documentation

| File | Content |
| ---- | ------- |
| [docs/configuration.md](docs/configuration.md) | The config file, the secrets, the database and the time zones. |
| [docs/sync.md](docs/sync.md) | The sync modes, the deletions, `purge`, and the details of each source. |
| [docs/export.md](docs/export.md) | The destination and the format of the calendar JSON. |
| [docs/deployment.md](docs/deployment.md) | The installation on a Linux server with systemd timers. |
| [docs/testing.md](docs/testing.md) | The automatic tests and the manual checks of each feature. |

## Test

```bash
cargo test
```

```bash
cargo clippy --all-targets -- -D warnings
```
