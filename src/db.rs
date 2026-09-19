//! SQLite storage: pool, migrations, upserts and queries.
//!
//! All times are unix seconds, UTC. Every write is idempotent: the same input
//! two times gives [`Upsert::Unchanged`] the second time.

use std::{collections::BTreeMap, path::Path, str::FromStr};

use sqlx::{
    SqliteConnection, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("cannot connect to SQLite")]
    Open(#[source] sqlx::Error),

    #[error("cannot run the migrations")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error(transparent)]
    Query(#[from] sqlx::Error),
}

/// What an upsert did to the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upsert {
    Inserted,
    Updated,
    Unchanged,
}

/// One event, as given by a source.
#[derive(Debug, Clone, Copy)]
pub struct NewEvent<'a> {
    pub source: &'a str,
    pub kind: &'a str,
    pub external_id: &'a str,
    pub occurred_at: i64,
    /// Raw JSON from the source.
    pub payload: &'a str,
}

/// The stored state of one source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncState {
    pub cursor: Option<String>,
    pub last_run: Option<i64>,
}

/// Row counts and sync state of one source, for the `status` command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceStats {
    pub state: SyncState,
    /// Events that still exist upstream.
    pub events: i64,
    /// Events marked as deleted upstream.
    pub deleted: i64,
    /// Rows in `daily_counts`.
    pub days: i64,
}

/// Opens (and creates if needed) the database file, then runs the migrations.
pub async fn open(path: &Path) -> Result<SqlitePool, DbError> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .map_err(DbError::Open)?;
    migrate(&pool).await?;
    Ok(pool)
}

/// Copies the database `source` to `copy` (a new file), then opens the copy
/// and runs the migrations on it. The source is opened read-only and is never
/// changed, even by a pending migration. If `source` does not exist, the copy
/// starts empty. For a dry run: the run writes to the copy only.
pub async fn open_copy(source: &Path, copy: &Path) -> Result<SqlitePool, DbError> {
    if source.exists() {
        let options = SqliteConnectOptions::new().filename(source).read_only(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(DbError::Open)?;
        // `VACUUM INTO` writes a consistent copy, even while another process
        // writes to the source.
        sqlx::query("VACUUM INTO ?1")
            .bind(copy.to_string_lossy().into_owned())
            .execute(&pool)
            .await?;
        pool.close().await;
    }
    open(copy).await
}

/// Opens a new empty in-memory database, for tests.
pub async fn open_in_memory() -> Result<SqlitePool, DbError> {
    let options = SqliteConnectOptions::from_str("sqlite::memory:").map_err(DbError::Open)?;
    // Each connection to `:memory:` is a separate database: keep exactly one, forever.
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(options)
        .await
        .map_err(DbError::Open)?;
    migrate(&pool).await?;
    Ok(pool)
}

async fn migrate(pool: &SqlitePool) -> Result<(), DbError> {
    sqlx::migrate!().run(pool).await?;
    Ok(())
}

/// Inserts an event, or updates it if its content changed.
///
/// An update keeps `first_seen` and clears `deleted_at`: the record is back upstream.
pub async fn upsert_event(
    db: &SqlitePool,
    event: &NewEvent<'_>,
    now: i64,
) -> Result<Upsert, DbError> {
    let inserted = sqlx::query(
        "INSERT INTO events (source, kind, external_id, occurred_at, payload, first_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (source, external_id) DO NOTHING",
    )
    .bind(event.source)
    .bind(event.kind)
    .bind(event.external_id)
    .bind(event.occurred_at)
    .bind(event.payload)
    .bind(now)
    .execute(db)
    .await?
    .rows_affected();
    if inserted > 0 {
        return Ok(Upsert::Inserted);
    }

    let updated = sqlx::query(
        "UPDATE events
         SET kind = ?3, occurred_at = ?4, payload = ?5, deleted_at = NULL
         WHERE source = ?1 AND external_id = ?2
           AND (kind IS NOT ?3 OR occurred_at IS NOT ?4 OR payload IS NOT ?5
                OR deleted_at IS NOT NULL)",
    )
    .bind(event.source)
    .bind(event.external_id)
    .bind(event.kind)
    .bind(event.occurred_at)
    .bind(event.payload)
    .execute(db)
    .await?
    .rows_affected();

    Ok(if updated > 0 {
        Upsert::Updated
    } else {
        Upsert::Unchanged
    })
}

/// Inserts the count of one day, or updates it if it changed.
pub async fn upsert_daily_count(
    db: &SqlitePool,
    source: &str,
    day: &str,
    count: i64,
) -> Result<Upsert, DbError> {
    let mut conn = db.acquire().await?;
    upsert_daily_count_on(&mut conn, source, day, count).await
}

/// [`upsert_daily_count`] on one connection, for use in a transaction.
async fn upsert_daily_count_on(
    conn: &mut SqliteConnection,
    source: &str,
    day: &str,
    count: i64,
) -> Result<Upsert, DbError> {
    let inserted = sqlx::query(
        "INSERT INTO daily_counts (source, day, count) VALUES (?1, ?2, ?3)
         ON CONFLICT (source, day) DO NOTHING",
    )
    .bind(source)
    .bind(day)
    .bind(count)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if inserted > 0 {
        return Ok(Upsert::Inserted);
    }

    let updated = sqlx::query(
        "UPDATE daily_counts SET count = ?3
         WHERE source = ?1 AND day = ?2 AND count IS NOT ?3",
    )
    .bind(source)
    .bind(day)
    .bind(count)
    .execute(&mut *conn)
    .await?
    .rows_affected();

    Ok(if updated > 0 {
        Upsert::Updated
    } else {
        Upsert::Unchanged
    })
}

/// What [`replace_daily_counts`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Replaced {
    pub inserted: u64,
    pub updated: u64,
    pub unchanged: u64,
    /// Days of the window that the new counts no longer have.
    pub removed: u64,
}

/// Replaces the counts of a source from `from_day` (inclusive, `YYYY-MM-DD`)
/// with `counts`, in one transaction. Days before `from_day` stay as they are:
/// they are history that the source no longer returns.
pub async fn replace_daily_counts(
    db: &SqlitePool,
    source: &str,
    from_day: &str,
    counts: &BTreeMap<String, i64>,
) -> Result<Replaced, DbError> {
    let mut tx = db.begin().await?;
    let mut replaced = Replaced::default();

    for (day, &count) in counts {
        match upsert_daily_count_on(&mut tx, source, day, count).await? {
            Upsert::Inserted => replaced.inserted += 1,
            Upsert::Updated => replaced.updated += 1,
            Upsert::Unchanged => replaced.unchanged += 1,
        }
    }

    let stored: Vec<String> =
        sqlx::query_scalar("SELECT day FROM daily_counts WHERE source = ?1 AND day >= ?2")
            .bind(source)
            .bind(from_day)
            .fetch_all(&mut *tx)
            .await?;
    for day in stored.iter().filter(|day| !counts.contains_key(*day)) {
        replaced.removed += sqlx::query("DELETE FROM daily_counts WHERE source = ?1 AND day = ?2")
            .bind(source)
            .bind(day)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }

    tx.commit().await?;
    Ok(replaced)
}

/// The distinct kinds of the events of a source.
pub async fn event_kinds(db: &SqlitePool, source: &str) -> Result<Vec<String>, DbError> {
    Ok(
        sqlx::query_scalar("SELECT DISTINCT kind FROM events WHERE source = ?1 ORDER BY kind")
            .bind(source)
            .fetch_all(db)
            .await?,
    )
}

/// Deletes all the events of one kind: of one source, or of all sources.
/// Returns the number of deleted rows.
pub async fn purge_kind(db: &SqlitePool, source: Option<&str>, kind: &str) -> Result<u64, DbError> {
    let result = sqlx::query("DELETE FROM events WHERE kind = ?1 AND (?2 IS NULL OR source = ?2)")
        .bind(kind)
        .bind(source)
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}

/// Counts the events of one kind, like [`purge_kind`] would delete them.
pub async fn count_kind(db: &SqlitePool, source: Option<&str>, kind: &str) -> Result<i64, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE kind = ?1 AND (?2 IS NULL OR source = ?2)",
    )
    .bind(kind)
    .bind(source)
    .fetch_one(db)
    .await?)
}

/// The ids of the events of a source that are not marked as deleted.
pub async fn live_event_ids(db: &SqlitePool, source: &str) -> Result<Vec<String>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT external_id FROM events WHERE source = ?1 AND deleted_at IS NULL",
    )
    .bind(source)
    .fetch_all(db)
    .await?)
}

/// Marks events as deleted upstream, in one transaction. The rows stay in the
/// database: an upsert of the same event clears `deleted_at`.
pub async fn mark_deleted(
    db: &SqlitePool,
    source: &str,
    external_ids: &[String],
    now: i64,
) -> Result<u64, DbError> {
    let mut tx = db.begin().await?;
    let mut marked = 0;
    for external_id in external_ids {
        marked += sqlx::query(
            "UPDATE events SET deleted_at = ?3
             WHERE source = ?1 AND external_id = ?2 AND deleted_at IS NULL",
        )
        .bind(source)
        .bind(external_id)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;
    Ok(marked)
}

/// Reads the `first_seen` time of an event, if the event is already stored.
pub async fn event_first_seen(
    db: &SqlitePool,
    source: &str,
    external_id: &str,
) -> Result<Option<i64>, DbError> {
    Ok(
        sqlx::query_scalar("SELECT first_seen FROM events WHERE source = ?1 AND external_id = ?2")
            .bind(source)
            .bind(external_id)
            .fetch_optional(db)
            .await?,
    )
}

/// Reads the stored state of a source. A source that never ran has the default state.
pub async fn sync_state(db: &SqlitePool, source: &str) -> Result<SyncState, DbError> {
    let row: Option<(Option<String>, Option<i64>)> =
        sqlx::query_as("SELECT cursor, last_run FROM sync_state WHERE source = ?1")
            .bind(source)
            .fetch_optional(db)
            .await?;

    Ok(row
        .map(|(cursor, last_run)| SyncState { cursor, last_run })
        .unwrap_or_default())
}

/// Stores the state of a source after a run.
pub async fn set_sync_state(
    db: &SqlitePool,
    source: &str,
    state: &SyncState,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO sync_state (source, cursor, last_run) VALUES (?1, ?2, ?3)
         ON CONFLICT (source) DO UPDATE
         SET cursor = excluded.cursor, last_run = excluded.last_run",
    )
    .bind(source)
    .bind(state.cursor.as_deref())
    .bind(state.last_run)
    .execute(db)
    .await?;
    Ok(())
}

/// Reads the row counts and the sync state of a source.
pub async fn source_stats(db: &SqlitePool, source: &str) -> Result<SourceStats, DbError> {
    let (events, deleted, days): (i64, i64, i64) = sqlx::query_as(
        "SELECT
           (SELECT COUNT(*) FROM events WHERE source = ?1 AND deleted_at IS NULL),
           (SELECT COUNT(*) FROM events WHERE source = ?1 AND deleted_at IS NOT NULL),
           (SELECT COUNT(*) FROM daily_counts WHERE source = ?1)",
    )
    .bind(source)
    .fetch_one(db)
    .await?;

    Ok(SourceStats {
        state: sync_state(db, source).await?,
        events,
        deleted,
        days,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_753_000_000;

    fn post<'a>(payload: &'a str) -> NewEvent<'a> {
        NewEvent {
            source: "atproto",
            kind: "app.bsky.feed.post",
            external_id: "at://did:plc:abc/app.bsky.feed.post/3kabc",
            occurred_at: NOW - 60,
            payload,
        }
    }

    async fn first_seen_and_deleted_at(db: &SqlitePool) -> (i64, Option<i64>) {
        sqlx::query_as("SELECT first_seen, deleted_at FROM events")
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn insert(db: &SqlitePool, source: &str, kind: &str, external_id: &str) {
        let event = NewEvent {
            source,
            kind,
            external_id,
            occurred_at: NOW,
            payload: "{}",
        };
        upsert_event(db, &event, NOW).await.unwrap();
    }

    #[tokio::test]
    async fn purge_deletes_one_kind() {
        let db = open_in_memory().await.unwrap();
        insert(&db, "atproto", "app.bsky.feed.like", "like-1").await;
        insert(&db, "atproto", "app.bsky.feed.like", "like-2").await;
        insert(&db, "atproto", "app.bsky.feed.post", "post-1").await;
        insert(&db, "other", "app.bsky.feed.like", "like-3").await;

        assert_eq!(
            purge_kind(&db, Some("atproto"), "app.bsky.feed.like")
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            event_kinds(&db, "atproto").await.unwrap(),
            ["app.bsky.feed.post"]
        );
        // Without a source: all the sources.
        assert_eq!(
            purge_kind(&db, None, "app.bsky.feed.like").await.unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn replace_keeps_history_and_removes_days_of_the_window() {
        let db = open_in_memory().await.unwrap();
        // Before the window: history to keep.
        upsert_daily_count(&db, "gitea:x", "2025-01-01", 4)
            .await
            .unwrap();
        // In the window, but no longer in the source.
        upsert_daily_count(&db, "gitea:x", "2026-05-01", 2)
            .await
            .unwrap();
        upsert_daily_count(&db, "gitea:x", "2026-06-01", 1)
            .await
            .unwrap();
        // Another source: never touched.
        upsert_daily_count(&db, "github", "2026-05-01", 9)
            .await
            .unwrap();

        let counts = BTreeMap::from([("2026-06-01".to_owned(), 3), ("2026-07-01".to_owned(), 5)]);
        let replaced = replace_daily_counts(&db, "gitea:x", "2026-01-01", &counts)
            .await
            .unwrap();

        assert_eq!(
            replaced,
            Replaced {
                inserted: 1,
                updated: 1,
                unchanged: 0,
                removed: 1,
            }
        );
        let rows: Vec<(String, String, i64)> =
            sqlx::query_as("SELECT source, day, count FROM daily_counts ORDER BY source, day")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(
            rows,
            [
                ("gitea:x".to_owned(), "2025-01-01".to_owned(), 4),
                ("gitea:x".to_owned(), "2026-06-01".to_owned(), 3),
                ("gitea:x".to_owned(), "2026-07-01".to_owned(), 5),
                ("github".to_owned(), "2026-05-01".to_owned(), 9),
            ]
        );

        // The same counts again: nothing changes.
        let again = replace_daily_counts(&db, "gitea:x", "2026-01-01", &counts)
            .await
            .unwrap();
        assert_eq!((again.unchanged, again.removed), (2, 0));
    }

    #[tokio::test]
    async fn marked_events_are_no_longer_live() {
        let db = open_in_memory().await.unwrap();
        insert(&db, "atproto", "k", "a").await;
        insert(&db, "atproto", "k", "b").await;

        let marked = mark_deleted(&db, "atproto", &["a".to_owned(), "x".to_owned()], NOW + 1)
            .await
            .unwrap();

        assert_eq!(marked, 1);
        assert_eq!(live_event_ids(&db, "atproto").await.unwrap(), ["b"]);
        // A second mark does not move `deleted_at`.
        assert_eq!(
            mark_deleted(&db, "atproto", &["a".to_owned()], NOW + 2)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn new_database_is_empty() {
        let db = open_in_memory().await.unwrap();

        assert_eq!(
            source_stats(&db, "atproto").await.unwrap(),
            SourceStats::default()
        );
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("hairball-test-{}-{name}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[tokio::test]
    async fn writes_to_a_copy_do_not_change_the_source() {
        let source = temp_path("copy-source");
        let copy = temp_path("copy-target");
        let db = open(&source).await.unwrap();
        upsert_event(&db, &post("{}"), NOW).await.unwrap();
        db.close().await;

        let copied = open_copy(&source, &copy).await.unwrap();
        // The copy has the rows of the source.
        assert_eq!(source_stats(&copied, "atproto").await.unwrap().events, 1);
        purge_kind(&copied, None, "app.bsky.feed.post")
            .await
            .unwrap();
        assert_eq!(source_stats(&copied, "atproto").await.unwrap().events, 0);
        copied.close().await;

        // The source still has its row.
        let db = open(&source).await.unwrap();
        assert_eq!(source_stats(&db, "atproto").await.unwrap().events, 1);
        db.close().await;
        std::fs::remove_file(&source).unwrap();
        std::fs::remove_file(&copy).unwrap();
    }

    #[tokio::test]
    async fn copy_of_a_missing_database_starts_empty() {
        let source = temp_path("missing-source");
        let copy = temp_path("missing-target");

        let copied = open_copy(&source, &copy).await.unwrap();

        assert_eq!(
            source_stats(&copied, "atproto").await.unwrap(),
            SourceStats::default()
        );
        assert!(!source.exists());
        copied.close().await;
        std::fs::remove_file(&copy).unwrap();
    }

    #[tokio::test]
    async fn count_kind_matches_what_purge_deletes() {
        let db = open_in_memory().await.unwrap();
        insert(&db, "atproto", "app.bsky.feed.like", "like-1").await;
        insert(&db, "other", "app.bsky.feed.like", "like-2").await;

        assert_eq!(
            count_kind(&db, None, "app.bsky.feed.like").await.unwrap(),
            2
        );
        assert_eq!(
            count_kind(&db, Some("atproto"), "app.bsky.feed.like")
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            purge_kind(&db, None, "app.bsky.feed.like").await.unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn open_creates_the_file_and_can_reopen_it() {
        let path = std::env::temp_dir().join(format!("hairball-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);

        open(&path).await.unwrap().close().await;
        assert!(path.exists());
        // The migrations already ran: a second open must not fail.
        open(&path).await.unwrap().close().await;

        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn event_upsert_is_idempotent() {
        let db = open_in_memory().await.unwrap();

        assert_eq!(
            upsert_event(&db, &post("{}"), NOW).await.unwrap(),
            Upsert::Inserted
        );
        assert_eq!(
            upsert_event(&db, &post("{}"), NOW + 10).await.unwrap(),
            Upsert::Unchanged
        );
        assert_eq!(
            upsert_event(&db, &post(r#"{"text":"edited"}"#), NOW + 20)
                .await
                .unwrap(),
            Upsert::Updated
        );
        // `first_seen` keeps the time of the first insert.
        assert_eq!(first_seen_and_deleted_at(&db).await, (NOW, None));
    }

    #[tokio::test]
    async fn deleted_event_comes_back() {
        let db = open_in_memory().await.unwrap();
        upsert_event(&db, &post("{}"), NOW).await.unwrap();
        sqlx::query("UPDATE events SET deleted_at = ?1")
            .bind(NOW + 5)
            .execute(&db)
            .await
            .unwrap();

        assert_eq!(
            upsert_event(&db, &post("{}"), NOW + 10).await.unwrap(),
            Upsert::Updated
        );
        assert_eq!(first_seen_and_deleted_at(&db).await, (NOW, None));
    }

    #[tokio::test]
    async fn daily_count_upsert_is_idempotent() {
        let db = open_in_memory().await.unwrap();
        let upsert = |count| upsert_daily_count(&db, "github", "2026-07-20", count);

        assert_eq!(upsert(3).await.unwrap(), Upsert::Inserted);
        assert_eq!(upsert(3).await.unwrap(), Upsert::Unchanged);
        assert_eq!(upsert(5).await.unwrap(), Upsert::Updated);
    }

    #[tokio::test]
    async fn sync_state_round_trip() {
        let db = open_in_memory().await.unwrap();
        assert_eq!(
            sync_state(&db, "lastfm").await.unwrap(),
            SyncState::default()
        );

        let first = SyncState {
            cursor: Some("1753000000".to_owned()),
            last_run: Some(NOW),
        };
        set_sync_state(&db, "lastfm", &first).await.unwrap();
        assert_eq!(sync_state(&db, "lastfm").await.unwrap(), first);

        let second = SyncState {
            cursor: None,
            last_run: Some(NOW + 3600),
        };
        set_sync_state(&db, "lastfm", &second).await.unwrap();
        assert_eq!(sync_state(&db, "lastfm").await.unwrap(), second);
    }

    #[tokio::test]
    async fn stats_count_rows_per_source() {
        let db = open_in_memory().await.unwrap();
        upsert_event(&db, &post("{}"), NOW).await.unwrap();
        let other = NewEvent {
            external_id: "at://did:plc:abc/app.bsky.feed.post/3kdef",
            ..post("{}")
        };
        upsert_event(&db, &other, NOW).await.unwrap();
        sqlx::query("UPDATE events SET deleted_at = ?1 WHERE external_id = ?2")
            .bind(NOW)
            .bind(other.external_id)
            .execute(&db)
            .await
            .unwrap();
        upsert_daily_count(&db, "github", "2026-07-20", 3)
            .await
            .unwrap();
        upsert_daily_count(&db, "github", "2026-07-21", 1)
            .await
            .unwrap();

        let atproto = source_stats(&db, "atproto").await.unwrap();
        assert_eq!((atproto.events, atproto.deleted, atproto.days), (1, 1, 0));

        let github = source_stats(&db, "github").await.unwrap();
        assert_eq!((github.events, github.deleted, github.days), (0, 0, 2));
    }
}
