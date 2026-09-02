//! Bluesky bookmarks adapter: one event per saved post.
//!
//! Bookmarks are private and stored off-protocol: they are not records of the
//! repo. Only `app.bsky.bookmark.getBookmarks`, with an authenticated session,
//! returns them. The PDS of the account forwards the call to the AppView.
//!
//! A `bookmarkView` (lexicon `app.bsky.bookmark.defs`) has a `subject` (the
//! strong ref of the post), an optional `createdAt` and an `item` (the view of
//! the post). Without `createdAt`, the event time is `first_seen`.

use std::collections::HashSet;

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use super::{SourceError, SyncContext, SyncReport, api_error};
use crate::{
    config::BskyBookmarksConfig,
    db::{self, NewEvent, SyncState},
    http,
    identity::{self, IdentityEndpoints},
    time,
};

/// The source id of the adapter.
pub const SOURCE: &str = "bsky_bookmarks";

/// The kind of every bookmark event.
pub const KIND: &str = "bookmark";

/// Bookmarks per page: the maximum of the API.
const PAGE_SIZE: &str = "100";

#[derive(Debug, Clone)]
pub struct BskyBookmarks {
    enabled: bool,
    identifier: String,
    app_password_env: String,
    /// Replaces the environment variable. For tests.
    password: Option<String>,
    endpoints: IdentityEndpoints,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    access_jwt: String,
    refresh_jwt: String,
}

#[derive(Deserialize)]
struct BookmarkPage {
    cursor: Option<String>,
    bookmarks: Vec<Value>,
}

/// What a page request gave.
enum PageResult {
    Page(BookmarkPage),
    /// The access token expired: log in again, then retry the page.
    ExpiredToken,
}

impl BskyBookmarks {
    pub fn new(config: &BskyBookmarksConfig) -> Self {
        Self {
            enabled: config.enabled,
            identifier: config.identifier.clone(),
            app_password_env: config.app_password_env.clone(),
            password: None,
            endpoints: IdentityEndpoints::default(),
        }
    }

    /// Replaces the identity endpoints and the app password. For tests.
    pub fn with_test_setup(mut self, endpoints: IdentityEndpoints, password: &str) -> Self {
        self.endpoints = endpoints;
        self.password = Some(password.to_owned());
        self
    }

    pub async fn sync(&self, ctx: &SyncContext<'_>) -> Result<SyncReport, SourceError> {
        // A disabled source must also be true for the database content.
        if !self.enabled {
            let purged = db::purge_kind(ctx.db, Some(SOURCE), KIND).await?;
            if purged > 0 {
                info!(rows = purged, "bookmarks disabled: purged");
            }
            return Ok(SyncReport {
                purged,
                ..SyncReport::default()
            });
        }

        let password = self.password()?;
        let pds = identity::resolve(ctx.client, &self.endpoints, &self.identifier)
            .await?
            .pds;
        let mut session = self.login(ctx, &pds, &password).await?;

        let mut report = SyncReport::default();
        let mut seen = HashSet::new();
        let mut cursor: Option<String> = None;
        let mut relogged = false;
        loop {
            let page = match self.page(ctx, &pds, &session, cursor.as_deref()).await? {
                PageResult::Page(page) => page,
                PageResult::ExpiredToken if !relogged => {
                    warn!("access token expired: log in again");
                    relogged = true;
                    session = self.login(ctx, &pds, &password).await?;
                    continue;
                }
                PageResult::ExpiredToken => {
                    return Err(SourceError::Api {
                        url: format!("{pds}/xrpc/app.bsky.bookmark.getBookmarks"),
                        status: StatusCode::UNAUTHORIZED,
                        message: "the access token expired again after a new login".to_owned(),
                    });
                }
            };

            for bookmark in &page.bookmarks {
                if let Some(uri) = self.store(ctx, bookmark, &mut report).await? {
                    seen.insert(uri);
                }
            }
            match page.cursor {
                Some(next) if !page.bookmarks.is_empty() => cursor = Some(next),
                _ => break,
            }
        }

        // The run read every page: a stored bookmark that is missing was removed.
        let missing: Vec<String> = db::live_event_ids(ctx.db, SOURCE)
            .await?
            .into_iter()
            .filter(|uri| !seen.contains(uri))
            .collect();
        report.deleted = db::mark_deleted(ctx.db, SOURCE, &missing, ctx.now.as_second()).await?;

        // Do not leave an open session behind. A failure here does not change the result.
        let _ = ctx
            .client
            .post(format!("{pds}/xrpc/com.atproto.server.deleteSession"))
            .bearer_auth(&session.refresh_jwt)
            .send()
            .await;

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

    fn password(&self) -> Result<String, SourceError> {
        if let Some(password) = &self.password {
            return Ok(password.clone());
        }
        std::env::var(&self.app_password_env)
            .ok()
            .filter(|password| !password.trim().is_empty())
            .ok_or_else(|| SourceError::MissingSecret(self.app_password_env.clone()))
    }

    async fn login(
        &self,
        ctx: &SyncContext<'_>,
        pds: &str,
        password: &str,
    ) -> Result<Session, SourceError> {
        let url = format!("{pds}/xrpc/com.atproto.server.createSession");
        let request = ctx
            .client
            .post(&url)
            .json(&json!({ "identifier": self.identifier, "password": password }));
        let response = http::send_with_retry(request).await?;
        if !response.status().is_success() {
            return Err(api_error(&url, response).await);
        }
        Ok(response.json().await?)
    }

    async fn page(
        &self,
        ctx: &SyncContext<'_>,
        pds: &str,
        session: &Session,
        cursor: Option<&str>,
    ) -> Result<PageResult, SourceError> {
        let url = format!("{pds}/xrpc/app.bsky.bookmark.getBookmarks");
        let mut query = vec![("limit", PAGE_SIZE)];
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor));
        }
        let request = ctx
            .client
            .get(&url)
            .query(&query)
            .bearer_auth(&session.access_jwt);
        let response = http::send_with_retry(request).await?;

        let status = response.status();
        if status.is_success() {
            return Ok(PageResult::Page(response.json().await?));
        }
        // An expired access token: HTTP 400 `ExpiredToken` (ATProto), or HTTP 401.
        let error = api_error(&url, response).await;
        let expired = status == StatusCode::UNAUTHORIZED
            || matches!(&error, SourceError::Api { message, .. } if message.contains("xpired"));
        if expired {
            debug!(%status, "expired token");
            return Ok(PageResult::ExpiredToken);
        }
        Err(error)
    }

    /// Stores one bookmark. Returns its URI, or `None` if it has no subject.
    async fn store(
        &self,
        ctx: &SyncContext<'_>,
        bookmark: &Value,
        report: &mut SyncReport,
    ) -> Result<Option<String>, SourceError> {
        let Some(uri) = bookmark.pointer("/subject/uri").and_then(Value::as_str) else {
            warn!("bookmark without a subject: skipped");
            return Ok(None);
        };
        let created_at = bookmark.get("createdAt").and_then(Value::as_str);

        // A known bookmark keeps its first `first_seen`: the fallback time must not move.
        let first_seen = db::event_first_seen(ctx.db, SOURCE, uri)
            .await?
            .and_then(|seconds| jiff::Timestamp::from_second(seconds).ok())
            .unwrap_or(ctx.now);
        let event_time = time::event_time(
            created_at,
            None,
            first_seen,
            ctx.now,
            time::DEFAULT_OLDEST_EVENT_TIME,
        );
        for rejected in &event_time.rejected {
            warn!(%uri, "{rejected}");
        }

        let payload = serde_json::to_string(bookmark)?;
        let upsert = db::upsert_event(
            ctx.db,
            &NewEvent {
                source: SOURCE,
                kind: KIND,
                external_id: uri,
                occurred_at: event_time.at.as_second(),
                payload: &payload,
            },
            ctx.now.as_second(),
        )
        .await?;
        report.count(upsert);
        Ok(Some(uri.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use sqlx::SqlitePool;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, header, method, path, query_param, query_param_is_missing},
    };

    use super::*;

    const DID: &str = "did:plc:abc";
    const NOW: &str = "2026-11-12T12:00:00Z";
    const LIST: &str = "/xrpc/app.bsky.bookmark.getBookmarks";
    const LOGIN: &str = "/xrpc/com.atproto.server.createSession";

    fn bookmarks(server: &MockServer, enabled: bool) -> BskyBookmarks {
        let config: BskyBookmarksConfig = toml::from_str(&format!(
            "enabled = {enabled}\nidentifier = \"me.example\"\napp_password_env = \"BSKY_APP_PASSWORD\""
        ))
        .unwrap();
        BskyBookmarks::new(&config).with_test_setup(
            IdentityEndpoints {
                plc: server.uri(),
                handle_resolver: server.uri(),
                web_override: Some(server.uri()),
            },
            "app-pw",
        )
    }

    /// `me.example` -> `did:plc:abc`, on a PDS that is the mock server itself.
    async fn mount_account(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/.well-known/atproto-did"))
            .respond_with(ResponseTemplate::new(200).set_body_string(DID))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/{DID}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": DID,
                "alsoKnownAs": ["at://me.example"],
                "service": [{
                    "id": "#atproto_pds",
                    "type": "AtprotoPersonalDataServer",
                    "serviceEndpoint": server.uri()
                }]
            })))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/xrpc/com.atproto.server.deleteSession"))
            .respond_with(ResponseTemplate::new(200))
            .mount(server)
            .await;
    }

    /// Each login gives the tokens `access-<n>` and `refresh-<n>`, n = 1, 2...
    async fn mount_logins(server: &MockServer, count: u64) {
        for n in 1..=count {
            Mock::given(method("POST"))
                .and(path(LOGIN))
                .and(body_json(
                    json!({ "identifier": "me.example", "password": "app-pw" }),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "did": DID, "handle": "me.example",
                    "accessJwt": format!("access-{n}"), "refreshJwt": format!("refresh-{n}")
                })))
                .up_to_n_times(1)
                .expect(1)
                .mount(server)
                .await;
        }
    }

    fn bookmark(rkey: &str, created_at: Option<&str>) -> Value {
        let uri = format!("at://did:plc:author/app.bsky.feed.post/{rkey}");
        let mut view = json!({
            "subject": { "uri": uri, "cid": "bafy" },
            "item": { "$type": "app.bsky.feed.defs#postView", "uri": uri }
        });
        if let Some(created_at) = created_at {
            view["createdAt"] = json!(created_at);
        }
        view
    }

    async fn mount_page(
        server: &MockServer,
        token: &str,
        cursor: Option<&str>,
        items: Vec<Value>,
        next: Option<&str>,
    ) {
        let mock = Mock::given(method("GET"))
            .and(path(LIST))
            .and(query_param("limit", "100"))
            .and(header("authorization", format!("Bearer {token}").as_str()));
        let mock = match cursor {
            Some(cursor) => mock.and(query_param("cursor", cursor)),
            None => mock.and(query_param_is_missing("cursor")),
        };
        mock.respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "cursor": next, "bookmarks": items })),
        )
        .mount(server)
        .await;
    }

    async fn sync(adapter: &BskyBookmarks, db: &SqlitePool) -> Result<SyncReport, SourceError> {
        let client = http::client().unwrap();
        let ctx = SyncContext {
            db,
            client: &client,
            now: NOW.parse().unwrap(),
            full: false,
        };
        adapter.sync(&ctx).await
    }

    async fn events(db: &SqlitePool) -> Vec<(String, i64, Option<i64>)> {
        sqlx::query_as(
            "SELECT external_id, occurred_at, deleted_at FROM events
             WHERE source = 'bsky_bookmarks' ORDER BY external_id",
        )
        .fetch_all(db)
        .await
        .unwrap()
    }

    fn seconds(text: &str) -> i64 {
        text.parse::<Timestamp>().unwrap().as_second()
    }

    fn post_uri(rkey: &str) -> String {
        format!("at://did:plc:author/app.bsky.feed.post/{rkey}")
    }

    #[tokio::test]
    async fn bookmarks_of_all_pages_are_stored() {
        let server = MockServer::start().await;
        mount_account(&server).await;
        mount_logins(&server, 1).await;
        mount_page(
            &server,
            "access-1",
            None,
            vec![bookmark("3b", Some("2026-11-01T10:00:00Z"))],
            Some("page-2"),
        )
        .await;
        // Without `createdAt`: the time is `first_seen` (now).
        mount_page(
            &server,
            "access-1",
            Some("page-2"),
            vec![bookmark("3a", None)],
            None,
        )
        .await;
        let db = db::open_in_memory().await.unwrap();

        let report = sync(&bookmarks(&server, true), &db).await.unwrap();

        assert_eq!(report.inserted, 2);
        assert_eq!(
            events(&db).await,
            [
                (post_uri("3a"), seconds(NOW), None),
                (post_uri("3b"), seconds("2026-11-01T10:00:00Z"), None),
            ]
        );
    }

    #[tokio::test]
    async fn expired_token_logs_in_again_once() {
        let server = MockServer::start().await;
        mount_account(&server).await;
        mount_logins(&server, 2).await;
        Mock::given(method("GET"))
            .and(path(LIST))
            .and(header("authorization", "Bearer access-1"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "ExpiredToken", "message": "Token has expired"
            })))
            .expect(1)
            .mount(&server)
            .await;
        mount_page(&server, "access-2", None, vec![bookmark("3a", None)], None).await;
        let db = db::open_in_memory().await.unwrap();

        let report = sync(&bookmarks(&server, true), &db).await.unwrap();

        assert_eq!(report.inserted, 1);
    }

    #[tokio::test]
    async fn token_that_expires_again_is_an_error() {
        let server = MockServer::start().await;
        mount_account(&server).await;
        mount_logins(&server, 2).await;
        Mock::given(method("GET"))
            .and(path(LIST))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": "AuthenticationRequired", "message": "Bad token"
            })))
            .expect(2)
            .mount(&server)
            .await;
        let db = db::open_in_memory().await.unwrap();

        let error = sync(&bookmarks(&server, true), &db).await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("expired again after a new login"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn removed_bookmark_is_marked_deleted() {
        let server = MockServer::start().await;
        mount_account(&server).await;
        mount_logins(&server, 1).await;
        mount_page(&server, "access-1", None, vec![bookmark("3a", None)], None).await;
        let db = db::open_in_memory().await.unwrap();
        let removed = post_uri("3z");
        let event = NewEvent {
            source: SOURCE,
            kind: KIND,
            external_id: &removed,
            occurred_at: 0,
            payload: "{}",
        };
        db::upsert_event(&db, &event, 0).await.unwrap();

        let report = sync(&bookmarks(&server, true), &db).await.unwrap();

        assert_eq!(report.deleted, 1);
        assert_eq!(events(&db).await[1], (removed, 0, Some(seconds(NOW))));
    }

    #[tokio::test]
    async fn disabled_source_purges_its_rows_without_any_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let db = db::open_in_memory().await.unwrap();
        for rkey in ["3a", "3b"] {
            let uri = post_uri(rkey);
            let event = NewEvent {
                source: SOURCE,
                kind: KIND,
                external_id: &uri,
                occurred_at: 0,
                payload: "{}",
            };
            db::upsert_event(&db, &event, 0).await.unwrap();
        }

        let report = sync(&bookmarks(&server, false), &db).await.unwrap();

        assert_eq!(report.purged, 2);
        assert!(events(&db).await.is_empty());
    }

    #[tokio::test]
    async fn missing_password_stops_before_any_request() {
        let config: BskyBookmarksConfig = toml::from_str(
            "enabled = true\nidentifier = \"me.example\"\napp_password_env = \"HAIRBALL_TEST_UNSET_PW\"",
        )
        .unwrap();
        let db = db::open_in_memory().await.unwrap();

        let error = sync(&BskyBookmarks::new(&config), &db).await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "the environment variable `HAIRBALL_TEST_UNSET_PW` is not set or is empty"
        );
    }
}
