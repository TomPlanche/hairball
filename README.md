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

To see the validation, put a bad value in a copy of the config, for example `timezone = ""`. The command must then print the error with its TOML path and exit with code 1.

The unit tests of `check` use a mock server (wiremock): they never call the real APIs. To test the real secrets, export them (see "Secrets"), then run:

```bash
cargo run -- check
```

Each source with a valid secret must show `ok`. To see a failure, run the command with a wrong value, for example `GITHUB_TOKEN=wrong cargo run -- check`. The `github` line must then show `FAILED` and the HTTP 401, and the command must exit with code 1.
