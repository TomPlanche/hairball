# Export

`export` writes the JSON of a GitHub-style heatmap for one calendar year (default: the current year):

```bash
cargo run -- export --year 2026 --out calendar.json
```

## Destination

The CLI chooses the destination in this order:

1. `--out <path>` on the command line.
2. `out` in the `[export]` section of the config:

   ```toml
   [export]
   out = "/srv/hairball/calendar.json"
   ```

3. Stdout.

The destination is not a secret, so it goes in the config, not in `.env`. For a file, the CLI writes a temporary file, then renames it. Thus a web server never reads a half-written file.

With `--dry-run`, the CLI makes the calendar, then logs the destination, the number of days, the number of sources and the size. It writes no file and shows no JSON.

## Format

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

- `days` contains only the days with activity, in order. The frontend gives the value 0 to the other days.
- `max` is the highest count of one day for the source. Use `max` to normalize the intensity of each source. Without it, a busy source (Last.fm, many scrobbles each day) hides all the other sources.
- The export does not include the events deleted upstream, or the days with a count of 0.

## Source keys

| Data | Key | Label |
| ---- | --- | ----- |
| ATProto records | the app of the NSID (see below) | the app name |
| Last.fm scrobbles | `lastfm` | Last.fm |
| Bluesky bookmarks | `bookmarks` | Bluesky bookmarks |
| GitHub | `github` | GitHub |
| `[[gitea]]` entry | its `name` (for example `codeberg`) | the name, with a capital letter |

`src/registry.rs` puts the ATProto collections into apps, with a static table of NSID prefixes. For example, `app.bsky` is Bluesky, `sh.tangled` and `org.tangled` are Tangled, and `fm.teal` is teal.fm.

An unknown NSID uses its authority as key and label. The authority is the NSID without its last segment, for example `com.tomplanche.atkuzu`. If an app has collections in many sub-namespaces, add its prefix to the table. If you do not, each sub-namespace is a different source.

## Limit

The CLI puts the events into the days of `timezone`. But the sources of `daily_counts` (GitHub, Gitea) give days that they already calculated. GitHub can use UTC days (see the open question in [sync.md](sync.md#github)). Thus a GitHub contribution near midnight can be on the day before or after its day in Paris.
