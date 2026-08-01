//! Shared HTTP client.

use std::time::Duration;

use reqwest::Client;

/// Sent with every request, so that the APIs can identify the client.
pub const USER_AGENT: &str = concat!(
    "hairball/",
    env!("CARGO_PKG_VERSION"),
    " (+https://tomplanche.com)"
);

/// Every request fails after this delay.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// Builds the HTTP client: timeout and `User-Agent` on every request.
pub fn client() -> reqwest::Result<Client> {
    Client::builder()
        .user_agent(USER_AGENT)
        .timeout(TIMEOUT)
        .build()
}
