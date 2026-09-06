//! Calendar JSON export, for a GitHub-style heatmap.
//!
//! Each event is put in a day of the configured time zone. The `daily_counts`
//! days come already grouped by their source (GitHub, Gitea): they are kept as
//! they are, so their day boundary can differ from the one of the events.

use std::collections::BTreeMap;

use jiff::{Timestamp, ToSpan, civil::Date, tz::TimeZone};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::{db::DbError, registry, time};

/// The export of one date range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Calendar {
    pub timezone: String,
    /// First day (inclusive), `YYYY-MM-DD`.
    pub from: String,
    /// Last day (inclusive), `YYYY-MM-DD`.
    pub to: String,
    /// One entry per source that has activity in the range.
    pub sources: BTreeMap<String, SourceInfo>,
    /// The days with activity, in order. A day without activity is absent.
    pub days: Vec<Day>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceInfo {
    pub label: String,
    /// The highest count of one day: the frontend normalizes the intensity of
    /// each source with it, so that a busy source does not hide the others.
    pub max: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Day {
    pub date: String,
    pub total: i64,
    pub by_source: BTreeMap<String, i64>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("database error")]
    Db(#[from] DbError),

    #[error("invalid date range")]
    Range(#[from] jiff::Error),
}

impl From<sqlx::Error> for ExportError {
    fn from(error: sqlx::Error) -> Self {
        Self::Db(DbError::Query(error))
    }
}

/// The export key and label of an event, from its source and kind.
fn event_key(source: &str, kind: &str) -> (String, String) {
    match source {
        "atproto" => {
            let app = registry::app_of(kind);
            (app.key, app.label)
        }
        "lastfm" => ("lastfm".to_owned(), "Last.fm".to_owned()),
        "bsky_bookmarks" => ("bookmarks".to_owned(), "Bluesky bookmarks".to_owned()),
        other => (other.to_owned(), other.to_owned()),
    }
}

/// The export key and label of a `daily_counts` source.
fn daily_key(source: &str) -> (String, String) {
    if source == "github" {
        return ("github".to_owned(), "GitHub".to_owned());
    }
    if let Some(name) = source.strip_prefix("gitea:") {
        let mut chars = name.chars();
        let label = chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect())
            .unwrap_or_default();
        return (name.to_owned(), label);
    }
    (source.to_owned(), source.to_owned())
}

/// Builds the calendar from `from` to `to` (both inclusive), with the days of
/// the events in `tz`. Events marked as deleted upstream are left out.
pub async fn build(
    db: &SqlitePool,
    tz: &TimeZone,
    tz_name: &str,
    from: Date,
    to: Date,
) -> Result<Calendar, ExportError> {
    let start = from.to_zoned(tz.clone())?.timestamp();
    let end = to.checked_add(1.day())?.to_zoned(tz.clone())?.timestamp();

    let mut counts: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
    let mut labels: BTreeMap<String, String> = BTreeMap::new();

    let events: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT source, kind, occurred_at FROM events
         WHERE deleted_at IS NULL AND occurred_at >= ?1 AND occurred_at < ?2",
    )
    .bind(start.as_second())
    .bind(end.as_second())
    .fetch_all(db)
    .await?;
    for (source, kind, occurred_at) in events {
        let Ok(at) = Timestamp::from_second(occurred_at) else {
            continue;
        };
        let (key, label) = event_key(&source, &kind);
        let day = time::day(at, tz).to_string();
        *counts
            .entry(day)
            .or_default()
            .entry(key.clone())
            .or_default() += 1;
        labels.entry(key).or_insert(label);
    }

    let daily: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT source, day, count FROM daily_counts
         WHERE day >= ?1 AND day <= ?2 AND count > 0",
    )
    .bind(from.to_string())
    .bind(to.to_string())
    .fetch_all(db)
    .await?;
    for (source, day, count) in daily {
        let (key, label) = daily_key(&source);
        *counts
            .entry(day)
            .or_default()
            .entry(key.clone())
            .or_default() += count;
        labels.entry(key).or_insert(label);
    }

    let mut sources: BTreeMap<String, SourceInfo> = labels
        .into_iter()
        .map(|(key, label)| (key, SourceInfo { label, max: 0 }))
        .collect();
    let days = counts
        .into_iter()
        .map(|(date, by_source)| {
            for (key, &count) in &by_source {
                if let Some(info) = sources.get_mut(key) {
                    info.max = info.max.max(count);
                }
            }
            Day {
                date,
                total: by_source.values().sum(),
                by_source,
            }
        })
        .collect();

    Ok(Calendar {
        timezone: tz_name.to_owned(),
        from: from.to_string(),
        to: to.to_string(),
        sources,
        days,
    })
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;
    use serde_json::json;

    use super::*;
    use crate::db::{self, NewEvent};

    async fn event(db: &SqlitePool, source: &str, kind: &str, id: &str, at: &str) {
        let at: Timestamp = at.parse().unwrap();
        let event = NewEvent {
            source,
            kind,
            external_id: id,
            occurred_at: at.as_second(),
            payload: "{}",
        };
        db::upsert_event(db, &event, 0).await.unwrap();
    }

    async fn calendar(db: &SqlitePool) -> serde_json::Value {
        let paris = TimeZone::get("Europe/Paris").unwrap();
        let calendar = build(
            db,
            &paris,
            "Europe/Paris",
            date(2026, 1, 1),
            date(2026, 12, 31),
        )
        .await
        .unwrap();
        serde_json::to_value(calendar).unwrap()
    }

    #[tokio::test]
    async fn sources_are_grouped_per_day() {
        let db = db::open_in_memory().await.unwrap();
        event(
            &db,
            "atproto",
            "app.bsky.feed.post",
            "p1",
            "2026-11-12T10:00:00Z",
        )
        .await;
        event(
            &db,
            "atproto",
            "app.bsky.feed.like",
            "l1",
            "2026-11-12T11:00:00Z",
        )
        .await;
        event(
            &db,
            "atproto",
            "sh.tangled.repo",
            "t1",
            "2026-11-12T12:00:00Z",
        )
        .await;
        event(&db, "lastfm", "scrobble", "s1", "2026-11-12T13:00:00Z").await;
        event(&db, "lastfm", "scrobble", "s2", "2026-11-11T13:00:00Z").await;
        event(&db, "lastfm", "scrobble", "s3", "2026-11-11T14:00:00Z").await;
        db::upsert_daily_count(&db, "github", "2026-11-12", 3)
            .await
            .unwrap();
        db::upsert_daily_count(&db, "gitea:codeberg", "2026-11-11", 2)
            .await
            .unwrap();

        assert_eq!(
            calendar(&db).await,
            json!({
                "timezone": "Europe/Paris",
                "from": "2026-01-01",
                "to": "2026-12-31",
                "sources": {
                    "bluesky": { "label": "Bluesky", "max": 2 },
                    "codeberg": { "label": "Codeberg", "max": 2 },
                    "github": { "label": "GitHub", "max": 3 },
                    "lastfm": { "label": "Last.fm", "max": 2 },
                    "tangled": { "label": "Tangled", "max": 1 }
                },
                "days": [
                    { "date": "2026-11-11", "total": 4, "by_source": { "codeberg": 2, "lastfm": 2 } },
                    {
                        "date": "2026-11-12",
                        "total": 7,
                        "by_source": { "bluesky": 2, "github": 3, "lastfm": 1, "tangled": 1 }
                    }
                ]
            })
        );
    }

    #[tokio::test]
    async fn events_use_the_day_of_the_time_zone() {
        let db = db::open_in_memory().await.unwrap();
        // 23:30 UTC on 15 January is 16 January in Paris (UTC+1 in winter).
        event(&db, "lastfm", "scrobble", "s1", "2026-01-15T23:30:00Z").await;
        // 31 December 23:30 UTC is already 2027 in Paris: out of the range.
        event(&db, "lastfm", "scrobble", "s2", "2026-12-31T23:30:00Z").await;
        // 31 December 2025 23:30 UTC is 1 January 2026 in Paris: in the range.
        event(&db, "lastfm", "scrobble", "s3", "2025-12-31T23:30:00Z").await;

        let days = calendar(&db).await["days"].clone();

        assert_eq!(days[0]["date"], "2026-01-01");
        assert_eq!(days[1]["date"], "2026-01-16");
        assert_eq!(days.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn deleted_events_and_zero_days_are_left_out() {
        let db = db::open_in_memory().await.unwrap();
        event(
            &db,
            "atproto",
            "app.bsky.feed.post",
            "p1",
            "2026-11-12T10:00:00Z",
        )
        .await;
        event(
            &db,
            "atproto",
            "app.bsky.feed.post",
            "p2",
            "2026-11-12T11:00:00Z",
        )
        .await;
        db::mark_deleted(&db, "atproto", &["p2".to_owned()], 0)
            .await
            .unwrap();
        // GitHub stores days with 0 contributions: they are not activity.
        db::upsert_daily_count(&db, "github", "2026-11-10", 0)
            .await
            .unwrap();

        let calendar = calendar(&db).await;

        assert_eq!(calendar["days"].as_array().unwrap().len(), 1);
        assert_eq!(calendar["days"][0]["total"], 1);
        assert!(calendar["sources"].get("github").is_none());
    }

    #[test]
    fn keys_of_the_sources() {
        assert_eq!(
            event_key("atproto", "com.tomplanche.atkuzu.result").0,
            "com.tomplanche.atkuzu"
        );
        assert_eq!(event_key("bsky_bookmarks", "bookmark").0, "bookmarks");
        assert_eq!(
            daily_key("gitea:forge-perso"),
            ("forge-perso".to_owned(), "Forge-perso".to_owned())
        );
    }
}
