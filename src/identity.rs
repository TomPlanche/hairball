//! ATProto identity resolution: handle -> DID -> PDS.
//!
//! Every account lives on one PDS (Personal Data Server), with its own URL:
//! `bsky.social` hosts, Eurosky, a self-hosted server, etc. The DID document
//! of the account gives this URL.

use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::Value;

/// Base URLs used for the resolution. Tests replace them with a mock server.
#[derive(Debug, Clone)]
pub struct IdentityEndpoints {
    /// The PLC directory, for `did:plc` documents.
    pub plc: String,
    /// A service that implements `com.atproto.identity.resolveHandle`.
    /// Used only when the HTTPS method fails (for example a handle set with DNS only).
    pub handle_resolver: String,
    /// Replaces `https://<domain>` for the well-known files. For tests only.
    pub web_override: Option<String>,
}

impl Default for IdentityEndpoints {
    fn default() -> Self {
        Self {
            plc: "https://plc.directory".to_owned(),
            handle_resolver: "https://public.api.bsky.app".to_owned(),
            web_override: None,
        }
    }
}

/// A resolved account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub did: String,
    /// The handle given by the user, or the first one of the DID document.
    pub handle: Option<String>,
    /// URL of the PDS, without a trailing `/`.
    pub pds: String,
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("the request failed")]
    Http(#[from] reqwest::Error),

    #[error("the handle `{0}` does not resolve to a DID")]
    HandleNotFound(String),

    #[error("the DID method of `{0}` is not supported (only did:plc and did:web)")]
    UnsupportedDid(String),

    #[error("the DID document of `{did}` cannot be read (HTTP {status})")]
    DidNotFound { did: String, status: StatusCode },

    #[error("the DID document of `{0}` has another id")]
    DidMismatch(String),

    #[error("the DID document of `{did}` does not confirm the handle `{handle}`")]
    HandleMismatch { did: String, handle: String },

    #[error("the DID document of `{0}` has no PDS service")]
    NoPds(String),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DidDocument {
    id: String,
    #[serde(default)]
    also_known_as: Vec<String>,
    #[serde(default)]
    service: Vec<Service>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Service {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    service_endpoint: Value,
}

impl DidDocument {
    /// The `#atproto_pds` service endpoint.
    fn pds(&self) -> Option<String> {
        self.service
            .iter()
            .find(|s| s.id.ends_with("#atproto_pds") && s.kind == "AtprotoPersonalDataServer")
            .and_then(|s| s.service_endpoint.as_str())
            .map(|url| url.trim_end_matches('/').to_owned())
    }

    /// The handles of the account: `at://<handle>` entries.
    fn handles(&self) -> impl Iterator<Item = &str> {
        self.also_known_as
            .iter()
            .filter_map(|aka| aka.strip_prefix("at://"))
    }
}

/// Resolves a handle (`example.com`, `@example.com`) or a DID to the account PDS.
///
/// For a handle, the DID document must list the handle back (`alsoKnownAs`):
/// anybody can point a domain to any DID.
pub async fn resolve(
    client: &Client,
    endpoints: &IdentityEndpoints,
    identifier: &str,
) -> Result<Identity, IdentityError> {
    let identifier = identifier.trim();
    let (did, handle) = if identifier.starts_with("did:") {
        (identifier.to_owned(), None)
    } else {
        let handle = identifier.trim_start_matches('@').to_ascii_lowercase();
        let did = resolve_handle(client, endpoints, &handle).await?;
        (did, Some(handle))
    };

    let document = did_document(client, endpoints, &did).await?;
    if document.id != did {
        return Err(IdentityError::DidMismatch(did));
    }
    if let Some(handle) = &handle
        && !document.handles().any(|h| h.eq_ignore_ascii_case(handle))
    {
        return Err(IdentityError::HandleMismatch {
            did,
            handle: handle.clone(),
        });
    }
    let pds = document
        .pds()
        .ok_or_else(|| IdentityError::NoPds(did.clone()))?;
    let handle = handle.or_else(|| document.handles().next().map(str::to_owned));

    Ok(Identity { did, handle, pds })
}

/// Handle -> DID: first `https://<handle>/.well-known/atproto-did`, then `resolveHandle`.
async fn resolve_handle(
    client: &Client,
    endpoints: &IdentityEndpoints,
    handle: &str,
) -> Result<String, IdentityError> {
    let url = format!("{}/.well-known/atproto-did", web_base(endpoints, handle));
    // Any failure of the HTTPS method (no server, 404, bad body) goes to the fallback.
    if let Ok(response) = client.get(url).send().await
        && response.status().is_success()
        && let Ok(text) = response.text().await
    {
        let did = text.trim();
        if did.starts_with("did:") && !did.contains(char::is_whitespace) {
            return Ok(did.to_owned());
        }
    }

    #[derive(Deserialize)]
    struct Resolved {
        did: String,
    }
    let response = client
        .get(format!(
            "{}/xrpc/com.atproto.identity.resolveHandle",
            endpoints.handle_resolver
        ))
        .query(&[("handle", handle)])
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(IdentityError::HandleNotFound(handle.to_owned()));
    }
    Ok(response.json::<Resolved>().await?.did)
}

/// DID -> DID document, for `did:plc` and `did:web`.
async fn did_document(
    client: &Client,
    endpoints: &IdentityEndpoints,
    did: &str,
) -> Result<DidDocument, IdentityError> {
    let url = if did.starts_with("did:plc:") {
        format!("{}/{did}", endpoints.plc)
    } else if let Some(domain) = did.strip_prefix("did:web:")
        && !domain.is_empty()
        && !domain.contains(':')
    {
        format!("{}/.well-known/did.json", web_base(endpoints, domain))
    } else {
        return Err(IdentityError::UnsupportedDid(did.to_owned()));
    };

    let response = client.get(url).send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(IdentityError::DidNotFound {
            did: did.to_owned(),
            status,
        });
    }
    Ok(response.json().await?)
}

fn web_base(endpoints: &IdentityEndpoints, domain: &str) -> String {
    endpoints
        .web_override
        .clone()
        .unwrap_or_else(|| format!("https://{domain}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };

    use super::*;
    use crate::http;

    const DID: &str = "did:plc:abc123";

    fn endpoints(server: &MockServer) -> IdentityEndpoints {
        IdentityEndpoints {
            plc: server.uri(),
            handle_resolver: server.uri(),
            web_override: Some(server.uri()),
        }
    }

    fn document(id: &str, handle: &str, pds: &str) -> Value {
        json!({
            "id": id,
            "alsoKnownAs": [format!("at://{handle}")],
            "service": [{
                "id": "#atproto_pds",
                "type": "AtprotoPersonalDataServer",
                "serviceEndpoint": pds
            }]
        })
    }

    async fn mount_plc(server: &MockServer, body: Value) {
        Mock::given(method("GET"))
            .and(path(format!("/{DID}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn resolve_on(server: &MockServer, identifier: &str) -> Result<Identity, IdentityError> {
        resolve(&http::client().unwrap(), &endpoints(server), identifier).await
    }

    #[tokio::test]
    async fn handle_on_a_custom_pds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/atproto-did"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("{DID}\n")))
            .expect(1)
            .mount(&server)
            .await;
        mount_plc(
            &server,
            document(DID, "me.example.com", "https://pds.example.eu/"),
        )
        .await;

        let identity = resolve_on(&server, "@Me.Example.com").await.unwrap();

        assert_eq!(
            identity,
            Identity {
                did: DID.to_owned(),
                handle: Some("me.example.com".to_owned()),
                pds: "https://pds.example.eu".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn dns_only_handle_uses_resolve_handle() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/atproto-did"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.identity.resolveHandle"))
            .and(query_param("handle", "me.example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "did": DID })))
            .expect(1)
            .mount(&server)
            .await;
        mount_plc(
            &server,
            document(DID, "me.example.com", "https://pds.example.eu"),
        )
        .await;

        let identity = resolve_on(&server, "me.example.com").await.unwrap();

        assert_eq!(identity.did, DID);
    }

    #[tokio::test]
    async fn unknown_handle_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.identity.resolveHandle"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "InvalidRequest", "message": "Unable to resolve handle"
            })))
            .mount(&server)
            .await;

        let error = resolve_on(&server, "nobody.example.com").await.unwrap_err();

        assert!(matches!(error, IdentityError::HandleNotFound(h) if h == "nobody.example.com"));
    }

    #[tokio::test]
    async fn did_skips_the_handle_lookup() {
        let server = MockServer::start().await;
        mount_plc(
            &server,
            document(DID, "me.example.com", "https://pds.example.eu"),
        )
        .await;

        let identity = resolve_on(&server, DID).await.unwrap();

        assert_eq!(identity.handle.as_deref(), Some("me.example.com"));
        assert_eq!(identity.pds, "https://pds.example.eu");
    }

    #[tokio::test]
    async fn handle_not_confirmed_by_the_document_is_rejected() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/atproto-did"))
            .respond_with(ResponseTemplate::new(200).set_body_string(DID))
            .mount(&server)
            .await;
        mount_plc(
            &server,
            document(DID, "someone.else", "https://pds.example.eu"),
        )
        .await;

        let error = resolve_on(&server, "me.example.com").await.unwrap_err();

        assert!(
            matches!(error, IdentityError::HandleMismatch { .. }),
            "{error}"
        );
    }

    #[tokio::test]
    async fn document_without_pds_is_rejected() {
        let server = MockServer::start().await;
        mount_plc(
            &server,
            json!({ "id": DID, "alsoKnownAs": ["at://me.example.com"], "service": [] }),
        )
        .await;

        let error = resolve_on(&server, DID).await.unwrap_err();

        assert!(matches!(error, IdentityError::NoPds(_)), "{error}");
    }

    #[tokio::test]
    async fn did_web_reads_the_well_known_document() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/did.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(document(
                "did:web:me.example.com",
                "me.example.com",
                "https://pds.example.eu",
            )))
            .expect(1)
            .mount(&server)
            .await;

        let identity = resolve_on(&server, "did:web:me.example.com").await.unwrap();

        assert_eq!(identity.pds, "https://pds.example.eu");
    }

    #[tokio::test]
    async fn other_did_methods_are_not_supported() {
        let server = MockServer::start().await;

        let error = resolve_on(&server, "did:key:z6Mk").await.unwrap_err();

        assert!(matches!(error, IdentityError::UnsupportedDid(_)), "{error}");
    }
}
