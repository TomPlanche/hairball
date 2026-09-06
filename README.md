# hairball

hairball is a Rust CLI that collects my public activity from many platforms into one SQLite file. It then exports a JSON file for a GitHub-style calendar heatmap.

The planned sources are ATProto (Bluesky, Tangled, standard.site), Gitea/Forgejo/Codeberg, GitHub, Last.fm and Bluesky bookmarks.

## Status

The project is at an early stage.

- `sync` works for all the sources (see "Sync"):
  - ATProto: it reads the records of the repo, stores one event per record, marks the records deleted upstream and purges the disabled kinds.
  - Gitea, Forgejo and Codeberg: it reads the contribution heatmap of each instance and stores one count per day.
  - GitHub: it reads the contribution calendar with the GraphQL API and stores one count per day.
  - Last.fm: it reads the scrobbles and stores one event per scrobble.
  - Bluesky bookmarks: it reads the saved posts with an app password and stores one event per bookmark.

  A Gitea instance with `mirror = true` is skipped with a log line.
- `status` lists the configured sources with their last run, row counts and cursor.
- `check` tests the secrets and the ATProto identity against the real APIs.
- `purge` deletes all the events of one kind.
- `export` writes the calendar JSON of one year (see "Export").

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

## Sync

`sync` has 2 modes:

- **Incremental** (default, for the hourly run): `listRecords` returns the records of a collection by record key, newest first. Most record keys are TIDs, which sort by time. The CLI stops a collection after a page where all the records are already in the database: the next pages are older.
- **Full** (`--full`, for the daily run): the CLI reads all the pages of all the collections.

A record with a key that is not a TID (a custom key) can appear on any page. The incremental mode can miss it, and the next full run finds it.

### Deletions

Only a full run sees all the records. After a full run that read every collection without an error, the CLI sets `deleted_at` on each row that the run did not see: the record was deleted upstream. The row stays in the database. If the record comes back, the next sync clears `deleted_at`. An incremental run, or a full run that failed, marks nothing.

### Disabled kinds

A kind that the config no longer wants (in `exclude`, or a `[atproto.bluesky]` toggle set to `false`) is not only skipped: at the start of the next sync, the CLI deletes all its rows. The database then contains only what the config enables.

To delete the rows of one kind by hand, use `purge`:

```bash
cargo run -- purge --kind app.rocksky.album
```

If the config still wants this kind, the command warns you: the next sync adds the rows again. Add the kind to `exclude` to keep it out.

### Gitea, Forgejo and Codeberg

Each `[[gitea]]` entry is one source, with the id `gitea:<name>`. Codeberg runs Forgejo, and Forgejo is a fork of Gitea: the 3 have the same `/api/v1` endpoints.

1. The CLI reads `GET <url>/api/v1/users/<user>/heatmap`. The answer is a list of `{ "timestamp": <unix>, "contributions": <n> }`, one per 15-minute slot, for about the last year.
2. It sums the slots per day, in the time zone of the config.
3. It replaces the days of the last 360 days in `daily_counts`: a day that the heatmap no longer has (for example after the deletion of a repository) is removed. The older days stay: they are history that the heatmap no longer returns.

The request never sends a token. With a token, older Gitea versions computed the heatmap with all the repositories that the token can read, so private activity leaked into the result. An anonymous request returns public activity only.

An instance with `mirror = true` is skipped, with one log line: it mirrors repositories of another source, and an aggregate cannot be deduplicated after the fact.

### GitHub

The GitHub source uses the GraphQL API (`POST https://api.github.com/graphql`) with the token of the environment variable `token_env` (see "Secrets" for its rights).

1. The CLI reads the creation date of the account (`user { createdAt }`).
2. It reads the contribution calendar year by year: one query covers 1 year at most. A full run reads every year since the creation of the account. An incremental run reads the current and the previous year.
3. It stores each day of the calendar in `daily_counts` with the source `github`, as GitHub gives it, days with 0 contributions included.

The calendar can include private contributions, if the profile setting "Include private contributions on my profile" is on. The CLI stores only the total per day, never a repository name.

GraphQL can report an error with HTTP 200 and an `errors` array, for example an unknown user: the sync of the source then fails with the message of GitHub.

Open question: GitHub groups the calendar days in UTC, or in the time zone of the profile? The GraphQL documentation does not say. To check it, make a commit between 00:00 and 01:00 in Paris (between 22:00 and 23:00 UTC the day before), run `sync --source github`, then look at which day gets the contribution in `daily_counts`.

### Last.fm

The Last.fm source uses the crate `lastfm-client` (method `user.getRecentTracks`), with the API key of the environment variable `api_key_env`.

1. An incremental run reads the scrobbles from the cursor: the highest `uts` (unix time of the scrobble) already stored. A full run reads the whole history.
2. The CLI skips the track that plays now: it has no date, so it is not a scrobble yet.
3. It stores each scrobble in `events`, with the kind `scrobble`, the time `uts`, and the id `<uts>:<artist>:<track>`: Last.fm scrobbles have no stable id.

The cursor scrobble comes back at each incremental run, unchanged: a repeated scrobble is safer than a gap.

Last.fm puts the track that plays now on top of the requested number of scrobbles. `lastfm-client` 5.0.0 counted it in this number, so the oldest scrobble of the page was cut while a track played. The fix is in `lastfm-client` 5.1.0: hairball needs this version or a later one.

If you also scrobble to an ATProto app (Rocksky, teal.fm), each listen exists 2 times: in Last.fm and in the ATProto repo. Exclude one of the 2 to count each listen once.

### Bluesky bookmarks

Bookmarks ("Saved posts") are private, and they are not records of the ATProto repo: only `app.bsky.bookmark.getBookmarks`, with a session, returns them. The source is disabled by default (`enabled = false` in `[bsky_bookmarks]`).

1. The CLI finds the PDS of `identifier` (see "ATProto accounts and their PDS"), then logs in there with the app password of `app_password_env` (`com.atproto.server.createSession`).
2. It reads all the pages of `getBookmarks` (100 per page). The PDS forwards the call to the AppView.
3. It stores each bookmark in `events`, with the kind `bookmark` and the URI of the saved post as id. The time is the `createdAt` of the bookmark. This field is optional in the lexicon (`app.bsky.bookmark.defs#bookmarkView`): without it, the time is the first time the CLI saw the bookmark.
4. A stored bookmark that the run did not see was removed: the CLI sets its `deleted_at`.
5. It closes the session (`deleteSession`).

If the access token expires during the run, the CLI logs in again once, then reads the page again. With `enabled = false`, a sync sends no request: it deletes all the stored bookmarks.

### Rate limits

A PDS limits the number of requests. When it answers HTTP 429, the CLI waits, then sends the request again, at most 3 times. The wait comes from the response:

1. `ratelimit-reset` (ATProto PDS): the unix time when the limit resets, plus 1 second.
2. `retry-after` (standard HTTP): a number of seconds.
3. Without these headers: 1, 2, then 4 seconds.

If the wait is longer than 10 minutes, the CLI does not wait: the sync of the source fails, and the next run continues. Each wait is a warning in the log, with the URL and the number of seconds.

## Export

`export` writes the JSON of a GitHub-style heatmap, for one calendar year (default: the current year):

```bash
cargo run -- export --year 2026 --out calendar.json
```

Without `--out`, the JSON goes to stdout. With `--out`, the CLI writes a temporary file, then renames it: a web server never reads a half-written file.

```json
{
  "timezone": "Europe/Paris",
  "from": "2026-01-01",
  "to": "2026-12-31",
  "sources": {
    "bluesky": { "label": "Bluesky", "max": 32 },
    "codeberg": { "label": "Codeberg", "max": 193 }
  },
  "days": [
    { "date": "2026-11-12", "total": 23, "by_source": { "bluesky": 5, "codeberg": 18 } }
  ]
}
```

- `days` contains only the days with activity, in order. The frontend fills the other days with 0.
- `max` is the highest count of one day for the source. Use it to normalize the intensity of each source: without it, a busy source (Last.fm, many scrobbles per day) hides all the others.
- The events marked as deleted upstream, and the days with a count of 0, are left out.

The keys of `sources`:

| Data | Key | Label |
| ---- | --- | ----- |
| ATProto records | the app of the NSID (see below) | the app name |
| Last.fm scrobbles | `lastfm` | Last.fm |
| Bluesky bookmarks | `bookmarks` | Bluesky bookmarks |
| GitHub | `github` | GitHub |
| `[[gitea]]` entry | its `name` (for example `codeberg`) | the name, with a capital letter |

`src/registry.rs` groups the ATProto collections by app, with a static table of NSID prefixes: `app.bsky` is Bluesky, `sh.tangled` and `org.tangled` are Tangled, `fm.teal` is teal.fm, etc. An unknown NSID uses its authority (the NSID without its last segment, for example `com.tomplanche.atkuzu`) as key and label. When an app spreads its collections over several sub-namespaces, add its prefix to the table: if not, each sub-namespace is a separate source.

Limit: the events go into the days of `timezone`, but the `daily_counts` days (GitHub, Gitea) come already grouped by their source. GitHub can group them in UTC (open question, see "GitHub"), so a GitHub contribution near midnight can be on the day before or after its day in Paris.

## Time

The database stores UTC times only. The CLI groups the events into days only at export time, in the time zone of `timezone` in the config (an IANA name, default: `Europe/Paris`). For example, 23:30 UTC on 15 January is 16 January in Paris. The CLI reads the time zone from the system time zone database: the server needs the `tzdata` package.

The `createdAt` field of an ATProto record comes from the client: it can be wrong or backdated. The CLI chooses the time of an event in this order:

1. `createdAt`.
2. The time in the record key, if the key is a TID (Timestamp IDentifier: 13 characters, with the creation time in microseconds).
3. The first time the CLI saw the record.

The CLI refuses a time before `oldest_date` of `[atproto]` (default: `2000-01-01`), or more than 1 day in the future, and then uses the next choice. ATProto started in 2022, but an app can import older data with its real date: for example Rocksky imports a scrobble history with the date of each listen. A limit of 2022 would move all these records to the day of the import. The `daily_counts` days come already grouped by each source: the CLI keeps them as they are.

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

The last log line must show `sync done: N inserted`. Then `status` must show the same number of events on the `atproto` line, and a last run. For a repo of about 50 000 records, this first run takes about 2 minutes.

Run `sync` a second time: it is incremental, so it must take a few seconds (about 10 s for 50 000 records), with `0 inserted` and only a small number of `unchanged` records. Then run `sync --full`: it must read everything again, with `0 inserted, 0 updated, N unchanged`, and `D deleted` if records were deleted upstream since the first run. `status` then shows these `D` rows in the `DELETED` column. With `RUST_LOG=hairball=debug`, each collection logs `only known records: stop` or `last page`.

To see the purge, add a kind that has rows to `exclude` (for example `app.bsky.feed.like`), then run `sync`: the log must show `purged: disabled in the config` with the number of rows, and `status` must show fewer events.

A real PDS rarely answers HTTP 429 for one repo. The retry has unit tests with a mock server: a 429 then a success, 4 answers 429 (1 try and 3 retries), and a reset time too far away:

```bash
cargo test http::
```

The unit tests of the Gitea adapter use a real heatmap of Codeberg (`tests/fixtures/codeberg_heatmap.json`). They check the sums per day in `Europe/Paris` and in `UTC`, and that the request has no `Authorization` header. To test a real instance, set your login in a `[[gitea]]` entry, then run the sync 2 times:

```bash
cargo run -- sync --source gitea:codeberg
```

The first run must show `N inserted`, the second `0 inserted, 0 updated, N unchanged`. `status` must show `N` in the `DAYS` column of the source. A `[[gitea]]` entry with `mirror = true` must show `skipped: mirror of another source`.

The unit tests of the GitHub adapter use a mock GraphQL server: the loop over the years, the incremental range, a GraphQL error with HTTP 200, a bad token and a missing token. To test the real API, set `GITHUB_TOKEN` (see "Secrets"), then run:

```bash
cargo run -- sync --source github --full
```

The log must show `sync done: N inserted`, with about 365 days per year since the creation of the account. Without the variable, the sync of the source must fail with `the environment variable GITHUB_TOKEN is not set or is empty`, before any request.

The unit tests of the Last.fm adapter send the requests of `lastfm-client` to a mock server: the scrobbles and their time, the cursor of the next run, the skip of the playing track, and a missing API key. To test the real API, set your user in `[lastfm]` and `LASTFM_API_KEY` (see "Secrets"), then run 2 times:

```bash
cargo run -- sync --source lastfm
```

The first run reads the whole history and must show `N inserted`. The second run must show `0 inserted` and `1 unchanged` (the cursor scrobble), and `status` must show the cursor (a unix time) on the `lastfm` line.

The unit tests of the bookmarks adapter use a mock PDS: the pages, a bookmark without `createdAt`, a new login after an expired token, a token that expires again, a removed bookmark and the purge of a disabled source. To test the real API, set `enabled = true` and your handle in `[bsky_bookmarks]`, and `BSKY_APP_PASSWORD` (see "Secrets"). Check the login first, then sync:

```bash
cargo run -- check
```

```bash
cargo run -- sync --source bsky_bookmarks
```

`check` must show `ok` on the `bsky_bookmarks` line. The sync must show `N inserted`, with N the number of your saved posts. Then set `enabled = false` and sync again: the log must show `bookmarks disabled: purged`, and `status` must show 0 events.
An unknown source id must stop the command with the list of the configured sources:

```bash
cargo run -- sync --source nope
```

The unit tests of `check` use a mock server (wiremock): they never call the real APIs. To test the real secrets, export them (see "Secrets"), then run:

```bash
cargo run -- check
```

Each source with a valid secret must show `ok`. To see a failure, run the command with a wrong value, for example `GITHUB_TOKEN=wrong cargo run -- check`. The `github` line must then show `FAILED` and the HTTP 401, and the command must exit with code 1.

The unit tests of the export use an in-memory database: the grouping per day and per source, the `max` of each source, the day boundary in Paris (also at the end of the year), and the deleted events and days with 0 that are left out. To test it on your database, after a sync:

```bash
cargo run -- export --year 2026 --out calendar.json
```

The log must show `calendar written` with the number of days and sources, and `calendar.json` must have one key in `sources` for each app with activity in 2026. No `calendar.json.tmp` file must stay.
