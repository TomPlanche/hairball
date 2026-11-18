# Sync

`sync` reads all the enabled sources. Use `--source <id>` to read only one source.

## Modes

`sync` has 2 modes:

- **Incremental** (default, for the hourly run): `listRecords` returns the records of a collection by record key, newest first. Most record keys are TIDs, which sort by time. When all the records of a page are already in the database, the CLI stops this collection. The next pages contain only older records.
- **Full** (`--full`, for the daily run): the CLI reads all the pages of all the collections.

A record with a key that is not a TID (a custom key) can be on any page. The incremental mode can miss this record. The next full run finds it.

## Progress

In a terminal, `sync` shows one progress bar for each source, under the logs:

```
⠄ atproto [5s] [========>                     ] 18/62 dev.npmx.feed.like: 2 records
⠒ github [0s] [                              ] 0/8 2019
⠁ lastfm [0s] scrobbles
```

| Source | Steps of the bar | Message |
| ------ | ---------------- | ------- |
| `atproto` | the collections to read | the collection, and the records read in it |
| `github` | the years to read | the year |
| `lastfm` | the scrobbles (total from Last.fm) | `scrobbles` |
| `gitea:<name>`, `bsky_bookmarks` | unknown: a spinner only | `heatmap`, or the bookmarks read |

When a source is complete, its bar disappears, and the `sync done` log line shows the result. Without a terminal (systemd, a pipe, a file), the CLI shows no bar, only log lines. The bars also need the `TERM` environment variable. All terminals set it.

## Dry run

With `--dry-run`, the CLI copies the database to a temporary file. It runs the sync on the copy, then deletes the copy. The logs show the real counts (`dry run, would change: N inserted, ... P purged`). The database does not change.

Use a dry run before a change to the config, for example a new `exclude` entry. `sync --dry-run` shows how many rows the purge will delete.

A dry run still sends its requests to the sources. These requests are read-only, but the login of the bookmarks source opens and closes a session. The logs of each source (`purged: disabled in the config`) do not change. Only the last line of each source (`dry run, would change`) tells that nothing changed.

## Deletions

Only a full run sees all the records. A full run can read all collections without an error. Then the CLI sets `deleted_at` on each row that the run did not see, because the record was deleted upstream. The row stays in the database. If the record comes back, the next sync clears `deleted_at`. An incremental run, or a full run that failed, marks no row.

## Disabled kinds

The config can disable a kind with `exclude`, or with a `[atproto.bluesky]` toggle set to `false`. At the start of the next sync, the CLI deletes all the rows of this kind. Thus the database contains only the kinds that the config enables.

An `exclude` entry has 2 forms:

| Entry | Matches | Does not match |
| ----- | ------- | -------------- |
| `app.bsky.graph` (no `*`): an NSID or an NSID prefix | `app.bsky.graph`, `app.bsky.graph.follow` | `app.bsky.graphx.follow` |
| `app.rocksky.*`: a pattern, `*` matches any characters | `app.rocksky.scrobble`, `app.rocksky.actor.profile` | `app.rockskyx.scrobble` |
| `*.feed.like` | `app.bsky.feed.like`, `dev.npmx.feed.like` | `app.bsky.feed.likes` |

A pattern must match the full NSID: `app.*.feed` does not match `app.bsky.feed.post`. Use `app.*.feed.*` for this NSID.

## Purge

To delete the rows of one kind manually, use `purge`:

```bash
cargo run -- purge --kind app.rocksky.album
```

`--kind` also accepts a pattern, where `*` matches any characters. For example, `'app.rocksky.*'` deletes all Rocksky kinds, and `'*.feed.like'` deletes the likes of all apps. The command shows one line for each kind, then the total.

Without `*`, the kind must be equal. This is different from `exclude`: `purge --kind app.rocksky` deletes nothing. Thus a purge never deletes more than you ask.

Put the pattern in single quotes. If you do not, the shell tries to change `*` into file names before the CLI gets it, and fish stops with `No matches for wildcard`:

```bash
cargo run -- purge --kind 'app.rocksky.*' --dry-run
```

If the config still enables a kind, `purge` gives a warning, because the next sync adds the rows again. To keep the kind out, add it to `exclude`.

## Sources

### ATProto accounts and their PDS

Each ATProto account is on one PDS (Personal Data Server), with its own URL. This can be a Bluesky server, Eurosky (`https://eurosky.social`) or a self-hosted server. You do not write this URL in the config. The CLI finds it from the handle:

1. Handle to DID: the CLI reads `https://<handle>/.well-known/atproto-did`. If this fails (for example, a handle set with a DNS record only), the CLI sends `com.atproto.identity.resolveHandle` to `https://public.api.bsky.app`.
2. DID to DID document: `https://plc.directory/<did>` for a `did:plc`, `https://<domain>/.well-known/did.json` for a `did:web`.
3. The DID document must contain the handle in `alsoKnownAs`. If it does not, the CLI refuses the handle, because anybody can point a domain to any DID.
4. The `#atproto_pds` service of the document gives the PDS URL.

Then the CLI sends all ATProto calls to this PDS.

### Gitea, Forgejo and Codeberg

Each `[[gitea]]` entry is one source, with the id `gitea:<name>`. Codeberg uses Forgejo, and Forgejo is a fork of Gitea. Thus the 3 have the same `/api/v1` endpoints.

1. The CLI reads `GET <url>/api/v1/users/<user>/heatmap`. The response is a list of `{ "timestamp": <unix>, "contributions": <n> }`, one for each 15-minute slot, for approximately the last year.
2. The CLI adds the slots of each day, in the time zone of the config.
3. The CLI replaces the last 360 days in `daily_counts`. It removes a day that the heatmap no longer contains (for example, after you delete a repository). The older days stay, because the heatmap no longer returns them.

The request never sends a token. With a token, older Gitea versions calculated the heatmap with all the repositories that the token can read. Thus private activity was in the result. An anonymous request returns only public activity.

The CLI skips an instance with `mirror = true` and writes one log line. This instance copies the repositories of another source, and the CLI cannot remove duplicates from a total.

### GitHub

The GitHub source uses the GraphQL API (`POST https://api.github.com/graphql`). It uses the token of the environment variable `token_env`.

1. The CLI reads the creation date of the account (`user { createdAt }`).
2. The CLI reads the contribution calendar for each year, because one query covers 1 year at most. A full run reads all the years since the creation of the account. An incremental run reads the current year and the previous year.
3. The CLI writes each day of the calendar in `daily_counts` with the source `github`. It also writes the days with 0 contributions.

The calendar can include private contributions, if the profile setting "Include private contributions on my profile" is on. The CLI keeps only the total of each day, never a repository name.

GraphQL can send an error with HTTP 200 and an `errors` array, for example for an unknown user. Then the sync of the source fails with the message from GitHub.

Open question: does GitHub put the calendar days in UTC, or in the time zone of the profile? The GraphQL documentation does not say. To find the answer:

1. Make a commit between 00:00 and 01:00 in Paris (between 22:00 and 23:00 UTC on the day before).
2. Run `sync --source github`.
3. Look at the day of the contribution in `daily_counts`.

### Last.fm

The Last.fm source uses the crate `lastfm-client` (method `user.getRecentTracks`). It uses the API key of the environment variable `api_key_env`. The source needs `lastfm-client` 5.1.0 or later. Version 5.0.0 lost the oldest scrobble of a page while a track played.

1. An incremental run reads the scrobbles from the cursor. The cursor is the highest `uts` (unix time of the scrobble) in the database. A full run reads the full history.
2. The CLI skips the track that plays now. This track has no date, so it is not a scrobble yet.
3. The CLI writes each scrobble in `events`, with the kind `scrobble` and the time `uts`. The id is `<uts>:<artist>:<track>`, because Last.fm scrobbles have no stable id.

Each incremental run gets the cursor scrobble again, with no change. A repeated scrobble is safer than a gap.

If you also send your scrobbles to an ATProto app (Rocksky, teal.fm), each listen is in the database 2 times. Exclude one of the 2 sources to count each listen one time.

### Bluesky bookmarks

Bookmarks ("Saved posts") are private, and they are not records of the ATProto repo. Only `app.bsky.bookmark.getBookmarks` returns them, and it needs a session. The source is disabled by default (`enabled = false` in `[bsky_bookmarks]`).

1. The CLI finds the PDS of `identifier` (see "ATProto accounts and their PDS"). Then it logs in to this PDS with the app password of `app_password_env` (`com.atproto.server.createSession`).
2. The CLI reads all the pages of `getBookmarks` (100 for each page). The PDS sends the call to the AppView.
3. The CLI writes each bookmark in `events`, with the kind `bookmark`. The id is the URI of the saved post. The time is the `createdAt` of the bookmark. This field is optional in the lexicon (`app.bsky.bookmark.defs#bookmarkView`). Without it, the time is the first time that the CLI saw the bookmark.
4. If the run did not see a bookmark of the database, the user removed it. The CLI sets its `deleted_at`.
5. The CLI closes the session (`deleteSession`).

If the access token expires during the run, the CLI logs in again one time. Then it reads the page again. With `enabled = false`, a sync sends no request and deletes all the bookmarks in the database.

## Rate limits

A PDS limits the number of requests. When the PDS sends HTTP 429, the CLI waits, then sends the request again, at most 3 times. The CLI gets the wait time from the response:

1. `ratelimit-reset` (ATProto PDS): the unix time when the limit resets, plus 1 second.
2. `retry-after` (standard HTTP): a number of seconds.
3. Without these headers: 1, 2, then 4 seconds.

If the wait time is more than 10 minutes, the CLI does not wait. The sync of the source fails, and the next run continues. Each wait gives a warning in the log, with the URL and the number of seconds.
