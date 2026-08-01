//! Checks that the credentials of the sources are valid and work.
//!
//! Each check sends one small read-only request with the secret, then reports
//! what the API answered. No data goes into the database.

use std::{error::Error, fmt};

use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    config::Config,
    identity::{self, IdentityEndpoints, IdentityError},
};

/// Base URLs of the APIs. Tests replace them with a mock server.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub github: String,
    pub lastfm: String,
    /// For ATProto: the PDS URL comes from the identity of the account.
    pub identity: IdentityEndpoints,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            github: "https://api.github.com".to_owned(),
            lastfm: "https://ws.audioscrobbler.com".to_owned(),
            identity: IdentityEndpoints::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Failed,
    Skipped,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ok => "ok",
            Self::Failed => "FAILED",
            Self::Skipped => "skipped",
        })
    }
}

/// The result of the check of one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub source: &'static str,
    pub status: Status,
    pub detail: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    #[error("the environment variable `{0}` is not set or is empty")]
    MissingSecret(String),

    #[error("the request failed")]
    Http(#[from] reqwest::Error),

    #[error("cannot find the PDS of the account")]
    Identity(#[from] IdentityError),

    /// The API answered, but refused the credential or the request.
    #[error("{0}")]
    Rejected(String),
}

/// Checks the credential of each configured source that has one, and the
/// ATProto identity (handle and PDS), which needs no secret.
///
/// `env` returns the value of an environment variable, or `None` if it is not set.
pub async fn run(
    config: &Config,
    env: impl Fn(&str) -> Option<String>,
    client: &Client,
    endpoints: &Endpoints,
) -> Vec<Check> {
    let mut checks = Vec::new();

    if let Some(atproto) = &config.atproto {
        let result = check_atproto(client, &endpoints.identity, &atproto.handle).await;
        checks.push(to_check("atproto", result));
    }

    if let Some(github) = &config.github {
        let result = match env(&github.token_env) {
            Some(token) => check_github(client, &endpoints.github, &github.user, &token).await,
            None => Err(CheckError::MissingSecret(github.token_env.clone())),
        };
        checks.push(to_check("github", result));
    }

    if let Some(lastfm) = &config.lastfm {
        let result = match env(&lastfm.api_key_env) {
            Some(key) => check_lastfm(client, &endpoints.lastfm, &lastfm.user, &key).await,
            None => Err(CheckError::MissingSecret(lastfm.api_key_env.clone())),
        };
        checks.push(to_check("lastfm", result));
    }

    if let Some(bookmarks) = &config.bsky_bookmarks {
        if bookmarks.enabled {
            let result = match env(&bookmarks.app_password_env) {
                Some(password) => {
                    check_bluesky(
                        client,
                        &endpoints.identity,
                        &bookmarks.identifier,
                        &password,
                    )
                    .await
                }
                None => Err(CheckError::MissingSecret(
                    bookmarks.app_password_env.clone(),
                )),
            };
            checks.push(to_check("bsky_bookmarks", result));
        } else {
            checks.push(Check {
                source: "bsky_bookmarks",
                status: Status::Skipped,
                detail: "disabled".to_owned(),
            });
        }
    }

    checks
}

fn to_check(source: &'static str, result: Result<String, CheckError>) -> Check {
    match result {
        Ok(detail) => Check {
            source,
            status: Status::Ok,
            detail,
        },
        Err(error) => Check {
            source,
            status: Status::Failed,
            detail: error_chain(&error),
        },
    }
}

/// Joins an error and all its sources: `a: b: c`.
fn error_chain(error: &dyn Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[derive(Deserialize)]
struct GraphqlResponse {
    data: Option<GraphqlData>,
    #[serde(default)]
    errors: Vec<GraphqlError>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlData {
    viewer: Viewer,
    rate_limit: RateLimit,
}

#[derive(Deserialize)]
struct Viewer {
    login: String,
}

#[derive(Deserialize)]
struct RateLimit {
    remaining: i64,
}

#[derive(Deserialize)]
struct GraphqlError {
    message: String,
}

/// Asks GitHub who owns the token. Warns if the token has more rights than needed.
pub async fn check_github(
    client: &Client,
    base: &str,
    user: &str,
    token: &str,
) -> Result<String, CheckError> {
    let response = client
        .post(format!("{base}/graphql"))
        .header("Authorization", format!("bearer {token}"))
        .json(&json!({ "query": "query { viewer { login } rateLimit { remaining } }" }))
        .send()
        .await?;

    let status = response.status();
    if status == StatusCode::UNAUTHORIZED {
        return Err(CheckError::Rejected(
            "GitHub refused the token (HTTP 401): it is wrong, expired or revoked".to_owned(),
        ));
    }
    if !status.is_success() {
        return Err(CheckError::Rejected(format!(
            "GitHub answered HTTP {status}"
        )));
    }

    let header = |name| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    // Only classic tokens send their scopes. A fine-grained token sends no header.
    let scopes = header("x-oauth-scopes").filter(|scopes| !scopes.trim().is_empty());
    let expiration = header("github-authentication-token-expiration");

    let body: GraphqlResponse = response.json().await?;
    if let Some(error) = body.errors.first() {
        return Err(CheckError::Rejected(format!(
            "GitHub returned an error: {}",
            error.message
        )));
    }
    let data = body
        .data
        .ok_or_else(|| CheckError::Rejected("GitHub returned no data".to_owned()))?;

    let mut detail = format!(
        "token of `{}`, {} requests left this hour",
        data.viewer.login, data.rate_limit.remaining
    );
    match expiration {
        Some(date) => detail.push_str(&format!(", expires {date}")),
        None => detail.push_str(", no expiration"),
    }
    if !data.viewer.login.eq_ignore_ascii_case(user) {
        detail.push_str(&format!(
            "; warning: the config user is `{user}`, not the token owner"
        ));
    }
    if let Some(scopes) = scopes {
        detail.push_str(&format!(
            "; warning: the token has the scopes `{scopes}`, but needs none"
        ));
    }
    Ok(detail)
}

/// Reads the last scrobble of the user with the API key.
pub async fn check_lastfm(
    client: &Client,
    base: &str,
    user: &str,
    api_key: &str,
) -> Result<String, CheckError> {
    let response = client
        .get(format!("{base}/2.0/"))
        .query(&[
            ("method", "user.getrecenttracks"),
            ("user", user),
            ("api_key", api_key),
            ("format", "json"),
            ("limit", "1"),
        ])
        .send()
        .await?;

    // Last.fm sends errors as JSON, with or without an HTTP error status.
    let status = response.status();
    let body: Value = response.json().await?;
    if let Some(code) = body.get("error").and_then(Value::as_i64) {
        let message = body
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("no message");
        let reason = match code {
            10 => "invalid API key".to_owned(),
            6 => format!("user `{user}` not found"),
            17 => {
                "the scrobbles are private (\"Hide recent listening information\" is on)".to_owned()
            }
            _ => message.to_owned(),
        };
        return Err(CheckError::Rejected(format!(
            "Last.fm refused the request: {reason} (error {code})"
        )));
    }
    if !status.is_success() {
        return Err(CheckError::Rejected(format!(
            "Last.fm answered HTTP {status}"
        )));
    }

    let total = body
        .pointer("/recenttracks/@attr/total")
        .and_then(Value::as_str)
        .ok_or_else(|| CheckError::Rejected("Last.fm returned no scrobble count".to_owned()))?;
    Ok(format!("API key works, `{user}` has {total} scrobbles"))
}

#[derive(Deserialize)]
struct Session {
    did: String,
    handle: String,
    #[serde(rename = "refreshJwt")]
    refresh_jwt: String,
}

#[derive(Deserialize)]
struct XrpcError {
    message: Option<String>,
}

#[derive(Deserialize)]
struct RepoDescription {
    collections: Vec<String>,
}

/// Finds the PDS of the account, then lists the collections of its repo.
/// There is no secret: this checks that the handle and the PDS work.
pub async fn check_atproto(
    client: &Client,
    endpoints: &IdentityEndpoints,
    handle: &str,
) -> Result<String, CheckError> {
    let account = identity::resolve(client, endpoints, handle).await?;

    let response = client
        .get(format!(
            "{}/xrpc/com.atproto.repo.describeRepo",
            account.pds
        ))
        .query(&[("repo", account.did.as_str())])
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        return Err(CheckError::Rejected(format!(
            "the PDS {} answered HTTP {status}: {}",
            account.pds,
            xrpc_message(response).await
        )));
    }
    let repo: RepoDescription = response.json().await?;

    Ok(format!(
        "{} on the PDS {}, {} collections",
        account.did,
        account.pds,
        repo.collections.len()
    ))
}

/// Finds the PDS of the account, logs in there with the app password,
/// then closes the session at once.
pub async fn check_bluesky(
    client: &Client,
    endpoints: &IdentityEndpoints,
    identifier: &str,
    password: &str,
) -> Result<String, CheckError> {
    // Each account logs in on its own PDS: bsky.social, Eurosky, a self-hosted server...
    let pds = identity::resolve(client, endpoints, identifier).await?.pds;

    let response = client
        .post(format!("{pds}/xrpc/com.atproto.server.createSession"))
        .json(&json!({ "identifier": identifier, "password": password }))
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        return Err(CheckError::Rejected(format!(
            "the PDS {pds} refused the login (HTTP {status}): {}",
            xrpc_message(response).await
        )));
    }
    let session: Session = response.json().await?;

    // Do not leave an open session behind. A failure here does not change the result.
    let _ = client
        .post(format!("{pds}/xrpc/com.atproto.server.deleteSession"))
        .bearer_auth(&session.refresh_jwt)
        .send()
        .await;

    Ok(format!(
        "app password works for `{}` ({}) on the PDS {pds}",
        session.handle, session.did
    ))
}

/// The `message` of an XRPC error body, if any.
async fn xrpc_message(response: reqwest::Response) -> String {
    response
        .json::<XrpcError>()
        .await
        .ok()
        .and_then(|error| error.message)
        .unwrap_or_else(|| "no message".to_owned())
}

#[cfg(test)]
mod tests {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, header, method, path, query_param},
    };

    use super::*;
    use crate::http;

    fn endpoints(server: &MockServer) -> Endpoints {
        Endpoints {
            github: server.uri(),
            lastfm: server.uri(),
            identity: identity_endpoints(server),
        }
    }

    fn identity_endpoints(server: &MockServer) -> IdentityEndpoints {
        IdentityEndpoints {
            plc: server.uri(),
            handle_resolver: server.uri(),
            web_override: Some(server.uri()),
        }
    }

    /// `handle` resolves to `did:plc:abc`, whose PDS is the mock server itself
    /// (like an account on a custom PDS: Eurosky, self-hosted...).
    async fn mount_identity(server: &MockServer, handle: &str) {
        Mock::given(method("GET"))
            .and(path("/.well-known/atproto-did"))
            .respond_with(ResponseTemplate::new(200).set_body_string("did:plc:abc"))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/did:plc:abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "did:plc:abc",
                "alsoKnownAs": [format!("at://{handle}")],
                "service": [{
                    "id": "#atproto_pds",
                    "type": "AtprotoPersonalDataServer",
                    "serviceEndpoint": server.uri()
                }]
            })))
            .mount(server)
            .await;
    }

    fn github_ok() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "data": { "viewer": { "login": "TomPlanche" }, "rateLimit": { "remaining": 4999 } }
        }))
    }

    #[tokio::test]
    async fn github_fine_grained_token_is_valid() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(header("authorization", "bearer secret"))
            .and(header("user-agent", http::USER_AGENT))
            .respond_with(github_ok().insert_header(
                "github-authentication-token-expiration",
                "2027-07-30 12:00:00 UTC",
            ))
            .expect(1)
            .mount(&server)
            .await;

        let detail = check_github(
            &http::client().unwrap(),
            &server.uri(),
            "tomplanche",
            "secret",
        )
        .await
        .unwrap();

        assert_eq!(
            detail,
            "token of `TomPlanche`, 4999 requests left this hour, expires 2027-07-30 12:00:00 UTC"
        );
    }

    #[tokio::test]
    async fn github_warns_about_scopes_and_other_owner() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(github_ok().insert_header("x-oauth-scopes", "repo, user"))
            .mount(&server)
            .await;

        let detail = check_github(&http::client().unwrap(), &server.uri(), "someone", "secret")
            .await
            .unwrap();

        assert!(detail.contains("no expiration"), "{detail}");
        assert!(detail.contains("the config user is `someone`"), "{detail}");
        assert!(
            detail.contains("the scopes `repo, user`, but needs none"),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn github_bad_token_is_rejected() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "message": "Bad credentials"
            })))
            .mount(&server)
            .await;

        let error = check_github(&http::client().unwrap(), &server.uri(), "a", "wrong")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("HTTP 401"), "{error}");
    }

    #[tokio::test]
    async fn github_graphql_error_with_http_200_is_rejected() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors": [{ "message": "Something went wrong" }]
            })))
            .mount(&server)
            .await;

        let error = check_github(&http::client().unwrap(), &server.uri(), "a", "t")
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "GitHub returned an error: Something went wrong"
        );
    }

    #[tokio::test]
    async fn lastfm_key_is_valid() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2.0/"))
            .and(query_param("method", "user.getrecenttracks"))
            .and(query_param("user", "me"))
            .and(query_param("api_key", "key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "recenttracks": { "track": [], "@attr": { "total": "12345" } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let detail = check_lastfm(&http::client().unwrap(), &server.uri(), "me", "key")
            .await
            .unwrap();

        assert_eq!(detail, "API key works, `me` has 12345 scrobbles");
    }

    async fn lastfm_error(status: u16, code: i64) -> String {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                "error": code,
                "message": "upstream message"
            })))
            .mount(&server)
            .await;

        check_lastfm(&http::client().unwrap(), &server.uri(), "me", "key")
            .await
            .unwrap_err()
            .to_string()
    }

    #[tokio::test]
    async fn lastfm_errors_are_explained() {
        assert_eq!(
            lastfm_error(403, 10).await,
            "Last.fm refused the request: invalid API key (error 10)"
        );
        assert!(
            lastfm_error(200, 17)
                .await
                .contains("the scrobbles are private")
        );
        assert!(lastfm_error(404, 6).await.contains("user `me` not found"));
        assert!(lastfm_error(500, 8).await.contains("upstream message"));
    }

    #[tokio::test]
    async fn atproto_handle_resolves_to_its_pds() {
        let server = MockServer::start().await;
        mount_identity(&server, "me.example").await;
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.describeRepo"))
            .and(query_param("repo", "did:plc:abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "did": "did:plc:abc",
                "handle": "me.example",
                "collections": ["app.bsky.feed.post", "sh.tangled.repo"]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let detail = check_atproto(
            &http::client().unwrap(),
            &identity_endpoints(&server),
            "me.example",
        )
        .await
        .unwrap();

        assert_eq!(
            detail,
            format!("did:plc:abc on the PDS {}, 2 collections", server.uri())
        );
    }

    #[tokio::test]
    async fn atproto_unknown_handle_is_reported() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&server)
            .await;

        let check = to_check(
            "atproto",
            check_atproto(
                &http::client().unwrap(),
                &identity_endpoints(&server),
                "nobody.example",
            )
            .await,
        );

        assert_eq!(check.status, Status::Failed);
        assert_eq!(
            check.detail,
            "cannot find the PDS of the account: the handle `nobody.example` does not resolve to a DID"
        );
    }

    #[tokio::test]
    async fn bluesky_app_password_works_and_session_is_closed() {
        let server = MockServer::start().await;
        mount_identity(&server, "me.example").await;
        Mock::given(method("POST"))
            .and(path("/xrpc/com.atproto.server.createSession"))
            .and(body_json(
                json!({ "identifier": "me.example", "password": "app-pw" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "did": "did:plc:abc",
                "handle": "me.example",
                "accessJwt": "access",
                "refreshJwt": "refresh"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/xrpc/com.atproto.server.deleteSession"))
            .and(header("authorization", "Bearer refresh"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let detail = check_bluesky(
            &http::client().unwrap(),
            &identity_endpoints(&server),
            "me.example",
            "app-pw",
        )
        .await
        .unwrap();

        assert_eq!(
            detail,
            format!(
                "app password works for `me.example` (did:plc:abc) on the PDS {}",
                server.uri()
            )
        );
    }

    #[tokio::test]
    async fn bluesky_bad_password_is_rejected() {
        let server = MockServer::start().await;
        mount_identity(&server, "me.example").await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": "AuthenticationRequired",
                "message": "Invalid identifier or password"
            })))
            .mount(&server)
            .await;

        let error = check_bluesky(
            &http::client().unwrap(),
            &identity_endpoints(&server),
            "me.example",
            "bad",
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            format!(
                "the PDS {} refused the login (HTTP 401 Unauthorized): Invalid identifier or password",
                server.uri()
            )
        );
    }

    #[tokio::test]
    async fn run_reports_missing_secrets_and_skips_disabled_sources() {
        let server = MockServer::start().await;
        let config = Config::parse(
            r#"
                [github]
                user = "me"
                token_env = "GITHUB_TOKEN"

                [bsky_bookmarks]
                enabled = false

                [[gitea]]
                name = "codeberg"
                url = "https://codeberg.org"
                user = "me"
            "#,
        )
        .unwrap();

        let checks = run(
            &config,
            |_| None,
            &http::client().unwrap(),
            &endpoints(&server),
        )
        .await;

        assert_eq!(
            checks,
            [
                Check {
                    source: "github",
                    status: Status::Failed,
                    detail: "the environment variable `GITHUB_TOKEN` is not set or is empty"
                        .to_owned(),
                },
                Check {
                    source: "bsky_bookmarks",
                    status: Status::Skipped,
                    detail: "disabled".to_owned(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn run_reads_the_secret_from_the_named_variable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("authorization", "bearer from-env"))
            .respond_with(github_ok())
            .expect(1)
            .mount(&server)
            .await;
        let config =
            Config::parse("[github]\nuser = \"TomPlanche\"\ntoken_env = \"MY_TOKEN\"").unwrap();
        let env = |name: &str| (name == "MY_TOKEN").then(|| "from-env".to_owned());

        let checks = run(&config, env, &http::client().unwrap(), &endpoints(&server)).await;

        assert_eq!(checks[0].status, Status::Ok, "{:?}", checks[0]);
    }

    #[tokio::test]
    async fn network_errors_show_the_cause() {
        // Nothing listens on this port: the connection fails.
        let check = to_check(
            "github",
            check_github(&http::client().unwrap(), "http://127.0.0.1:9", "a", "t").await,
        );

        assert_eq!(check.status, Status::Failed);
        assert!(
            check.detail.starts_with("the request failed: "),
            "{}",
            check.detail
        );
    }
}
