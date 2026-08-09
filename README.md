# hairball

hairball is a Rust CLI that collects my public activity from many platforms into one SQLite file. It then exports a JSON file for a GitHub-style calendar heatmap.

The planned sources are ATProto (Bluesky, Tangled, standard.site), Gitea/Forgejo/Codeberg, GitHub, Last.fm and Bluesky bookmarks.

## Status

The project is at an early stage.

- `sync` works for the ATProto source: it reads all the records of the repo and stores one event per record. It reads all the pages at each run: the incremental mode, the deletions and the purge of disabled kinds come later. The other sources are skipped with a log line.
- `status` lists the configured sources with their last run, row counts and cursor.
- `check` tests the secrets and the ATProto identity against the real APIs.
- `export` and `purge` return a "not implemented yet" error.

## Logs

The CLI writes its logs to stderr. Set the level with `RUST_LOG` (default: `info`):

```bash
RUST_LOG=hairball=debug cargo run -- sync
```

With `debug`, the ATProto adapter logs each collection it reads or skips. A rejected event time (for example a `createdAt` in the future) is a warning, with the URI of the record.

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
  check                           Check that the secrets are set, valid and accepted.
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

## Time

The database stores UTC times only. The CLI groups the events into days only at export time, in the time zone of `timezone` in the config (an IANA name, default: `Europe/Paris`). For example, 23:30 UTC on 15 January is 16 January in Paris. The CLI reads the time zone from the system time zone database: the server needs the `tzdata` package.

The `createdAt` field of an ATProto record comes from the client: it can be wrong or backdated. The CLI chooses the time of an event in this order:

1. `createdAt`.
2. The time in the record key, if the key is a TID (Timestamp IDentifier: 13 characters, with the creation time in microseconds).
3. The first time the CLI saw the record.

The CLI refuses a time before 2022-01-01, or more than 1 day in the future, and then uses the next choice. The `daily_counts` days come already grouped by each source: the CLI keeps them as they are.

## Secrets

The CLI reads secrets from environment variables only. The config file holds the variable names, never the values. This keeps the config free of secrets: you can show it, copy it or back it up without risk, and you can renew a secret without a change to the config.

On your machine, put the secrets in a `.env` file:

1. Copy `.env.example` to `.env`.
2. Fill in the values you need. Leave the others empty.
3. Run the CLI. At startup, it reads `.env` in the current directory (or in a parent directory), if the file exists.

A variable that is already set in the environment keeps its value: the shell and systemd (`EnvironmentFile`) win over `.env`. An empty value counts as not set. If `.env` has a bad line, the CLI stops with the line in the error.

On a server, do not use `.env`: give the variables to the service with systemd's `EnvironmentFile`, in a file with mode `0600`.

Do not commit `.env`. Git ignores it. The comments of `config.example.toml` give the rights to give to each token: as few as possible.

### Check the secrets

Run the `check` command after you set or renew a secret, and after each deployment:

```bash
cargo run -- check
```

For each source, the command sends small read-only requests to the real API:

| Source           | Request                                                 | Shows                                       |
| ---------------- | ------------------------------------------------------- | ------------------------------------------- |
| `atproto`        | PDS resolution, then `describeRepo` (no secret)         | DID, PDS URL, number of collections         |
| `github`         | GraphQL `viewer { login }`                              | token owner, requests left, expiration date |
| `lastfm`         | `user.getRecentTracks` with `limit=1`                   | number of scrobbles                         |
| `bsky_bookmarks` | PDS resolution, `createSession`, then `deleteSession`   | handle, DID and PDS URL                     |

The command also warns when a GitHub token has scopes (it needs none), and when the token owner is not the configured user. It exits with code 1 if one check fails. It writes nothing to the database.

### ATProto accounts and their PDS

Each ATProto account lives on one PDS (Personal Data Server), with its own URL: a Bluesky server, Eurosky (`https://eurosky.social`), a self-hosted server, etc. You do not write this URL in the config: the CLI finds it from the handle.

1. Handle to DID: the CLI reads `https://<handle>/.well-known/atproto-did`. If this fails (for example a handle set with a DNS record only), it asks `com.atproto.identity.resolveHandle` on `https://public.api.bsky.app`.
2. DID to DID document: `https://plc.directory/<did>` for a `did:plc`, `https://<domain>/.well-known/did.json` for a `did:web`.
3. The DID document must list the handle back (`alsoKnownAs`). If not, the CLI refuses the handle: anybody can point a domain to any DID.
4. The `#atproto_pds` service of the document gives the PDS URL.

The CLI then sends all ATProto calls to this PDS: the login of the app password, and later the reads of the records.

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

To see the validation, put a bad value in a copy of the config, for example `timezone = "Europe/Atlantis"`. The command must then print the error with its TOML path (`timezone: ... is not a known time zone`) and exit with code 1.

The unit tests of the time helpers (TID decoding, choice of the event time, days in a time zone) use a real TID of a Bluesky post and the ATProto specification example:

```bash
cargo test time::
```

The unit tests of the ATProto adapter use a mock PDS. To fill a database with your real records, set your handle in `[atproto]`, then run:

```bash
cargo run --release -- sync --source atproto
```

The last log line must show `sync done: N inserted`. Then `status` must show the same number of events on the `atproto` line, and a last run. Run `sync` a second time: the log must show `0 inserted, 0 updated, N unchanged`. For a repo of about 50 000 records, a run takes about 2 to 3 minutes.

An unknown source id must stop the command with the list of the configured sources:

```bash
cargo run -- sync --source nope
```

The unit tests of `check` use a mock server (wiremock): they never call the real APIs. To test the real secrets, export them (see "Secrets"), then run:

```bash
cargo run -- check
```

Each source with a valid secret must show `ok`. To see a failure, run the command with a wrong value, for example `GITHUB_TOKEN=wrong cargo run -- check`. The `github` line must then show `FAILED` and the HTTP 401, and the command must exit with code 1.
