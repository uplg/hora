//! Probing logic: turn a monitor into a single [`Outcome`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

use reqwest::{Client, RequestBuilder};
use tokio::net::TcpStream;

use crate::config::{Kind, Monitor};

/// Maximum length (chars) of the response-body snippet kept on failure.
const MAX_BODY_SNIPPET: usize = 300;

/// Result of a single probe.
#[derive(Debug)]
pub struct Outcome {
    pub up: bool,
    pub degraded: bool,
    pub latency_ms: Option<i64>,
    pub status_code: Option<i64>,
    pub error: Option<String>,
}

impl Outcome {
    /// Numeric status stored in the database: 0 = down, 1 = up, 2 = degraded.
    #[must_use]
    pub fn status_value(&self) -> i64 {
        if !self.up {
            0
        } else if self.degraded {
            2
        } else {
            1
        }
    }

    fn down(error: String) -> Self {
        Self {
            up: false,
            degraded: false,
            latency_ms: None,
            status_code: None,
            error: Some(error),
        }
    }
}

/// Probe a monitor according to its kind.
#[must_use]
pub async fn run(client: &Client, monitor: &Monitor) -> Outcome {
    match monitor.kind {
        Kind::Http => http(client, monitor).await,
        Kind::Tcp => tcp(monitor).await,
    }
}

async fn http(client: &Client, monitor: &Monitor) -> Outcome {
    let start = Instant::now();
    let request = with_headers(
        client.get(&monitor.target).timeout(monitor.timeout()),
        &monitor.headers,
    );
    let result = request.send().await;
    let latency = millis(start.elapsed());

    match result {
        Ok(response) => {
            let code = response.status().as_u16();
            let up = match monitor.expected_status {
                Some(expected) => code == expected,
                None => response.status().is_success(),
            };
            let degraded = up && over_threshold(latency, monitor.degraded_over_ms);
            let error = if up {
                None
            } else {
                // Capture a bounded body snippet so the alert says *what* broke.
                let snippet = body_snippet(response).await;
                Some(if snippet.is_empty() {
                    format!("HTTP {code}")
                } else {
                    format!("HTTP {code}: {snippet}")
                })
            };
            Outcome {
                up,
                degraded,
                latency_ms: Some(latency),
                status_code: Some(i64::from(code)),
                error,
            }
        }
        Err(err) => Outcome::down(describe(&err).to_owned()),
    }
}

async fn tcp(monitor: &Monitor) -> Outcome {
    let start = Instant::now();
    match tokio::time::timeout(monitor.timeout(), TcpStream::connect(&monitor.target)).await {
        Ok(Ok(_stream)) => {
            let latency = millis(start.elapsed());
            Outcome {
                up: true,
                degraded: over_threshold(latency, monitor.degraded_over_ms),
                latency_ms: Some(latency),
                status_code: None,
                error: None,
            }
        }
        Ok(Err(err)) => Outcome::down(err.to_string()),
        Err(_elapsed) => Outcome::down("connection timed out".to_owned()),
    }
}

/// Apply every configured header to the request. reqwest *appends* headers, so
/// each distinct header is kept — none overwrites another.
fn with_headers(mut request: RequestBuilder, headers: &HashMap<String, String>) -> RequestBuilder {
    for (name, value) in headers {
        request = request.header(name, value);
    }
    request
}

fn over_threshold(latency_ms: i64, threshold: Option<i64>) -> bool {
    threshold.is_some_and(|limit| latency_ms > limit)
}

fn millis(elapsed: Duration) -> i64 {
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

/// Read a bounded, single-line snippet of the response body (for failure detail).
async fn body_snippet(mut response: reqwest::Response) -> String {
    let mut buf = Vec::new();
    while buf.len() < MAX_BODY_SNIPPET {
        match response.chunk().await {
            Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
            _ => break,
        }
    }
    buf.truncate(MAX_BODY_SNIPPET); // strict byte bound before the lossy decode
    String::from_utf8_lossy(&buf)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_BODY_SNIPPET)
        .collect()
}

/// A concise, URL-free description of a request error. The raw error embeds the
/// target URL (which may carry credentials), so we categorize instead.
fn describe(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "request timed out"
    } else if err.is_connect() {
        "connection failed"
    } else if err.is_redirect() {
        "too many redirects"
    } else if err.is_body() || err.is_decode() {
        "invalid response body"
    } else {
        "request error"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_detects_slow() {
        assert!(over_threshold(900, Some(800)));
        assert!(!over_threshold(700, Some(800)));
        assert!(!over_threshold(900, None));
    }

    #[test]
    fn millis_saturates() {
        assert_eq!(millis(Duration::from_millis(5)), 5);
    }

    #[test]
    fn status_value_mapping() {
        let down = Outcome::down("x".to_owned());
        assert_eq!(down.status_value(), 0);
    }

    #[tokio::test]
    async fn applies_every_configured_header() {
        let client = Client::new();
        let mut headers = HashMap::new();
        headers.insert("Accept".to_owned(), "text/html".to_owned());
        headers.insert("X-Token".to_owned(), "abc".to_owned());

        let request = with_headers(client.get("https://example.com"), &headers);
        let built = request.build().expect("request builds");

        // Both headers survive: appending never overwrites the previous one.
        assert_eq!(built.headers().len(), 2);
        assert_eq!(
            built.headers().get("accept").unwrap().to_str().unwrap(),
            "text/html"
        );
        assert_eq!(
            built.headers().get("x-token").unwrap().to_str().unwrap(),
            "abc"
        );
    }
}
