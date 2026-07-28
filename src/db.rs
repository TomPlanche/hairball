//! SQLite storage: pool, migrations, upserts and queries.
//!
//! All times are unix seconds, UTC. Every write is idempotent: the same input
//! two times gives [`Upsert::Unchanged`] the second time.

use std::{path::Path, str::FromStr};

use sqlx::{
    SqlitePool,
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
    let inserted = sqlx::query(
        "INSERT INTO daily_counts (source, day, count) VALUES (?1, ?2, ?3)
         ON CONFLICT (source, day) DO NOTHING",
    )
    .bind(source)
    .bind(day)
    .bind(count)
    .execute(db)
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
    .execute(db)
    .await?
    .rows_affected();

    Ok(if updated > 0 {
        Upsert::Updated
    } else {
        Upsert::Unchanged
    })
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

    #[tokio::test]
    async fn new_database_is_empty() {
        let db = open_in_memory().await.unwrap();

        assert_eq!(
            source_stats(&db, "atproto").await.unwrap(),
            SourceStats::default()
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
