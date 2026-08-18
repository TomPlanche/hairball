//! Shared HTTP client, and the retry on HTTP 429 (rate limit).

use std::time::Duration;

use jiff::Timestamp;
use reqwest::{Client, RequestBuilder, Response, StatusCode, header::HeaderMap};
use tracing::warn;

/// Sent with every request, so that the APIs can identify the client.
pub const USER_AGENT: &str = concat!(
    "hairball/",
    env!("CARGO_PKG_VERSION"),
    " (+https://tomplanche.com)"
);

/// Every request fails after this delay.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// Retries after an HTTP 429, after the first try.
pub const MAX_RETRIES: u32 = 3;

/// A longer wait is not done: the request fails with its HTTP 429.
pub const MAX_WAIT: Duration = Duration::from_secs(10 * 60);

/// Builds the HTTP client: timeout and `User-Agent` on every request.
pub fn client() -> reqwest::Result<Client> {
    Client::builder()
        .user_agent(USER_AGENT)
        .timeout(TIMEOUT)
        .build()
}

/// Sends a request. On HTTP 429, waits as the server asks, then sends it again,
/// at most [`MAX_RETRIES`] times.
///
/// The last response comes back as it is: after the last retry, or if the wait
/// is longer than [`MAX_WAIT`], the caller gets the HTTP 429.
pub async fn send_with_retry(request: RequestBuilder) -> reqwest::Result<Response> {
    let mut retries = 0;
    loop {
        // A request with a streamed body cannot be cloned: send it once.
        let Some(attempt) = request.try_clone() else {
            return request.send().await;
        };
        let response = attempt.send().await?;
        if response.status() != StatusCode::TOO_MANY_REQUESTS || retries == MAX_RETRIES {
            return Ok(response);
        }

        let delay = retry_delay(response.headers(), Timestamp::now(), retries);
        if delay > MAX_WAIT {
            warn!(
                url = %response.url(),
                wait_seconds = delay.as_secs(),
                "rate limited for too long: no retry"
            );
            return Ok(response);
        }
        retries += 1;
        warn!(
            url = %response.url(),
            wait_seconds = delay.as_secs(),
            retry = retries,
            "rate limited: wait, then retry"
        );
        tokio::time::sleep(delay).await;
    }
}

/// How long to wait after an HTTP 429, from the response headers:
///
/// 1. `ratelimit-reset`: the unix time (seconds) when the limit resets (ATProto PDS).
/// 2. `retry-after`: a number of seconds (standard HTTP).
/// 3. Without these headers: 1, 2, then 4 seconds.
pub fn retry_delay(headers: &HeaderMap, now: Timestamp, retries: u32) -> Duration {
    let number = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|text| text.trim().parse::<i64>().ok())
    };

    if let Some(reset) = number("ratelimit-reset") {
        let seconds = reset - now.as_second();
        // 1 second more: the reset time is rounded down to the second.
        return Duration::from_secs(u64::try_from(seconds).map_or(0, |s| s + 1));
    }
    if let Some(seconds) = number("retry-after") {
        return Duration::from_secs(u64::try_from(seconds).unwrap_or(0));
    }
    Duration::from_secs(1 << retries.min(6))
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;

    const NOW: Timestamp = Timestamp::constant(1_786_000_000, 0);

    fn headers(name: &'static str, value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(name, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn delay_follows_ratelimit_reset() {
        let reset = (NOW.as_second() + 42).to_string();

        assert_eq!(
            retry_delay(&headers("ratelimit-reset", &reset), NOW, 0),
            Duration::from_secs(43)
        );
        // A reset time in the past: no wait.
        let past = (NOW.as_second() - 5).to_string();
        assert_eq!(
            retry_delay(&headers("ratelimit-reset", &past), NOW, 0),
            Duration::ZERO
        );
    }

    #[test]
    fn delay_follows_retry_after() {
        assert_eq!(
            retry_delay(&headers("retry-after", "7"), NOW, 0),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn delay_doubles_without_headers() {
        let none = HeaderMap::new();
        let delays: Vec<_> = (0..3)
            .map(|r| retry_delay(&none, NOW, r).as_secs())
            .collect();

        assert_eq!(delays, [1, 2, 4]);
    }

    /// A 429 that asks for no wait: the tests do not sleep.
    fn rate_limited() -> ResponseTemplate {
        ResponseTemplate::new(429).insert_header("ratelimit-reset", "0")
    }

    #[tokio::test]
    async fn retries_after_a_429() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(rate_limited())
            .up_to_n_times(2)
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;

        let client = client().unwrap();
        let response = send_with_retry(client.get(format!("{}/x", server.uri())))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn gives_up_after_the_last_retry() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(rate_limited())
            // 1 try + 3 retries.
            .expect(u64::from(MAX_RETRIES) + 1)
            .mount(&server)
            .await;

        let client = client().unwrap();
        let response = send_with_retry(client.get(server.uri())).await.unwrap();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn does_not_wait_too_long() {
        let server = MockServer::start().await;
        let far = (Timestamp::now().as_second() + 3600).to_string();
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("ratelimit-reset", far.as_str()))
            .expect(1)
            .mount(&server)
            .await;

        let client = client().unwrap();
        let response = send_with_retry(client.get(server.uri())).await.unwrap();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
