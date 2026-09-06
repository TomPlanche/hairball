//! Groups the ATProto collections (NSIDs) by app, for the export.
//!
//! All the apps write to the same repo: `app.bsky.feed.post` is Bluesky,
//! `sh.tangled.repo` is Tangled, etc. The export shows one heatmap row per app.

/// A known app: NSID prefix, export key, label.
const KNOWN_APPS: &[(&str, &str, &str)] = &[
    ("app.bsky", "bluesky", "Bluesky"),
    ("sh.tangled", "tangled", "Tangled"),
    // Tangled also writes records under its newer domain, tangled.org.
    ("org.tangled", "tangled", "Tangled"),
    ("site.standard", "standard", "Standard"),
    ("fm.teal", "teal", "teal.fm"),
    ("app.rocksky", "rocksky", "Rocksky"),
    // Apps that spread their collections over several sub-namespaces: without
    // an entry, each sub-namespace would be a separate source.
    ("id.sifa", "sifa", "sifa.id"),
    ("dev.npmx", "npmx", "npmx.dev"),
    ("fyi.atstore", "atstore", "atstore.fyi"),
];

/// The app of a collection, as an export key and a label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub key: String,
    pub label: String,
}

/// Finds the app of an NSID. An unknown NSID uses its authority (the NSID
/// without its last segment, for example `com.tomplanche.atkuzu`) as the key
/// and the label.
pub fn app_of(nsid: &str) -> App {
    for (prefix, key, label) in KNOWN_APPS {
        if nsid == *prefix || nsid.starts_with(&format!("{prefix}.")) {
            return App {
                key: (*key).to_owned(),
                label: (*label).to_owned(),
            };
        }
    }
    let authority = nsid
        .rsplit_once('.')
        .map_or(nsid, |(authority, _)| authority);
    App {
        key: authority.to_owned(),
        label: authority.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(nsid: &str) -> String {
        app_of(nsid).key
    }

    #[test]
    fn known_prefixes_give_the_app() {
        assert_eq!(key("app.bsky.feed.post"), "bluesky");
        assert_eq!(key("app.bsky.feed.like"), "bluesky");
        assert_eq!(key("sh.tangled.repo.pull"), "tangled");
        assert_eq!(key("site.standard.document"), "standard");
        assert_eq!(app_of("fm.teal.alpha.feed.play").label, "teal.fm");
        assert_eq!(key("app.rocksky.scrobble"), "rocksky");
    }

    #[test]
    fn sub_namespaces_of_one_app_share_its_key() {
        assert_eq!(key("org.tangled.graph.attestation"), "tangled");
        assert_eq!(key("org.tangled.feed.star"), "tangled");
        assert_eq!(key("id.sifa.profile.skill"), "sifa");
        assert_eq!(key("id.sifa.graph.follow"), "sifa");
        assert_eq!(key("dev.npmx.feed.like"), "npmx");
        assert_eq!(key("fyi.atstore.listing.item"), "atstore");
    }

    #[test]
    fn a_prefix_stops_at_a_dot() {
        // `app.bskyx` is another authority than `app.bsky`.
        assert_eq!(key("app.bskyx.feed.post"), "app.bskyx.feed");
    }

    #[test]
    fn unknown_nsid_uses_its_authority() {
        assert_eq!(
            app_of("com.tomplanche.atkuzu.result"),
            App {
                key: "com.tomplanche.atkuzu".to_owned(),
                label: "com.tomplanche.atkuzu".to_owned(),
            }
        );
        assert_eq!(key("nodots"), "nodots");
    }
}
