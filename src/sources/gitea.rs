//! Gitea adapter: the contribution heatmap of a user, for Codeberg, Forgejo and
//! Gitea (they share the `/api/v1` endpoints).
//!
//! The heatmap gives aggregates only (contributions per 15-minute slot), so the
//! rows go to `daily_counts`, one per day.
//!
//! Privacy: the request never has an `Authorization` header. With a token,
//! older Gitea versions computed the heatmap with the full repository access
//! of the token, so private activity leaked into the result. Anonymous calls
//! return public activity only.

use std::collections::BTreeMap;

use jiff::{Timestamp, ToSpan, tz::TimeZone};
use serde::Deserialize;
use tracing::{debug, warn};

use super::{SourceError, SyncContext, SyncReport, api_error};
use crate::{
    config::GiteaConfig,
    db::{self, SyncState},
    http, time,
};

/// The heatmap covers about the last year. The CLI replaces only the days of
/// this shorter window: older days stay, even if the heatmap still has them.
const WINDOW_DAYS: i64 = 360;

#[derive(Debug, Clone)]
pub struct Gitea {
    name: String,
    /// Base URL of the instance, without a trailing `/`.
    url: String,
    user: String,
    tz: TimeZone,
}

/// One slot of the heatmap.
#[derive(Debug, Deserialize)]
struct Slot {
    timestamp: i64,
    contributions: i64,
}

impl Gitea {
    pub fn new(config: &GiteaConfig, tz: TimeZone) -> Self {
        Self {
            name: config.name.clone(),
            url: config.url.trim_end_matches('/').to_owned(),
            user: config.user.clone(),
            tz,
        }
    }

    /// The source id: `gitea:<name>`.
    pub fn id(&self) -> String {
        format!("gitea:{}", self.name)
    }

    pub async fn sync(&self, ctx: &SyncContext<'_>) -> Result<SyncReport, SourceError> {
        let id = self.id();
        let url = format!("{}/api/v1/users/{}/heatmap", self.url, self.user);

        // One request: no length, only an activity indicator.
        ctx.progress.set_length(None);
        ctx.progress.set_message("heatmap".to_owned());
        // No `Authorization` header, on purpose: see the module documentation.
        let response = http::send_with_retry(ctx.client.get(&url)).await?;
        if !response.status().is_success() {
            return Err(api_error(&url, response).await);
        }
        let slots: Vec<Slot> = response.json().await?;
        debug!(source = %id, slots = slots.len(), "heatmap read");

        let counts = self.daily_counts(&slots);
        let today = time::day(ctx.now, &self.tz);
        let from_day = today.saturating_sub(WINDOW_DAYS.days()).to_string();
        let replaced = db::replace_daily_counts(ctx.db, &id, &from_day, &counts).await?;

        db::set_sync_state(
            ctx.db,
            &id,
            &SyncState {
                cursor: None,
                last_run: Some(ctx.now.as_second()),
            },
        )
        .await?;

        Ok(SyncReport {
            inserted: replaced.inserted,
            updated: replaced.updated,
            unchanged: replaced.unchanged,
            deleted: replaced.removed,
            purged: 0,
        })
    }

    /// Sums the slots per day, in the configured time zone.
    fn daily_counts(&self, slots: &[Slot]) -> BTreeMap<String, i64> {
        let mut counts = BTreeMap::new();
        for slot in slots {
            let Ok(at) = Timestamp::from_second(slot.timestamp) else {
                warn!(source = %self.id(), timestamp = slot.timestamp, "slot time out of range");
                continue;
            };
            *counts
                .entry(time::day(at, &self.tz).to_string())
                .or_insert(0) += slot.contributions;
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;
    use wiremock::{
        Mock, MockServer, Request, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;

    /// The real heatmap of `tom_planche` on Codeberg (Forgejo 17), 2026-11-12.
    const FIXTURE: &str = include_str!("../../tests/fixtures/codeberg_heatmap.json");

    fn gitea(server: &MockServer, tz: &str) -> Gitea {
        let config: GiteaConfig = toml::from_str(&format!(
            "name = \"codeberg\"\nurl = \"{}/\"\nuser = \"tom_planche\"",
            server.uri()
        ))
        .unwrap();
        Gitea::new(&config, TimeZone::get(tz).unwrap())
    }

    async fn mount_heatmap(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/v1/users/tom_planche/heatmap"))
            // Privacy: the request must not carry any credential.
            .and(|request: &Request| !request.headers.contains_key("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE))
            .expect(1..)
            .mount(server)
            .await;
    }

    async fn sync(gitea: &Gitea, db: &SqlitePool) -> Result<SyncReport, SourceError> {
        let client = http::client().unwrap();
        let progress = crate::progress::none();
        let ctx = SyncContext {
            db,
            client: &client,
            now: "2026-11-12T12:00:00Z".parse().unwrap(),
            full: false,
            progress: &progress,
        };
        gitea.sync(&ctx).await
    }

    async fn days(db: &SqlitePool) -> Vec<(String, i64)> {
        sqlx::query_as(
            "SELECT day, count FROM daily_counts WHERE source = 'gitea:codeberg' ORDER BY day",
        )
        .fetch_all(db)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn heatmap_is_summed_per_day_in_paris() {
        let server = MockServer::start().await;
        mount_heatmap(&server).await;
        let db = db::open_in_memory().await.unwrap();

        let report = sync(&gitea(&server, "Europe/Paris"), &db).await.unwrap();

        assert_eq!(report.inserted, 11);
        // Expected values computed with Python `zoneinfo`, from the same fixture.
        let expected = [
            ("2026-04-13", 193),
            ("2026-07-09", 1),
            ("2026-07-22", 1),
            ("2026-07-23", 1),
            ("2026-07-27", 17),
            ("2026-08-16", 1),
            ("2026-08-18", 1),
            ("2026-08-19", 9),
            ("2026-08-20", 12),
            ("2026-08-21", 7),
            ("2026-09-02", 1),
        ];
        assert_eq!(
            days(&db).await,
            expected.map(|(day, count)| (day.to_owned(), count))
        );
    }

    #[tokio::test]
    async fn day_boundary_follows_the_time_zone() {
        let server = MockServer::start().await;
        mount_heatmap(&server).await;
        let db = db::open_in_memory().await.unwrap();

        sync(&gitea(&server, "UTC"), &db).await.unwrap();

        // 22:00 and 22:30 UTC on 19 August are 20 August in Paris, not in UTC.
        let days = days(&db).await;
        assert!(days.contains(&("2026-08-19".to_owned(), 18)), "{days:?}");
        assert!(days.contains(&("2026-08-20".to_owned(), 3)), "{days:?}");
    }

    #[tokio::test]
    async fn second_run_changes_nothing_and_keeps_history() {
        let server = MockServer::start().await;
        mount_heatmap(&server).await;
        let db = db::open_in_memory().await.unwrap();
        // Older than the window: history that the heatmap no longer returns.
        db::upsert_daily_count(&db, "gitea:codeberg", "2025-03-01", 5)
            .await
            .unwrap();
        // In the window, but no longer in the heatmap (a deleted repository).
        db::upsert_daily_count(&db, "gitea:codeberg", "2026-06-01", 2)
            .await
            .unwrap();
        let gitea = gitea(&server, "Europe/Paris");

        let first = sync(&gitea, &db).await.unwrap();
        let second = sync(&gitea, &db).await.unwrap();

        assert_eq!((first.inserted, first.deleted), (11, 1));
        assert_eq!(
            (second.inserted, second.updated, second.unchanged),
            (0, 0, 11)
        );
        let days = days(&db).await;
        assert_eq!(days[0], ("2025-03-01".to_owned(), 5));
        assert!(!days.iter().any(|(day, _)| day == "2026-06-01"));
    }

    #[tokio::test]
    async fn unknown_user_is_an_error_with_the_message() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "message": "user redirect does not exist [name: tom_planche]",
                "url": "https://codeberg.org/api/swagger"
            })))
            .mount(&server)
            .await;
        let db = db::open_in_memory().await.unwrap();

        let error = sync(&gitea(&server, "UTC"), &db).await.unwrap_err();

        assert!(
            error.to_string().ends_with(
                "answered HTTP 404 Not Found: user redirect does not exist [name: tom_planche]"
            ),
            "{error}"
        );
    }
}
