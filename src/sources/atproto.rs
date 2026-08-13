//! ATProto adapter: all the records of my repo, one event per record.
//!
//! Bluesky, Tangled, standard.site... are not separate sources: they are
//! collections (NSIDs) of the same repo, on the PDS of the account.

use jiff::Timestamp;
use reqwest::Client;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use tracing::{debug, info, warn};

use super::{SourceError, SyncContext, SyncReport};
use crate::{
    config::{AtprotoConfig, BlueskyToggles},
    db::{self, NewEvent, SyncState},
    identity::{self, IdentityEndpoints},
    time,
};

/// The source id of the adapter.
pub const SOURCE: &str = "atproto";

/// Records per `listRecords` page: the maximum of the API.
const PAGE_SIZE: &str = "100";

#[derive(Debug, Clone)]
pub struct Atproto {
    handle: String,
    exclude: Vec<String>,
    bluesky: BlueskyToggles,
    /// An older `createdAt` is rejected.
    oldest: Timestamp,
    endpoints: IdentityEndpoints,
}

#[derive(Deserialize)]
struct RepoDescription {
    collections: Vec<String>,
}

#[derive(Deserialize)]
struct RecordPage {
    cursor: Option<String>,
    records: Vec<Record>,
}

#[derive(Deserialize)]
struct Record {
    uri: String,
    value: Value,
}

#[derive(Deserialize)]
struct XrpcError {
    message: Option<String>,
}

impl Atproto {
    pub fn new(config: &AtprotoConfig) -> Self {
        Self {
            handle: config.handle.clone(),
            exclude: config.exclude.clone(),
            bluesky: config.bluesky.clone(),
            // `Config::parse` already refused a bad date: the default is never used then.
            oldest: config
                .oldest_time()
                .unwrap_or(time::DEFAULT_OLDEST_EVENT_TIME),
            endpoints: IdentityEndpoints::default(),
        }
    }

    /// Replaces the identity endpoints (PLC directory, handle resolver). For tests.
    pub fn with_endpoints(mut self, endpoints: IdentityEndpoints) -> Self {
        self.endpoints = endpoints;
        self
    }

    /// Whether the collection `nsid` is scraped: not excluded, and not disabled
    /// by a `[atproto.bluesky]` toggle.
    pub fn wants(&self, nsid: &str) -> bool {
        let excluded = self
            .exclude
            .iter()
            .any(|prefix| nsid == prefix || nsid.starts_with(&format!("{prefix}.")));
        let toggled_off = match nsid {
            "app.bsky.feed.post" => !self.bluesky.posts,
            "app.bsky.feed.like" => !self.bluesky.likes,
            "app.bsky.feed.repost" => !self.bluesky.reposts,
            _ => false,
        };
        !excluded && !toggled_off
    }

    pub async fn sync(&self, ctx: &SyncContext<'_>) -> Result<SyncReport, SourceError> {
        let account = identity::resolve(ctx.client, &self.endpoints, &self.handle).await?;
        info!(did = %account.did, pds = %account.pds, "resolved the account");

        let description: RepoDescription = xrpc_get(
            ctx.client,
            &format!("{}/xrpc/com.atproto.repo.describeRepo", account.pds),
            &[("repo", account.did.as_str())],
        )
        .await?;

        let mut report = SyncReport::default();
        for nsid in &description.collections {
            if !self.wants(nsid) {
                debug!(%nsid, "collection skipped by the config");
                continue;
            }
            let before = report;
            self.sync_collection(ctx, &account.pds, &account.did, nsid, &mut report)
                .await?;
            debug!(%nsid, inserted = report.inserted - before.inserted, "collection done");
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

    /// Reads the pages of one collection, and upserts each record.
    ///
    /// `listRecords` returns the records by record key, newest first (TIDs sort
    /// by time). Without `--full`, the read stops after a page where all the
    /// records are already known: the next pages are older.
    async fn sync_collection(
        &self,
        ctx: &SyncContext<'_>,
        pds: &str,
        did: &str,
        nsid: &str,
        report: &mut SyncReport,
    ) -> Result<(), SourceError> {
        let url = format!("{pds}/xrpc/com.atproto.repo.listRecords");
        let mut cursor: Option<String> = None;
        let mut pages = 0;

        loop {
            let mut query = vec![("repo", did), ("collection", nsid), ("limit", PAGE_SIZE)];
            if let Some(cursor) = &cursor {
                query.push(("cursor", cursor));
            }
            let page: RecordPage = xrpc_get(ctx.client, &url, &query).await?;
            pages += 1;

            let mut all_known = true;
            for record in &page.records {
                let upsert = upsert_record(ctx, nsid, record, self.oldest).await?;
                all_known &= upsert != db::Upsert::Inserted;
                report.count(upsert);
            }

            if !ctx.full && all_known && !page.records.is_empty() {
                debug!(%nsid, pages, "only known records: stop");
                return Ok(());
            }
            match page.cursor {
                Some(next) if !page.records.is_empty() => cursor = Some(next),
                _ => {
                    debug!(%nsid, pages, "last page");
                    return Ok(());
                }
            }
        }
    }
}

/// Stores one record as an event. The event time follows [`time::event_time`].
async fn upsert_record(
    ctx: &SyncContext<'_>,
    nsid: &str,
    record: &Record,
    oldest: Timestamp,
) -> Result<db::Upsert, SourceError> {
    let record_key = record.uri.rsplit('/').next();
    let created_at = record.value.get("createdAt").and_then(Value::as_str);

    // A known record keeps its first `first_seen`: the fallback time must not move.
    let first_seen = db::event_first_seen(ctx.db, SOURCE, &record.uri)
        .await?
        .and_then(|seconds| Timestamp::from_second(seconds).ok())
        .unwrap_or(ctx.now);

    let event_time = time::event_time(created_at, record_key, first_seen, ctx.now, oldest);
    for rejected in &event_time.rejected {
        warn!(uri = %record.uri, "{rejected}");
    }

    let payload = serde_json::to_string(&record.value)?;
    Ok(db::upsert_event(
        ctx.db,
        &NewEvent {
            source: SOURCE,
            kind: nsid,
            external_id: &record.uri,
            occurred_at: event_time.at.as_second(),
            payload: &payload,
        },
        ctx.now.as_second(),
    )
    .await?)
}

/// Sends an XRPC query and decodes the JSON answer.
async fn xrpc_get<T: DeserializeOwned>(
    client: &Client,
    url: &str,
    query: &[(&str, &str)],
) -> Result<T, SourceError> {
    let response = client.get(url).query(query).send().await?;
    let status = response.status();
    if !status.is_success() {
        let message = response
            .json::<XrpcError>()
            .await
            .ok()
            .and_then(|error| error.message)
            .unwrap_or_else(|| "no message".to_owned());
        return Err(SourceError::Api {
            url: url.to_owned(),
            status,
            message,
        });
    }
    Ok(response.json().await?)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::SqlitePool;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param, query_param_is_missing},
    };

    use super::*;
    use crate::http;

    const DID: &str = "did:plc:abc";
    const LIST: &str = "/xrpc/com.atproto.repo.listRecords";

    fn uri(nsid: &str, rkey: &str) -> String {
        format!("at://{DID}/{nsid}/{rkey}")
    }

    fn record(nsid: &str, rkey: &str, value: Value) -> Value {
        json!({ "uri": uri(nsid, rkey), "cid": "bafy", "value": value })
    }

    fn adapter(server: &MockServer, extra_config: &str) -> Atproto {
        let config: AtprotoConfig =
            toml::from_str(&format!("handle = \"me.example\"\n{extra_config}")).unwrap();
        Atproto::new(&config).with_endpoints(IdentityEndpoints {
            plc: server.uri(),
            handle_resolver: server.uri(),
            web_override: Some(server.uri()),
        })
    }

    /// `me.example` -> `did:plc:abc`, on a PDS that is the mock server itself.
    async fn mount_account(server: &MockServer, collections: &[&str]) {
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
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.describeRepo"))
            .and(query_param("repo", DID))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "did": DID, "handle": "me.example", "collections": collections
            })))
            .mount(server)
            .await;
    }

    async fn mount_page(
        server: &MockServer,
        nsid: &str,
        cursor: Option<&str>,
        records: Vec<Value>,
        next: Option<&str>,
    ) {
        let mock = Mock::given(method("GET"))
            .and(path(LIST))
            .and(query_param("repo", DID))
            .and(query_param("collection", nsid))
            .and(query_param("limit", "100"));
        let mock = match cursor {
            Some(cursor) => mock.and(query_param("cursor", cursor)),
            None => mock.and(query_param_is_missing("cursor")),
        };
        mock.respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "cursor": next, "records": records })),
        )
        .mount(server)
        .await;
    }

    /// An incremental sync (no `--full`).
    async fn sync_at(
        adapter: &Atproto,
        db: &SqlitePool,
        now: &str,
    ) -> Result<SyncReport, SourceError> {
        sync_with(adapter, db, now, false).await
    }

    async fn sync_with(
        adapter: &Atproto,
        db: &SqlitePool,
        now: &str,
        full: bool,
    ) -> Result<SyncReport, SourceError> {
        let client = http::client().unwrap();
        let ctx = SyncContext {
            db,
            client: &client,
            now: now.parse().unwrap(),
            full,
        };
        adapter.sync(&ctx).await
    }

    async fn events(db: &SqlitePool) -> Vec<(String, String, i64)> {
        sqlx::query_as("SELECT kind, external_id, occurred_at FROM events ORDER BY external_id")
            .fetch_all(db)
            .await
            .unwrap()
    }

    fn seconds(text: &str) -> i64 {
        text.parse::<Timestamp>().unwrap().as_second()
    }

    #[test]
    fn exclude_and_toggles_select_the_collections() {
        let server_less = Atproto::new(
            &toml::from_str(
                "handle = \"a\"\nexclude = [\"app.bsky.graph\", \"fm.teal.alpha.feed.play\"]\n\
                 [bluesky]\nlikes = false",
            )
            .unwrap(),
        );

        assert!(server_less.wants("app.bsky.feed.post"));
        assert!(server_less.wants("app.bsky.feed.repost"));
        assert!(!server_less.wants("app.bsky.feed.like"));
        assert!(!server_less.wants("app.bsky.graph.follow"));
        assert!(!server_less.wants("fm.teal.alpha.feed.play"));
        // A prefix stops at a dot: `app.bsky.graphx` is another authority.
        assert!(server_less.wants("app.bsky.graphx.thing"));
        assert!(server_less.wants("sh.tangled.repo"));
    }

    #[tokio::test]
    async fn sync_reads_all_pages_of_the_wanted_collections() {
        let server = MockServer::start().await;
        mount_account(
            &server,
            &[
                "app.bsky.feed.post",
                "app.bsky.feed.like",
                "app.bsky.graph.follow",
                "sh.tangled.repo",
            ],
        )
        .await;
        let post = "app.bsky.feed.post";
        mount_page(
            &server,
            post,
            None,
            vec![
                record(
                    post,
                    "3mvgdyaeyt22l",
                    json!({ "text": "c", "createdAt": "2026-09-13T19:40:12.208Z" }),
                ),
                record(
                    post,
                    "3mu2lu2t2xi2o",
                    json!({ "text": "b", "createdAt": "2026-08-27T10:03:53.445Z" }),
                ),
            ],
            Some("page-2"),
        )
        .await;
        mount_page(
            &server,
            post,
            Some("page-2"),
            vec![record(
                post,
                "3jzfcijpj2z2a",
                json!({ "text": "a", "createdAt": "2023-06-30T15:03:01.887Z" }),
            )],
            None,
        )
        .await;
        // No `createdAt`: the time comes from the TID of the record key.
        mount_page(
            &server,
            "sh.tangled.repo",
            None,
            vec![record(
                "sh.tangled.repo",
                "3jzfcijpj2z2a",
                json!({ "name": "hairball" }),
            )],
            None,
        )
        .await;
        // Disabled by the config: never requested.
        for nsid in ["app.bsky.feed.like", "app.bsky.graph.follow"] {
            Mock::given(method("GET"))
                .and(path(LIST))
                .and(query_param("collection", nsid))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&server)
                .await;
        }

        let db = db::open_in_memory().await.unwrap();
        let adapter = adapter(
            &server,
            "exclude = [\"app.bsky.graph\"]\n[bluesky]\nlikes = false",
        );
        let report = sync_at(&adapter, &db, "2026-11-12T12:00:00Z")
            .await
            .unwrap();

        assert_eq!((report.inserted, report.unchanged), (4, 0));
        assert_eq!(
            events(&db).await,
            [
                (
                    post.to_owned(),
                    uri(post, "3jzfcijpj2z2a"),
                    seconds("2023-06-30T15:03:01Z")
                ),
                (
                    post.to_owned(),
                    uri(post, "3mu2lu2t2xi2o"),
                    seconds("2026-08-27T10:03:53Z")
                ),
                (
                    post.to_owned(),
                    uri(post, "3mvgdyaeyt22l"),
                    seconds("2026-09-13T19:40:12Z")
                ),
                (
                    "sh.tangled.repo".to_owned(),
                    uri("sh.tangled.repo", "3jzfcijpj2z2a"),
                    seconds("2023-06-30T15:03:01Z"),
                ),
            ]
        );
        let payload: String = sqlx::query_scalar("SELECT payload FROM events WHERE kind = ?1")
            .bind("sh.tangled.repo")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(payload, r#"{"name":"hairball"}"#);
        assert_eq!(
            db::sync_state(&db, SOURCE).await.unwrap().last_run,
            Some(seconds("2026-11-12T12:00:00Z"))
        );

        // A second full run reads everything again, and changes nothing.
        let report = sync_with(&adapter, &db, "2026-11-12T13:00:00Z", true)
            .await
            .unwrap();
        assert_eq!(
            (report.inserted, report.updated, report.unchanged),
            (0, 0, 4)
        );
    }

    #[tokio::test]
    async fn record_without_any_time_keeps_its_first_seen() {
        let server = MockServer::start().await;
        let profile = "app.bsky.actor.profile";
        mount_account(&server, &[profile]).await;
        // `self` is not a TID, and the profile has no `createdAt`.
        mount_page(
            &server,
            profile,
            None,
            vec![record(profile, "self", json!({ "displayName": "Me" }))],
            None,
        )
        .await;
        let db = db::open_in_memory().await.unwrap();
        let adapter = adapter(&server, "");

        sync_at(&adapter, &db, "2026-11-01T08:00:00Z")
            .await
            .unwrap();
        let report = sync_at(&adapter, &db, "2026-11-12T12:00:00Z")
            .await
            .unwrap();

        assert_eq!(report.unchanged, 1);
        assert_eq!(events(&db).await[0].2, seconds("2026-11-01T08:00:00Z"));
    }

    #[tokio::test]
    async fn pds_error_stops_the_sync_with_its_message() {
        let server = MockServer::start().await;
        mount_account(&server, &["app.bsky.feed.post"]).await;
        Mock::given(method("GET"))
            .and(path(LIST))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "InvalidRequest", "message": "Could not find repo"
            })))
            .mount(&server)
            .await;
        let db = db::open_in_memory().await.unwrap();

        let error = sync_at(&adapter(&server, ""), &db, "2026-11-12T12:00:00Z")
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .ends_with("answered HTTP 400 Bad Request: Could not find repo"),
            "{error}"
        );
        // A failed run does not store a last run.
        assert_eq!(db::sync_state(&db, SOURCE).await.unwrap().last_run, None);
    }

    const POST: &str = "app.bsky.feed.post";

    /// 2 pages of posts, newest first. The second page must be requested
    /// `second_page_reads` times during the test.
    async fn mount_two_pages(server: &MockServer, second_page_reads: u64) {
        mount_account(server, &[POST]).await;
        mount_page(
            server,
            POST,
            None,
            vec![
                record(
                    POST,
                    "3mvgdyaeyt22l",
                    json!({ "createdAt": "2026-09-13T19:40:12Z" }),
                ),
                record(
                    POST,
                    "3mu2lu2t2xi2o",
                    json!({ "createdAt": "2026-08-27T10:03:53Z" }),
                ),
            ],
            Some("page-2"),
        )
        .await;
        Mock::given(method("GET"))
            .and(path(LIST))
            .and(query_param("collection", POST))
            .and(query_param("cursor", "page-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "records": [record(POST, "3jzfcijpj2z2a", json!({ "createdAt": "2023-06-30T15:03:01Z" }))]
            })))
            .expect(second_page_reads)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn incremental_sync_stops_at_a_page_of_known_records() {
        let server = MockServer::start().await;
        // Read by the first sync only.
        mount_two_pages(&server, 1).await;
        let db = db::open_in_memory().await.unwrap();
        let adapter = adapter(&server, "");

        let first = sync_at(&adapter, &db, "2026-11-12T12:00:00Z")
            .await
            .unwrap();
        let second = sync_at(&adapter, &db, "2026-11-12T13:00:00Z")
            .await
            .unwrap();

        assert_eq!(first.inserted, 3);
        // Only the first page: its 2 records are known.
        assert_eq!((second.inserted, second.unchanged), (0, 2));
    }

    #[tokio::test]
    async fn full_sync_reads_all_pages_again() {
        let server = MockServer::start().await;
        mount_two_pages(&server, 2).await;
        let db = db::open_in_memory().await.unwrap();
        let adapter = adapter(&server, "");

        sync_at(&adapter, &db, "2026-11-12T12:00:00Z")
            .await
            .unwrap();
        let full = sync_with(&adapter, &db, "2026-11-12T13:00:00Z", true)
            .await
            .unwrap();

        assert_eq!((full.inserted, full.unchanged), (0, 3));
    }

    #[tokio::test]
    async fn new_record_on_the_first_page_continues_to_the_next_page() {
        let server = MockServer::start().await;
        mount_two_pages(&server, 1).await;
        let db = db::open_in_memory().await.unwrap();
        // The database knows all the posts, except the newest one.
        for rkey in ["3mu2lu2t2xi2o", "3jzfcijpj2z2a"] {
            let uri = uri(POST, rkey);
            let event = NewEvent {
                source: SOURCE,
                kind: POST,
                external_id: &uri,
                occurred_at: 0,
                payload: "{}",
            };
            db::upsert_event(&db, &event, 0).await.unwrap();
        }

        let report = sync_at(&adapter(&server, ""), &db, "2026-11-12T12:00:00Z")
            .await
            .unwrap();

        // Page 1 has a new record: page 2 is read too.
        assert_eq!(report.inserted, 1);
        assert_eq!(report.inserted + report.updated + report.unchanged, 3);
    }
}
