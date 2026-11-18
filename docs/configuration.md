# Configuration

## Config file

The CLI reads `config.toml` in the current directory. Use `--config <path>` to read another file.

1. Copy `config.example.toml` to `config.toml`.
2. Edit the sections of the sources that you use. Remove the other sections.

The CLI validates the config at startup. It reports all the errors at the same time, with the TOML path of each error. Git ignores `config.toml`.

## Secrets

The CLI reads secrets from environment variables only. The config file holds the variable names, never the values. Thus you can show, copy or back up the config without risk. You can also change a secret without a change to the config.

On your machine, put the secrets in a `.env` file:

1. Copy `.env.example` to `.env`.
2. Write the values that you need. Leave the other values empty.
3. Run the CLI. At startup, the CLI reads `.env` in the current directory or in a parent directory, if the file exists.

A variable that the environment already sets keeps its value: the shell and systemd (`EnvironmentFile`) have priority over `.env`. An empty value counts as not set. If `.env` has a bad line, the CLI stops and shows the line in the error.

WARNING: Do not commit `.env`. Git ignores it. On a server, do not use `.env`. Give the variables to the service with the systemd `EnvironmentFile`, in a file with mode `0600`.

The comments of `config.example.toml` give the rights for each token. Give each token the fewest rights possible.

### Check the secrets

Run `check` after you set or change a secret, and after each deployment:

```bash
cargo run -- check
```

For each source, `check` sends small read-only requests to the real API:

| Source           | Request                                               | Shows                                       |
| ---------------- | ----------------------------------------------------- | ------------------------------------------- |
| `atproto`        | PDS resolution, then `describeRepo` (no secret)       | DID, PDS URL, number of collections         |
| `github`         | GraphQL `viewer { login }`                            | token owner, requests left, expiration date |
| `lastfm`         | `user.getRecentTracks` with `limit=1`                 | number of scrobbles                         |
| `bsky_bookmarks` | PDS resolution, `createSession`, then `deleteSession` | handle, DID and PDS URL                     |

`check` also gives a warning in 2 cases: the GitHub token has scopes (it needs none), or the token owner is not the configured user. If one check fails, `check` exits with code 1. It writes nothing to the database.

## Database

The CLI keeps all data in one SQLite file: `database` in the config (default: `hairball.db`). If the file does not exist, the CLI creates it. At each start, the CLI applies the migrations of `migrations/`. To back up the database, copy the file. Git ignores `*.db` files.

The database has 3 tables:

- `events`: one row for each record, for the sources that give single events (ATProto, Last.fm, bookmarks).
- `daily_counts`: one count for each day, for the sources that give only totals (GitHub, Gitea).
- `sync_state`: the cursor and the last run of each source.

All times are unix seconds, UTC.

## Time

The database keeps UTC times only. The CLI puts the events into days only when it exports. It uses the time zone of `timezone` in the config (an IANA name, default: `Europe/Paris`). For example, 23:30 UTC on 15 January is 16 January in Paris. The CLI reads the time zone from the time zone database of the system, so the server needs the `tzdata` package.

The client writes the `createdAt` field of an ATProto record, so this field can be wrong or backdated. The CLI chooses the time of an event in this order:

1. `createdAt`.
2. The time in the record key, if the key is a TID. A TID (Timestamp IDentifier) has 13 characters and contains the creation time in microseconds.
3. The first time that the CLI saw the record.

The CLI refuses a time before `oldest_date` of `[atproto]` (default: `2000-01-01`). It also refuses a time more than 1 day in the future. Then it uses the next choice in the list.

ATProto started in 2022, but an app can import older data with the real dates. For example, Rocksky imports a scrobble history with the date of each listen. A limit of 2022 moves all these records to the day of the import.

The sources of `daily_counts` give days that they already calculated. The CLI keeps these days as they are.

## Logs

The CLI writes its logs to stderr. Set the level with `RUST_LOG` (default: `info`):

```bash
RUST_LOG=hairball=debug cargo run -- sync
```

With `debug`, the ATProto adapter logs each collection that it reads or skips. A refused event time (for example a `createdAt` in the future) gives a warning with the URI of the record.
