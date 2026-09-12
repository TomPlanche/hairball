//! GitHub adapter: the contribution calendar of a user, with the GraphQL API.
//!
//! The calendar gives one total per day, so the rows go to `daily_counts`.
//! It can include private contributions (if the profile setting allows it):
//! the CLI stores only the total per day, never a repository name.

use jiff::{Timestamp, civil, tz::TimeZone};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tracing::debug;

use super::{SourceError, SyncContext, SyncReport, api_error};
use crate::{
    config::GithubConfig,
    db::{self, SyncState},
    http,
};

/// The source id of the adapter.
pub const SOURCE: &str = "github";

const CREATED_AT_QUERY: &str = "query ($login: String!) { user(login: $login) { createdAt } }";

const CALENDAR_QUERY: &str = "query ($login: String!, $from: DateTime!, $to: DateTime!) {
  user(login: $login) {
    contributionsCollection(from: $from, to: $to) {
      contributionCalendar {
        weeks { contributionDays { date contributionCount } }
      }
    }
  }
}";

#[derive(Debug, Clone)]
pub struct Github {
    user: String,
    token_env: String,
    /// Replaces the environment variable. For tests.
    token: Option<String>,
    api: String,
}

#[derive(Deserialize)]
struct GraphqlResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphqlError>,
}

#[derive(Deserialize)]
struct GraphqlError {
    message: String,
}

#[derive(Deserialize)]
struct UserData<T> {
    user: Option<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreatedAt {
    created_at: Timestamp,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Contributions {
    contributions_collection: Collection,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Collection {
    contribution_calendar: Calendar,
}

#[derive(Deserialize)]
struct Calendar {
    weeks: Vec<Week>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Week {
    contribution_days: Vec<Day>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Day {
    /// `YYYY-MM-DD`, as GitHub groups the contributions.
    date: String,
    contribution_count: i64,
}

impl Github {
    pub fn new(config: &GithubConfig) -> Self {
        Self {
            user: config.user.clone(),
            token_env: config.token_env.clone(),
            token: None,
            api: "https://api.github.com".to_owned(),
        }
    }

    /// Replaces the API URL and the token. For tests.
    pub fn with_api(mut self, api: &str, token: &str) -> Self {
        self.api = api.to_owned();
        self.token = Some(token.to_owned());
        self
    }

    pub async fn sync(&self, ctx: &SyncContext<'_>) -> Result<SyncReport, SourceError> {
        let token = self.token()?;
        let created: CreatedAt = self
            .query(ctx, &token, CREATED_AT_QUERY, json!({ "login": self.user }))
            .await?;

        // One query covers 1 year at most. A full run reads every year since the
        // creation of the account; an incremental run, the current and the previous year.
        let now_year = ctx.now.to_zoned(TimeZone::UTC).year();
        let first_year = created.created_at.to_zoned(TimeZone::UTC).year();
        let start_year = if ctx.full {
            first_year
        } else {
            first_year.max(now_year - 1)
        };

        let mut report = SyncReport::default();
        // One progress step per year.
        ctx.progress
            .set_length(Some(u64::try_from(now_year - start_year + 1).unwrap_or(0)));
        for (done, year) in (start_year..=now_year).enumerate() {
            ctx.progress.set_position(done as u64);
            ctx.progress.set_message(year.to_string());
            let (from, to) = year_range(year, created.created_at, ctx.now);
            let data: Contributions = self
                .query(
                    ctx,
                    &token,
                    CALENDAR_QUERY,
                    json!({ "login": self.user, "from": from.to_string(), "to": to.to_string() }),
                )
                .await?;

            let days = data
                .contributions_collection
                .contribution_calendar
                .weeks
                .into_iter()
                .flat_map(|week| week.contribution_days);
            let mut count = 0;
            for day in days {
                let upsert =
                    db::upsert_daily_count(ctx.db, SOURCE, &day.date, day.contribution_count)
                        .await?;
                report.count(upsert);
                count += 1;
            }
            debug!(year, days = count, "calendar read");
        }

        db::set_sync_state(
            ctx.db,
            SOURCE,
            &SyncState {
                cursor: None,
                last_run: Some(ctx.now.as_second()),
            },
        )
        .await?;
        Ok(report)
    }

    fn token(&self) -> Result<String, SourceError> {
        if let Some(token) = &self.token {
            return Ok(token.clone());
        }
        std::env::var(&self.token_env)
            .ok()
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| SourceError::MissingSecret(self.token_env.clone()))
    }

    /// Sends a GraphQL query about the user, and returns `data.user`.
    ///
    /// GraphQL reports some errors with HTTP 200 and an `errors` array.
    async fn query<T: DeserializeOwned>(
        &self,
        ctx: &SyncContext<'_>,
        token: &str,
        query: &str,
        variables: Value,
    ) -> Result<T, SourceError> {
        let url = format!("{}/graphql", self.api);
        let request = ctx
            .client
            .post(&url)
            .header("Authorization", format!("bearer {token}"))
            .json(&json!({ "query": query, "variables": variables }));
        let response = http::send_with_retry(request).await?;
        if !response.status().is_success() {
            return Err(api_error(&url, response).await);
        }

        let body: GraphqlResponse<UserData<T>> = response.json().await?;
        if !body.errors.is_empty() {
            let message = body
                .errors
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(SourceError::Graphql { url, message });
        }
        body.data
            .and_then(|data| data.user)
            .ok_or_else(|| SourceError::Graphql {
                url,
                message: format!("no user `{}`", self.user),
            })
    }
}

/// The query range of one year: from 1 January 00:00:00 UTC (or the creation
/// of the account) to 31 December 23:59:59 UTC (or now).
fn year_range(year: i16, created_at: Timestamp, now: Timestamp) -> (Timestamp, Timestamp) {
    let start = |year: i16| {
        civil::date(year, 1, 1)
            .to_zoned(TimeZone::UTC)
            .map(|zoned| zoned.timestamp())
            .unwrap_or(Timestamp::UNIX_EPOCH)
    };
    let end = Timestamp::from_second(start(year + 1).as_second() - 1).unwrap_or(now);
    (start(year).max(created_at), end.min(now))
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, header, method, path},
    };

    use super::*;

    const NOW: &str = "2026-11-12T12:00:00Z";

    fn github(server: &MockServer) -> Github {
        let config: GithubConfig =
            toml::from_str("user = \"TomPlanche\"\ntoken_env = \"GITHUB_TOKEN\"").unwrap();
        Github::new(&config).with_api(&server.uri(), "secret")
    }

    async fn mount_created_at(server: &MockServer, created_at: &str) {
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(header("authorization", "bearer secret"))
            .and(body_partial_json(json!({ "query": CREATED_AT_QUERY })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "user": { "createdAt": created_at } }
            })))
            .mount(server)
            .await;
    }

    /// The calendar of one year, with the given days. Must be read `reads` times.
    async fn mount_year(
        server: &MockServer,
        from: &str,
        to: &str,
        days: &[(&str, i64)],
        reads: u64,
    ) {
        let days: Vec<Value> = days
            .iter()
            .map(|(date, count)| json!({ "date": date, "contributionCount": count }))
            .collect();
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(header("authorization", "bearer secret"))
            .and(body_partial_json(json!({
                "query": CALENDAR_QUERY,
                "variables": { "login": "TomPlanche", "from": from, "to": to }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "user": { "contributionsCollection": { "contributionCalendar": {
                    "weeks": [{ "contributionDays": days }]
                } } } }
            })))
            .expect(reads)
            .mount(server)
            .await;
    }

    async fn sync(github: &Github, db: &SqlitePool, full: bool) -> Result<SyncReport, SourceError> {
        let client = http::client().unwrap();
        let progress = crate::progress::none();
        let ctx = SyncContext {
            db,
            client: &client,
            now: NOW.parse().unwrap(),
            full,
            progress: &progress,
        };
        github.sync(&ctx).await
    }

    async fn days(db: &SqlitePool) -> Vec<(String, i64)> {
        sqlx::query_as("SELECT day, count FROM daily_counts WHERE source = 'github' ORDER BY day")
            .fetch_all(db)
            .await
            .unwrap()
    }

    #[test]
    fn year_range_is_cut_by_creation_and_now() {
        let created = "2024-03-10T08:00:00Z".parse().unwrap();
        let now = NOW.parse().unwrap();

        let range = |year| {
            let (from, to) = year_range(year, created, now);
            (from.to_string(), to.to_string())
        };

        assert_eq!(
            range(2024),
            (
                "2024-03-10T08:00:00Z".to_owned(),
                "2024-12-31T23:59:59Z".to_owned()
            )
        );
        assert_eq!(
            range(2025),
            (
                "2025-01-01T00:00:00Z".to_owned(),
                "2025-12-31T23:59:59Z".to_owned()
            )
        );
        assert_eq!(
            range(2026),
            ("2026-01-01T00:00:00Z".to_owned(), NOW.to_owned())
        );
    }

    #[tokio::test]
    async fn full_sync_reads_every_year_since_the_account_creation() {
        let server = MockServer::start().await;
        mount_created_at(&server, "2024-03-10T08:00:00Z").await;
        mount_year(
            &server,
            "2024-03-10T08:00:00Z",
            "2024-12-31T23:59:59Z",
            &[("2024-03-11", 2)],
            1,
        )
        .await;
        mount_year(
            &server,
            "2025-01-01T00:00:00Z",
            "2025-12-31T23:59:59Z",
            &[("2025-06-01", 0), ("2025-06-02", 7)],
            1,
        )
        .await;
        mount_year(
            &server,
            "2026-01-01T00:00:00Z",
            NOW,
            &[("2026-11-12", 3)],
            1,
        )
        .await;
        let db = db::open_in_memory().await.unwrap();

        let report = sync(&github(&server), &db, true).await.unwrap();

        assert_eq!(report.inserted, 4);
        assert_eq!(
            days(&db).await,
            [
                ("2024-03-11".to_owned(), 2),
                ("2025-06-01".to_owned(), 0),
                ("2025-06-02".to_owned(), 7),
                ("2026-11-12".to_owned(), 3),
            ]
        );
        assert_eq!(
            db::sync_state(&db, SOURCE).await.unwrap().last_run,
            Some(NOW.parse::<Timestamp>().unwrap().as_second())
        );
    }

    #[tokio::test]
    async fn incremental_sync_reads_the_current_and_the_previous_year() {
        let server = MockServer::start().await;
        mount_created_at(&server, "2019-05-01T00:00:00Z").await;
        mount_year(
            &server,
            "2024-01-01T00:00:00Z",
            "2024-12-31T23:59:59Z",
            &[],
            0,
        )
        .await;
        mount_year(
            &server,
            "2025-01-01T00:00:00Z",
            "2025-12-31T23:59:59Z",
            &[("2025-06-02", 7)],
            2,
        )
        .await;
        mount_year(
            &server,
            "2026-01-01T00:00:00Z",
            NOW,
            &[("2026-11-12", 3)],
            2,
        )
        .await;
        let db = db::open_in_memory().await.unwrap();
        let github = github(&server);

        let first = sync(&github, &db, false).await.unwrap();
        let second = sync(&github, &db, false).await.unwrap();

        assert_eq!(first.inserted, 2);
        assert_eq!(
            (second.inserted, second.updated, second.unchanged),
            (0, 0, 2)
        );
    }

    #[tokio::test]
    async fn graphql_error_with_http_200_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "user": null },
                "errors": [{
                    "type": "NOT_FOUND",
                    "message": "Could not resolve to a User with the login of 'TomPlanche'."
                }]
            })))
            .mount(&server)
            .await;
        let db = db::open_in_memory().await.unwrap();

        let error = sync(&github(&server), &db, false).await.unwrap_err();

        assert!(
            error.to_string().ends_with(
                "returned a GraphQL error: Could not resolve to a User with the login of 'TomPlanche'."
            ),
            "{error}"
        );
        assert!(days(&db).await.is_empty());
    }

    #[tokio::test]
    async fn bad_token_is_an_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "message": "Bad credentials"
            })))
            .mount(&server)
            .await;
        let db = db::open_in_memory().await.unwrap();

        let error = sync(&github(&server), &db, false).await.unwrap_err();

        assert!(
            error
                .to_string()
                .ends_with("answered HTTP 401 Unauthorized: Bad credentials"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn missing_token_stops_before_any_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let config: GithubConfig =
            toml::from_str("user = \"a\"\ntoken_env = \"HAIRBALL_TEST_UNSET_TOKEN\"").unwrap();
        let mut github = Github::new(&config);
        github.api = server.uri();
        let db = db::open_in_memory().await.unwrap();

        let error = sync(&github, &db, false).await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "the environment variable `HAIRBALL_TEST_UNSET_TOKEN` is not set or is empty"
        );
    }
}
