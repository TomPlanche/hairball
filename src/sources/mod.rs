//! The source adapters, and the dispatch of a sync to each of them.
//!
//! `async fn` in a trait is not dyn-compatible: the adapters are the variants
//! of an enum, not `Box<dyn Source>`.

pub mod atproto;
pub mod gitea;
pub mod github;

use std::fmt;

use jiff::Timestamp;
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::{config::Config, db, db::DbError, identity::IdentityError};

/// What a sync needs: the database, the HTTP client and the run options.
#[derive(Debug, Clone, Copy)]
pub struct SyncContext<'a> {
    pub db: &'a SqlitePool,
    pub client: &'a Client,
    /// The start time of the run. All rows written by the run use it.
    pub now: Timestamp,
    /// Ignore the cursors and reconcile deletions.
    pub full: bool,
}

/// The counts of one sync.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub inserted: u64,
    pub updated: u64,
    pub unchanged: u64,
    /// Rows marked as deleted upstream.
    pub deleted: u64,
    /// Rows removed because their kind is disabled in the config.
    pub purged: u64,
}

impl SyncReport {
    pub fn count(&mut self, upsert: db::Upsert) {
        match upsert {
            db::Upsert::Inserted => self.inserted += 1,
            db::Upsert::Updated => self.updated += 1,
            db::Upsert::Unchanged => self.unchanged += 1,
        }
    }
}

impl fmt::Display for SyncReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} inserted, {} updated, {} unchanged, {} deleted, {} purged",
            self.inserted, self.updated, self.unchanged, self.deleted, self.purged
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("the request failed")]
    Http(#[from] reqwest::Error),

    #[error("{url} answered HTTP {status}: {message}")]
    Api {
        url: String,
        status: StatusCode,
        message: String,
    },

    #[error("cannot find the PDS of the account")]
    Identity(#[from] IdentityError),

    #[error("the environment variable `{0}` is not set or is empty")]
    MissingSecret(String),

    #[error("{url} returned a GraphQL error: {message}")]
    Graphql { url: String, message: String },

    #[error("database error")]
    Db(#[from] DbError),

    #[error("cannot encode the record as JSON")]
    Json(#[from] serde_json::Error),
}

/// Builds the error of a response with an HTTP error status. ATProto (XRPC)
/// and Gitea both send a JSON body with a `message` field.
pub(crate) async fn api_error(url: &str, response: Response) -> SourceError {
    #[derive(Deserialize)]
    struct ErrorBody {
        message: Option<String>,
    }
    let status = response.status();
    let message = response
        .json::<ErrorBody>()
        .await
        .ok()
        .and_then(|body| body.message)
        .unwrap_or_else(|| "no message".to_owned());
    SourceError::Api {
        url: url.to_owned(),
        status,
        message,
    }
}

/// A source adapter, ready to run.
#[derive(Debug, Clone)]
pub enum AnySource {
    Atproto(atproto::Atproto),
    Gitea(gitea::Gitea),
    Github(github::Github),
}

impl AnySource {
    /// The source id: the key of `sync_state` and of the rows of the source.
    pub fn id(&self) -> String {
        match self {
            Self::Atproto(_) => atproto::SOURCE.to_owned(),
            Self::Gitea(source) => source.id(),
            Self::Github(_) => github::SOURCE.to_owned(),
        }
    }

    /// Runs one sync: reads the source, writes the rows, stores the new state.
    pub async fn sync(&self, ctx: &SyncContext<'_>) -> Result<SyncReport, SourceError> {
        match self {
            Self::Atproto(source) => source.sync(ctx).await,
            Self::Gitea(source) => source.sync(ctx).await,
            Self::Github(source) => source.sync(ctx).await,
        }
    }
}

/// Why a configured source does not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    Disabled,
    /// A Gitea instance that mirrors another source (it would count each commit twice).
    Mirror,
    NotImplemented,
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Disabled => "disabled in the config",
            Self::Mirror => "mirror of another source",
            Self::NotImplemented => "adapter not implemented yet",
        })
    }
}

/// What a sync does with one configured source.
#[derive(Debug, Clone)]
pub enum Planned {
    Run(AnySource),
    Skip { id: String, reason: SkipReason },
}

impl Planned {
    pub fn id(&self) -> String {
        match self {
            Self::Run(source) => source.id(),
            Self::Skip { id, .. } => id.clone(),
        }
    }
}

/// Lists the configured sources, in the order of [`Config::sources`].
pub fn plan(config: &Config) -> Vec<Planned> {
    let skip = |id: &str, reason| Planned::Skip {
        id: id.to_owned(),
        reason,
    };
    let mut planned = Vec::new();

    if let Some(atproto) = &config.atproto {
        planned.push(Planned::Run(AnySource::Atproto(atproto::Atproto::new(
            atproto,
        ))));
    }
    if let Some(bookmarks) = &config.bsky_bookmarks {
        let reason = if bookmarks.enabled {
            SkipReason::NotImplemented
        } else {
            SkipReason::Disabled
        };
        planned.push(skip("bsky_bookmarks", reason));
    }
    for instance in &config.gitea {
        let source = gitea::Gitea::new(instance, config.time_zone.clone());
        planned.push(if instance.mirror {
            skip(&source.id(), SkipReason::Mirror)
        } else {
            Planned::Run(AnySource::Gitea(source))
        });
    }
    if let Some(github) = &config.github {
        planned.push(Planned::Run(AnySource::Github(github::Github::new(github))));
    }
    if config.lastfm.is_some() {
        planned.push(skip("lastfm", SkipReason::NotImplemented));
    }

    planned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_follows_the_config_order() {
        let config = Config::parse(include_str!("../../config.example.toml")).unwrap();
        let planned: Vec<_> = plan(&config)
            .iter()
            .map(|p| match p {
                Planned::Run(source) => (source.id(), None),
                Planned::Skip { id, reason } => (id.clone(), Some(*reason)),
            })
            .collect();

        assert_eq!(
            planned,
            [
                ("atproto".to_owned(), None),
                ("bsky_bookmarks".to_owned(), Some(SkipReason::Disabled)),
                ("gitea:codeberg".to_owned(), None),
                ("gitea:forge-perso".to_owned(), Some(SkipReason::Mirror)),
                ("github".to_owned(), None),
                ("lastfm".to_owned(), Some(SkipReason::NotImplemented)),
            ]
        );
        // The same ids as `status`.
        let ids: Vec<_> = config.sources().into_iter().map(|s| s.id).collect();
        assert_eq!(
            plan(&config).iter().map(Planned::id).collect::<Vec<_>>(),
            ids
        );
    }

    #[test]
    fn report_counts_upserts() {
        let mut report = SyncReport::default();
        report.count(db::Upsert::Inserted);
        report.count(db::Upsert::Inserted);
        report.count(db::Upsert::Unchanged);

        assert_eq!(
            report.to_string(),
            "2 inserted, 0 updated, 1 unchanged, 0 deleted, 0 purged"
        );
    }
}
