//! TOML configuration: types, loading and validation.

use std::{
    collections::HashSet,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use jiff::tz::TimeZone;
use serde::Deserialize;

/// The full configuration file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Time zone for the day buckets of the export: an IANA name.
    #[serde(default = "default_timezone")]
    pub timezone: String,

    /// The time zone of `timezone`, found by [`Config::parse`].
    #[serde(skip, default = "utc")]
    pub time_zone: TimeZone,

    /// Path to the SQLite database file.
    #[serde(default = "default_database")]
    pub database: PathBuf,

    pub atproto: Option<AtprotoConfig>,
    pub bsky_bookmarks: Option<BskyBookmarksConfig>,

    #[serde(default)]
    pub gitea: Vec<GiteaConfig>,

    pub github: Option<GithubConfig>,
    pub lastfm: Option<LastfmConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AtprotoConfig {
    /// A handle (`example.com`) or a DID.
    pub handle: String,

    /// NSIDs or NSID prefixes to ignore.
    #[serde(default)]
    pub exclude: Vec<String>,

    #[serde(default)]
    pub bluesky: BlueskyToggles,
}

/// Fine-grained toggles for the `app.bsky.*` collections.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BlueskyToggles {
    /// `app.bsky.feed.post`
    pub posts: bool,
    /// `app.bsky.feed.like`
    pub likes: bool,
    /// `app.bsky.feed.repost`
    pub reposts: bool,
}

impl Default for BlueskyToggles {
    fn default() -> Self {
        Self {
            posts: true,
            likes: true,
            reposts: true,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BskyBookmarksConfig {
    pub enabled: bool,
    pub identifier: String,
    /// Name of the environment variable that holds the app password.
    pub app_password_env: String,
}

/// One Gitea, Forgejo or Codeberg instance.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GiteaConfig {
    /// Short name, used in the source id `gitea:<name>`.
    pub name: String,
    pub url: String,
    pub user: String,

    /// The instance mirrors repositories of another source: skip it.
    #[serde(default)]
    pub mirror: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
    pub user: String,
    /// Name of the environment variable that holds the token.
    pub token_env: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LastfmConfig {
    pub user: String,
    /// Name of the environment variable that holds the API key.
    pub api_key_env: String,
}

/// A source declared in the config, with its state.
#[derive(Debug, PartialEq, Eq)]
pub struct ConfiguredSource {
    pub id: String,
    pub state: SourceState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceState {
    Enabled,
    Disabled,
    /// A Gitea instance with `mirror = true`.
    Mirror,
}

impl fmt::Display for SourceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Mirror => "skipped (mirror)",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read the file")]
    Read(#[source] io::Error),

    #[error("cannot parse the TOML")]
    Parse(#[from] toml::de::Error),

    #[error("the config is not valid:{0}")]
    Invalid(Issues),
}

/// One validation error, with the TOML path of the bad value.
#[derive(Debug, PartialEq, Eq)]
pub struct Issue {
    pub path: String,
    pub message: String,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// All the validation errors of a config.
#[derive(Debug)]
pub struct Issues(pub Vec<Issue>);

impl fmt::Display for Issues {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for issue in &self.0 {
            write!(f, "\n  - {issue}")?;
        }
        Ok(())
    }
}

impl Config {
    /// Reads, parses and validates the config file at `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(ConfigError::Read)?;
        Self::parse(&text)
    }

    /// Parses and validates a config from TOML text.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut config: Self = toml::from_str(text)?;
        let mut v = Validator::default();
        // On an error, the issue is recorded and `parse` fails below: UTC is never used.
        config.time_zone = v
            .time_zone("timezone", &config.timezone)
            .unwrap_or(TimeZone::UTC);
        config.validate(&mut v);

        if v.issues.is_empty() {
            Ok(config)
        } else {
            Err(ConfigError::Invalid(Issues(v.issues)))
        }
    }

    /// Records all validation errors at once in `v`.
    fn validate(&self, v: &mut Validator) {
        v.non_empty("database", &self.database.to_string_lossy());

        if let Some(atproto) = &self.atproto {
            v.non_empty("atproto.handle", &atproto.handle);
            for (i, nsid) in atproto.exclude.iter().enumerate() {
                v.non_empty(&format!("atproto.exclude[{i}]"), nsid);
            }
        }

        if let Some(bookmarks) = &self.bsky_bookmarks
            && bookmarks.enabled
        {
            v.non_empty("bsky_bookmarks.identifier", &bookmarks.identifier);
            v.env_var_name(
                "bsky_bookmarks.app_password_env",
                &bookmarks.app_password_env,
            );
        }

        let mut names = HashSet::new();
        for (i, gitea) in self.gitea.iter().enumerate() {
            let path = |field: &str| format!("gitea[{i}].{field}");

            if !is_source_name(&gitea.name) {
                v.push(
                    path("name"),
                    format!(
                        "`{}` must be non-empty and use only ASCII letters, digits, `-` and `_`",
                        gitea.name
                    ),
                );
            } else if !names.insert(gitea.name.as_str()) {
                v.push(
                    path("name"),
                    format!("`{}` is already used by another instance", gitea.name),
                );
            }

            if !(gitea.url.starts_with("https://") || gitea.url.starts_with("http://")) {
                v.push(
                    path("url"),
                    format!("`{}` must start with `https://` or `http://`", gitea.url),
                );
            }

            v.non_empty(&path("user"), &gitea.user);
        }

        if let Some(github) = &self.github {
            v.non_empty("github.user", &github.user);
            v.env_var_name("github.token_env", &github.token_env);
        }

        if let Some(lastfm) = &self.lastfm {
            v.non_empty("lastfm.user", &lastfm.user);
            v.env_var_name("lastfm.api_key_env", &lastfm.api_key_env);
        }
    }

    /// Lists the sources declared in the config, in a stable order.
    pub fn sources(&self) -> Vec<ConfiguredSource> {
        let mut sources = Vec::new();
        let mut add = |id: String, state| sources.push(ConfiguredSource { id, state });

        if self.atproto.is_some() {
            add("atproto".to_owned(), SourceState::Enabled);
        }
        if let Some(bookmarks) = &self.bsky_bookmarks {
            let state = if bookmarks.enabled {
                SourceState::Enabled
            } else {
                SourceState::Disabled
            };
            add("bsky_bookmarks".to_owned(), state);
        }
        for gitea in &self.gitea {
            let state = if gitea.mirror {
                SourceState::Mirror
            } else {
                SourceState::Enabled
            };
            add(format!("gitea:{}", gitea.name), state);
        }
        if self.github.is_some() {
            add("github".to_owned(), SourceState::Enabled);
        }
        if self.lastfm.is_some() {
            add("lastfm".to_owned(), SourceState::Enabled);
        }

        sources
    }
}

fn default_timezone() -> String {
    "Europe/Paris".to_owned()
}

fn default_database() -> PathBuf {
    PathBuf::from("hairball.db")
}

fn utc() -> TimeZone {
    TimeZone::UTC
}

/// Collects validation errors.
#[derive(Default)]
struct Validator {
    issues: Vec<Issue>,
}

impl Validator {
    fn push(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.issues.push(Issue {
            path: path.into(),
            message: message.into(),
        });
    }

    fn non_empty(&mut self, path: &str, value: &str) {
        if value.trim().is_empty() {
            self.push(path, "must not be empty");
        }
    }

    fn env_var_name(&mut self, path: &str, value: &str) {
        if !is_env_var_name(value) {
            self.push(
                path,
                format!("`{value}` is not a valid environment variable name"),
            );
        }
    }

    /// Finds the time zone `name` in the system time zone database.
    fn time_zone(&mut self, path: &str, name: &str) -> Option<TimeZone> {
        if name.trim().is_empty() {
            self.push(path, "must not be empty");
            return None;
        }
        match TimeZone::get(name) {
            Ok(tz) => Some(tz),
            Err(_) => {
                self.push(
                    path,
                    format!(
                        "`{name}` is not a known time zone (use an IANA name, for example `Europe/Paris`)"
                    ),
                );
                None
            }
        }
    }
}

/// `[A-Za-z_][A-Za-z0-9_]*`
fn is_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `[A-Za-z0-9_-]+`
fn is_source_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issues(text: &str) -> Vec<String> {
        match Config::parse(text) {
            Err(ConfigError::Invalid(Issues(issues))) => {
                issues.iter().map(ToString::to_string).collect()
            }
            other => panic!("expected validation errors, got {other:?}"),
        }
    }

    #[test]
    fn example_config_is_valid() {
        let config = Config::parse(include_str!("../config.example.toml")).unwrap();

        assert_eq!(config.timezone, "Europe/Paris");
        assert_eq!(config.gitea.len(), 2);
        assert!(!config.atproto.unwrap().bluesky.reposts);
    }

    #[test]
    fn empty_config_uses_defaults() {
        let config = Config::parse("").unwrap();

        assert_eq!(config.timezone, "Europe/Paris");
        assert_eq!(config.time_zone.iana_name(), Some("Europe/Paris"));
        assert_eq!(config.database, PathBuf::from("hairball.db"));
        assert!(config.sources().is_empty());
    }

    #[test]
    fn time_zone_is_found_by_name() {
        let config = Config::parse("timezone = \"America/New_York\"").unwrap();

        assert_eq!(config.time_zone.iana_name(), Some("America/New_York"));
    }

    #[test]
    fn unknown_time_zone_is_rejected() {
        assert_eq!(
            issues("timezone = \"Europe/Atlantis\""),
            [
                "timezone: `Europe/Atlantis` is not a known time zone (use an IANA name, for example `Europe/Paris`)"
            ]
        );
    }

    #[test]
    fn time_zone_cannot_be_set_directly() {
        // `time_zone` is computed: the TOML file cannot set it.
        let error = Config::parse("time_zone = \"UTC\"");

        assert!(matches!(error, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn bluesky_toggles_default_to_true() {
        let config = Config::parse("[atproto]\nhandle = \"example.com\"").unwrap();
        let toggles = config.atproto.unwrap().bluesky;

        assert!(toggles.posts && toggles.likes && toggles.reposts);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let error = Config::parse("[github]\nuser = \"a\"\ntoken_env = \"T\"\ntoken = \"x\"");

        assert!(matches!(error, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn all_errors_are_reported_at_once() {
        let text = r#"
            timezone = ""

            [github]
            user = ""
            token_env = "1TOKEN"

            [[gitea]]
            name = "code berg"
            url = "codeberg.org"
            user = "me"
        "#;

        assert_eq!(
            issues(text),
            [
                "timezone: must not be empty",
                "gitea[0].name: `code berg` must be non-empty and use only ASCII letters, digits, `-` and `_`",
                "gitea[0].url: `codeberg.org` must start with `https://` or `http://`",
                "github.user: must not be empty",
                "github.token_env: `1TOKEN` is not a valid environment variable name",
            ]
        );
    }

    #[test]
    fn duplicate_gitea_names_are_rejected() {
        let text = r#"
            [[gitea]]
            name = "codeberg"
            url = "https://codeberg.org"
            user = "me"

            [[gitea]]
            name = "codeberg"
            url = "https://codeberg.org"
            user = "other"
        "#;

        assert_eq!(
            issues(text),
            ["gitea[1].name: `codeberg` is already used by another instance"]
        );
    }

    #[test]
    fn disabled_bookmarks_need_no_credentials() {
        assert!(Config::parse("[bsky_bookmarks]\nenabled = false").is_ok());
        assert_eq!(
            issues("[bsky_bookmarks]\nenabled = true"),
            [
                "bsky_bookmarks.identifier: must not be empty",
                "bsky_bookmarks.app_password_env: `` is not a valid environment variable name",
            ]
        );
    }

    #[test]
    fn sources_of_the_example_config() {
        let config = Config::parse(include_str!("../config.example.toml")).unwrap();
        let sources: Vec<_> = config
            .sources()
            .into_iter()
            .map(|s| (s.id, s.state))
            .collect();

        assert_eq!(
            sources,
            [
                ("atproto".to_owned(), SourceState::Enabled),
                ("bsky_bookmarks".to_owned(), SourceState::Disabled),
                ("gitea:codeberg".to_owned(), SourceState::Enabled),
                ("gitea:forge-perso".to_owned(), SourceState::Mirror),
                ("github".to_owned(), SourceState::Enabled),
                ("lastfm".to_owned(), SourceState::Enabled),
            ]
        );
    }
}
