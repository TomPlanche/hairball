# hairball

hairball is a Rust CLI that collects my public activity from many platforms into one SQLite file. It then exports a JSON file for a GitHub-style calendar heatmap.

The planned sources are ATProto (Bluesky, Tangled, standard.site), Gitea/Forgejo/Codeberg, GitHub, Last.fm and Bluesky bookmarks.

## Status

The project is at an early stage. The command-line interface exists, but all commands return a "not implemented yet" error.

## Requirements

- Rust 1.85 or later (edition 2024).

## Build

```bash
cargo build --release
```

The binary is `target/release/hairball`.

## Usage

```
hairball <command>

  sync [--source <id>] [--full]   Run all enabled sources, or only one.
  export [--year <YYYY>] [--out <path>]
                                  Write the calendar JSON (default: stdout).
  status                          Show each source: last run, cursor and row counts.
  purge --kind <kind>             Delete all rows of one kind.
```

Run `hairball --help` or `hairball <command> --help` for the full list of options.

## Secrets

The CLI reads secrets from environment variables only. The config file will hold the variable names, never the values.

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
cargo run -- sync --full
```

The second command must print `Error: `sync` is not implemented yet (source: None, full: true)` and exit with code 1.
