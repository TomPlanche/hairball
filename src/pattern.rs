//! Patterns with `*`, for the NSIDs and the kinds (`exclude`, `purge --kind`).

/// Matches `text` against `pattern`, where `*` matches any characters (none
/// included). The pattern must match the whole text. Without `*`, the text
/// must be equal to the pattern.
pub fn matches(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == text;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    // `pattern` contains `*`: `split` gives at least 2 parts.
    let (Some(first), Some(last)) = (parts.first(), parts.last()) else {
        return false;
    };
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    // The middle parts, in order, each after the previous one.
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(index) => rest = &rest[index + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_matches_any_characters() {
        assert!(matches("app.rocksky.*", "app.rocksky.scrobble"));
        assert!(matches("app.rocksky.*", "app.rocksky.actor.profile"));
        assert!(matches("*.feed.like", "app.bsky.feed.like"));
        assert!(matches("app.*.actor.*", "app.rocksky.actor.profile"));
        assert!(matches("*", "anything"));
        assert!(!matches("app.rocksky.*", "app.rockskyx.scrobble"));
        assert!(!matches("app.*.feed", "app.bsky.feed.post"));
        assert!(!matches("a*a", "a"));
    }

    #[test]
    fn without_star_the_text_must_be_equal() {
        assert!(matches("scrobble", "scrobble"));
        assert!(!matches("app.bsky.feed", "app.bsky.feed.post"));
    }
}
