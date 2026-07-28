CREATE TABLE events (
  source      TEXT    NOT NULL,  -- 'atproto', 'lastfm', 'bsky_bookmarks'
  kind        TEXT    NOT NULL,  -- NSID for atproto, 'scrobble', 'bookmark'
  external_id TEXT    NOT NULL,  -- at-uri, scrobble id, bookmarked post uri
  occurred_at INTEGER NOT NULL,  -- unix seconds, UTC
  payload     TEXT    NOT NULL,  -- raw JSON from the source
  first_seen  INTEGER NOT NULL,  -- unix seconds, UTC
  deleted_at  INTEGER,           -- set when the record disappears upstream
  PRIMARY KEY (source, external_id)
);
CREATE INDEX events_occurred_at ON events (occurred_at);
CREATE INDEX events_kind ON events (kind);

-- Sources that only expose aggregates (GitHub calendar, Gitea heatmap).
CREATE TABLE daily_counts (
  source TEXT    NOT NULL,  -- 'github', 'gitea:codeberg', ...
  day    TEXT    NOT NULL,  -- 'YYYY-MM-DD', as given by the source
  count  INTEGER NOT NULL,
  PRIMARY KEY (source, day)
);

CREATE TABLE sync_state (
  source   TEXT PRIMARY KEY, -- adapter instance id
  cursor   TEXT,             -- adapter-specific (JSON if needed)
  last_run INTEGER           -- unix seconds, UTC
);
