//! Last.fm adapter: one event per scrobble, with the `lastfm-client` crate.

use std::sync::Arc;

use lastfm_client::{ConfigBuilder, LastFmClient, client::HttpClient, types::RecentTrack};
use tracing::debug;

use super::{SourceError, SyncContext, SyncReport};
use crate::{
    config::LastfmConfig,
    db::{self, NewEvent, SyncState},
    http,
};

/// The source id of the adapter.
pub const SOURCE: &str = "lastfm";

/// The kind of every Last.fm event.
pub const KIND: &str = "scrobble";

#[derive(Debug, Clone)]
pub struct Lastfm {
    user: String,
    api_key_env: String,
    /// Replaces the API key and the HTTP client. For tests.
    test_client: Option<(String, Arc<dyn HttpClient>)>,
}

impl Lastfm {
    pub fn new(config: &LastfmConfig) -> Self {
        Self {
            user: config.user.clone(),
            api_key_env: config.api_key_env.clone(),
            test_client: None,
        }
    }

    /// Replaces the API key and the HTTP client. For tests.
    pub fn with_http(mut self, api_key: &str, http: Arc<dyn HttpClient>) -> Self {
        self.test_client = Some((api_key.to_owned(), http));
        self
    }

    /// Reads the scrobbles since the cursor (the highest `uts` already stored),
    /// or all of them on a full run.
    pub async fn sync(&self, ctx: &SyncContext<'_>) -> Result<SyncReport, SourceError> {
        let client = self.client()?;
        let state = db::sync_state(ctx.db, SOURCE).await?;
        let cursor = state.cursor.as_deref().and_then(|c| c.parse::<i64>().ok());

        let mut request = client.recent_tracks(self.user.as_str());
        if let (Some(cursor), false) = (cursor, ctx.full) {
            // `from` includes the cursor: the last stored scrobble comes back,
            // unchanged. It is safer than a gap.
            request = request.since(cursor);
        }
        // The crate reports `(fetched, total)` after each batch of scrobbles.
        let progress = Arc::clone(ctx.progress);
        progress.set_message("scrobbles".to_owned());
        let request = request.on_progress(move |fetched, total| {
            progress.set_length(Some(u64::from(total)));
            progress.set_position(u64::from(fetched));
        });
        let tracks: Vec<RecentTrack> = request.fetch().await?.into();
        debug!(tracks = tracks.len(), ?cursor, "recent tracks read");

        let mut report = SyncReport::default();
        let mut highest = cursor;
        for track in &tracks {
            // The track that plays now has no date: it is not a scrobble yet.
            let Some(date) = &track.date else {
                debug!("now playing: skipped");
                continue;
            };
            let uts = i64::from(date.uts);
            let artist = track.artist.text.as_deref().unwrap_or_default();
            let name = track.name.as_deref().unwrap_or_default();
            // Last.fm scrobbles have no stable id.
            let external_id = format!("{uts}:{artist}:{name}");
            let payload = serde_json::to_string(track)?;

            let upsert = db::upsert_event(
                ctx.db,
                &NewEvent {
                    source: SOURCE,
                    kind: KIND,
                    external_id: &external_id,
                    occurred_at: uts,
                    payload: &payload,
                },
                ctx.now.as_second(),
            )
            .await?;
            report.count(upsert);
            highest = highest.max(Some(uts));
        }

        db::set_sync_state(
            ctx.db,
            SOURCE,
            &SyncState {
                cursor: highest.map(|uts| uts.to_string()),
                last_run: Some(ctx.now.as_second()),
            },
        )
        .await?;
        Ok(report)
    }

    fn client(&self) -> Result<LastFmClient, SourceError> {
        let (api_key, test_http) = match &self.test_client {
            Some((key, http)) => (key.clone(), Some(Arc::clone(http))),
            None => (
                std::env::var(&self.api_key_env)
                    .ok()
                    .filter(|key| !key.trim().is_empty())
                    .ok_or_else(|| SourceError::MissingSecret(self.api_key_env.clone()))?,
                None,
            ),
        };
        let config = ConfigBuilder::new()
            .api_key(api_key)
            .user_agent(http::USER_AGENT)
            .timeout(http::TIMEOUT)
            .build()?;
        Ok(match test_http {
            Some(http) => LastFmClient::with_http(config, http),
            None => LastFmClient::from_config(config),
        })
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use sqlx::SqlitePool;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param, query_param_is_missing},
    };

    use super::*;

    /// Sends the requests of `lastfm-client` to the mock server instead of Last.fm.
    #[derive(Debug)]
    struct ToMockServer {
        base: String,
        client: reqwest::Client,
    }

    #[async_trait]
    impl HttpClient for ToMockServer {
        async fn get(&self, url: &str) -> lastfm_client::Result<Value> {
            let url = url.replacen("https://ws.audioscrobbler.com", &self.base, 1);
            // `lastfm-client` uses another major version of reqwest: wrap the error.
            let wrap = |e: reqwest::Error| lastfm_client::LastFmError::Io(std::io::Error::other(e));
            let response = self.client.get(url).send().await.map_err(wrap)?;
            response.json().await.map_err(wrap)
        }
    }

    fn lastfm(server: &MockServer) -> Lastfm {
        let config: LastfmConfig =
            toml::from_str("user = \"me\"\napi_key_env = \"LASTFM_API_KEY\"").unwrap();
        let http = ToMockServer {
            base: server.uri(),
            client: reqwest::Client::new(),
        };
        Lastfm::new(&config).with_http("key", Arc::new(http))
    }

    fn scrobble(uts: u32, artist: &str, name: &str) -> Value {
        json!({
            "artist": { "mbid": "", "#text": artist },
            "streamable": "0",
            "image": [],
            "mbid": "",
            "album": { "mbid": "", "#text": "Album" },
            "name": name,
            "url": "https://www.last.fm/music/x",
            "date": { "uts": uts.to_string(), "#text": "01 Sep 2026, 10:00" }
        })
    }

    fn now_playing(artist: &str, name: &str) -> Value {
        json!({
            "artist": { "mbid": "", "#text": artist },
            "streamable": "0",
            "image": [],
            "mbid": "",
            "album": { "mbid": "", "#text": "Album" },
            "name": name,
            "url": "https://www.last.fm/music/x",
            "@attr": { "nowplaying": "true" }
        })
    }

    fn page(tracks: Vec<Value>, total: u32) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "recenttracks": {
                "track": tracks,
                "@attr": {
                    "user": "me", "page": "1", "perPage": "1000",
                    "totalPages": "1", "total": total.to_string()
                }
            }
        }))
    }

    /// Mounts the 2 requests of the crate: `limit=1` (to read the total), then
    /// the page with all the tracks. `from` is the expected cursor.
    async fn mount_recent(server: &MockServer, from: Option<&str>, tracks: Vec<Value>, total: u32) {
        for limit in ["1".to_owned(), total.to_string()] {
            let mock = Mock::given(method("GET"))
                .and(path("/2.0/"))
                .and(query_param("method", "user.getrecenttracks"))
                .and(query_param("user", "me"))
                .and(query_param("api_key", "key"))
                .and(query_param("limit", limit.as_str()));
            let mock = match from {
                Some(from) => mock.and(query_param("from", from)),
                None => mock.and(query_param_is_missing("from")),
            };
            mock.respond_with(page(tracks.clone(), total))
                .mount(server)
                .await;
        }
    }

    async fn sync(lastfm: &Lastfm, db: &SqlitePool, full: bool) -> Result<SyncReport, SourceError> {
        let client = http::client().unwrap();
        let progress = crate::progress::none();
        let ctx = SyncContext {
            db,
            client: &client,
            now: "2026-11-12T12:00:00Z".parse().unwrap(),
            full,
            progress: &progress,
        };
        lastfm.sync(&ctx).await
    }

    async fn events(db: &SqlitePool) -> Vec<(String, i64)> {
        sqlx::query_as("SELECT external_id, occurred_at FROM events WHERE source = 'lastfm' ORDER BY occurred_at")
            .fetch_all(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn scrobbles_are_stored_with_their_time() {
        let server = MockServer::start().await;
        mount_recent(
            &server,
            None,
            vec![
                scrobble(1_788_000_200, "B", "Two"),
                scrobble(1_788_000_000, "A", "One"),
            ],
            2,
        )
        .await;
        let db = db::open_in_memory().await.unwrap();

        let report = sync(&lastfm(&server), &db, false).await.unwrap();

        assert_eq!(report.inserted, 2);
        assert_eq!(
            events(&db).await,
            [
                ("1788000000:A:One".to_owned(), 1_788_000_000),
                ("1788000200:B:Two".to_owned(), 1_788_000_200),
            ]
        );
        assert_eq!(
            db::sync_state(&db, SOURCE).await.unwrap().cursor.as_deref(),
            Some("1788000200")
        );
    }

    #[tokio::test]
    async fn next_run_starts_at_the_cursor() {
        let server = MockServer::start().await;
        mount_recent(&server, None, vec![scrobble(1_788_000_000, "A", "One")], 1).await;
        mount_recent(
            &server,
            Some("1788000000"),
            vec![
                scrobble(1_788_000_300, "C", "Three"),
                scrobble(1_788_000_000, "A", "One"),
            ],
            2,
        )
        .await;
        let db = db::open_in_memory().await.unwrap();
        let lastfm = lastfm(&server);

        sync(&lastfm, &db, false).await.unwrap();
        let second = sync(&lastfm, &db, false).await.unwrap();

        // The scrobble at the cursor comes back unchanged.
        assert_eq!((second.inserted, second.unchanged), (1, 1));
        assert_eq!(
            db::sync_state(&db, SOURCE).await.unwrap().cursor.as_deref(),
            Some("1788000300")
        );
    }

    #[tokio::test]
    async fn now_playing_track_is_skipped() {
        let server = MockServer::start().await;
        // Last.fm puts the track that plays now first, without a date, on top
        // of the `limit` scrobbles; `total` does not count it.
        mount_recent(
            &server,
            None,
            vec![
                now_playing("C", "Three"),
                scrobble(1_788_000_200, "B", "Two"),
                scrobble(1_788_000_000, "A", "One"),
            ],
            2,
        )
        .await;
        let db = db::open_in_memory().await.unwrap();

        sync(&lastfm(&server), &db, false).await.unwrap();

        let ids: Vec<String> = events(&db).await.into_iter().map(|(id, _)| id).collect();
        assert!(!ids.iter().any(|id| id.contains("Three")), "{ids:?}");
        assert_eq!(ids, ["1788000000:A:One", "1788000200:B:Two"]);
    }

    #[tokio::test]
    async fn missing_api_key_stops_before_any_request() {
        let config: LastfmConfig =
            toml::from_str("user = \"me\"\napi_key_env = \"HAIRBALL_TEST_UNSET_KEY\"").unwrap();
        let db = db::open_in_memory().await.unwrap();

        let error = sync(&Lastfm::new(&config), &db, false).await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "the environment variable `HAIRBALL_TEST_UNSET_KEY` is not set or is empty"
        );
    }
}
