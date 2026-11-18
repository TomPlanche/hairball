# Testing

## Automatic tests

```bash
cargo test
```

```bash
cargo clippy --all-targets -- -D warnings
```

The unit tests never call the real APIs. They use mock servers (wiremock) or fixtures:

| Module | Test data | Covers |
| ------ | --------- | ------ |
| `time::` | a real TID of a Bluesky post, the ATProto specification example | TID decoding, choice of the event time, days in a time zone |
| `http::` | a mock server | a 429 then a success, 4 answers 429 (1 try and 3 retries), a reset time too far away |
| ATProto adapter | a mock PDS | the pages, the progress steps and messages |
| Gitea adapter | `tests/fixtures/codeberg_heatmap.json` | the sums for each day in `Europe/Paris` and `UTC`, no `Authorization` header |
| GitHub adapter | a mock GraphQL server | the loop over the years, the incremental range, a GraphQL error with HTTP 200, a bad token, a missing token |
| Last.fm adapter | a mock server | the scrobbles and their time, the cursor, the skip of the playing track, a missing API key |
| Bookmarks adapter | a mock PDS | the pages, a bookmark without `createdAt`, a new login after an expired token, a token that expires again, a removed bookmark, the purge of a disabled source |
| `check` | a mock server | the requests and the results of each source |
| Export | an in-memory database | the days and sources, `max`, the day limit in Paris (also at the end of the year), the deleted events and the days with 0 |
| Dry run | a temporary database | the copy does not change the source, a missing database gives an empty copy, the count of `purge --dry-run` |
| `src/pattern.rs` | an in-memory database | the patterns, and the kinds that a pattern selects |

To run only one module, give its name, for example `cargo test time::`.

## Manual checks

Each check below uses the real APIs. Set the secrets first (see [configuration.md](configuration.md#secrets)).

### Status and validation

1. Run `status` with the example config:

   ```bash
   cargo run -- --config config.example.toml status
   ```

   The command creates `hairball.db` in the current directory. It must show 6 sources: `atproto`, `bsky_bookmarks` (disabled), `gitea:codeberg`, `gitea:forge-perso` (skipped, mirror), `github` and `lastfm`. Each source must show `never` as last run and `0` rows.

2. Add rows manually, then run `status` again:

   ```bash
   sqlite3 hairball.db "INSERT INTO daily_counts VALUES ('github', '2026-07-20', 3); INSERT INTO sync_state VALUES ('github', '2026', 1753450000);"
   ```

   The `github` line must show `2025-07-25T13:26:40Z`, `1` day and the cursor `2026`.

3. Delete `hairball.db`.
4. Put a bad value in a copy of the config, for example `timezone = "Europe/Atlantis"`, then run `status` with this copy. The command must show the error with its TOML path (`timezone: ... is not a known time zone`) and exit with code 1.
5. Run `cargo run -- sync --source nope`. The command must stop and show the list of the configured sources.

### ATProto

1. Set your handle in `[atproto]`, then run:

   ```bash
   cargo run --release -- sync --source atproto
   ```

   The last log line must show `sync done: N inserted`. `status` must show the same number of events on the `atproto` line, and a last run. For a repo of approximately 50 000 records, this first run takes approximately 2 minutes.

2. Run `sync` again. This run is incremental, so it must take a few seconds (approximately 10 s for 50 000 records). It must show `0 inserted` and a small number of `unchanged` records.
3. Run `sync --full`. It must read all the records again and show `0 inserted, 0 updated, N unchanged`. If records were deleted upstream after the first run, it also shows `D deleted`, and `status` shows these `D` rows in the `DELETED` column.

With `RUST_LOG=hairball=debug`, each collection logs `only known records: stop` or `last page`.

To check the purge of a disabled kind, add a kind that has rows to `exclude` (for example `app.bsky.feed.like`). Then run `sync`. The log must show `purged: disabled in the config` with the number of rows, and `status` must show fewer events.

### Gitea

Set your login in a `[[gitea]]` entry, then run this command 2 times:

```bash
cargo run -- sync --source gitea:codeberg
```

The first run must show `N inserted`. The second run must show `0 inserted, 0 updated, N unchanged`. `status` must show `N` in the `DAYS` column of the source. A `[[gitea]]` entry with `mirror = true` must show `skipped: mirror of another source`.

### GitHub

Set `GITHUB_TOKEN`, then run:

```bash
cargo run -- sync --source github --full
```

The log must show `sync done: N inserted`, with approximately 365 days for each year since the creation of the account. Without the variable, the sync of the source must fail before it sends a request, with `the environment variable GITHUB_TOKEN is not set or is empty`.

### Last.fm

Set your user in `[lastfm]` and `LASTFM_API_KEY`, then run this command 2 times:

```bash
cargo run -- sync --source lastfm
```

The first run reads the full history and must show `N inserted`. The second run must show `0 inserted` and `1 unchanged` (the cursor scrobble). `status` must show the cursor (a unix time) on the `lastfm` line.

### Bluesky bookmarks

1. Set `enabled = true` and your handle in `[bsky_bookmarks]`, and set `BSKY_APP_PASSWORD`.
2. Run `cargo run -- check`. The `bsky_bookmarks` line must show `ok`.
3. Run `cargo run -- sync --source bsky_bookmarks`. The log must show `N inserted`, where N is the number of your saved posts.
4. Set `enabled = false`, then run the sync again. The log must show `bookmarks disabled: purged`, and `status` must show 0 events.

### Secrets

Run `cargo run -- check`. Each source with a correct secret must show `ok`.

To see a failure, give a wrong value: `GITHUB_TOKEN=wrong cargo run -- check`. The `github` line must show `FAILED` and the HTTP 401, and the command must exit with code 1.

### Export

After a sync, run:

```bash
cargo run -- export --year 2026 --out calendar.json
```

The log must show `calendar written` with the number of days and sources. `calendar.json` must have one key in `sources` for each app with activity in 2026. No `calendar.json.tmp` file must stay.

To check the order of the destinations:

1. Set `out = "from-config.json"` in `[export]`, then run `export` without `--out`. The log must show `path=from-config.json`.
2. Run `export --out other.json`. The log must show `path=other.json`.
3. Remove `[export]`, then run `export` without `--out`. The JSON must go to stdout.

### Progress bars

Run a full sync in a terminal:

```bash
cargo run --release -- sync --full
```

Each source must show a bar or a spinner while it runs, then its `sync done` line. With `cargo run -- sync --full 2> sync.log`, `sync.log` must contain only log lines, without a bar.

### Dry run

Calculate the checksum of the database, run the 3 dry runs, then calculate the checksum again:

```bash
shasum hairball.db && cargo run -- sync --dry-run && cargo run -- purge --kind app.bsky.feed.like --dry-run && cargo run -- export --dry-run && shasum hairball.db
```

The 2 checksums must be equal. No `hairball-dry-run-*.db` file must stay in the temporary directory, and the command must write no export file.

### Purge with a pattern

Run the purge as a dry run first, with the pattern in quotes:

```bash
cargo run -- purge --kind 'app.bsky.feed.*' --dry-run
```

The command must show one `would delete` line for each matching kind (for example `app.bsky.feed.like` and `app.bsky.feed.post`), then `would delete N events in K kinds`. A pattern without a match must show `no event of a kind that matches`.

### systemd units

The systemd units have no automatic test. On the server, check them with `systemd-analyze verify` (step 7 of [Install](deployment.md#install)). After the first full sync (step 8), `journalctl` must show one `sync done` or `sync failed` line for each source, then `calendar written`. `/srv/hairball/calendar.json` must exist. `systemctl list-timers 'hairball*'` must show the 2 timers.
