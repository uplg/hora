//! Probing logic: turn a monitor into a single [`Outcome`].

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use reqwest::{Client, RequestBuilder};
use socket2::Type;
use surge_ping::{
    Client as PingClient, Config as PingConfig, ICMP, PingIdentifier, PingSequence, SurgeError,
};
use tokio::net::TcpStream;

use crate::config::{Kind, Monitor, Secret};

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

    pub(crate) fn down(error: String) -> Self {
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
        Kind::Icmp => icmp(monitor).await,
        // Push monitors are evaluated from stored heartbeats by the scheduler,
        // never actively probed; this arm is unreachable in practice.
        Kind::Push => Outcome::down("push monitor has no active probe".to_owned()),
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
            let status_ok = match monitor.expected_status {
                Some(expected) => code == expected,
                None => response.status().is_success(),
            };
            // Read the body only when we need it: to detail a failure, or to run
            // a keyword/JSON assertion. Assertions get a larger budget.
            let assertions = monitor.keyword.is_some() || monitor.json_query.is_some();
            let body = if !status_ok || assertions {
                let cap = if assertions {
                    monitor.assertion_body_cap()
                } else {
                    MAX_BODY_SNIPPET
                };
                read_body(response, cap).await
            } else {
                Vec::new()
            };

            let (up, error) = if !status_ok {
                let snippet = snippet(&body);
                let detail = if snippet.is_empty() {
                    format!("HTTP {code}")
                } else {
                    format!("HTTP {code}: {snippet}")
                };
                (false, Some(detail))
            } else if let Some(failure) = check_assertions(monitor, &body) {
                (false, Some(failure))
            } else {
                (true, None)
            };

            let degraded = up && over_threshold(latency, monitor.degraded_over_ms);
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

/// Run the configured keyword/JSON assertions against the body; the first that
/// fails returns its reason. `None` means every assertion passed.
fn check_assertions(monitor: &Monitor, body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    if let Some(keyword) = &monitor.keyword {
        let found = text.contains(keyword.as_str());
        if found == monitor.keyword_invert {
            return Some(if monitor.keyword_invert {
                format!("keyword present: {keyword}")
            } else {
                format!("keyword missing: {keyword}")
            });
        }
    }
    if let Some(query) = &monitor.json_query {
        return check_json(query, monitor.json_expected.as_deref(), &text);
    }
    None
}

/// Evaluate a `JSONPath` against the body. Returns a failure reason or `None`.
fn check_json(query: &str, expected: Option<&str>, body: &str) -> Option<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Some("response is not valid JSON".to_owned());
    };
    // The query is validated at config load, so this should not fail.
    let Ok(path) = serde_json_path::JsonPath::parse(query) else {
        return Some(format!("invalid JSON query: {query}"));
    };
    let nodes = path.query(&value).all();
    match expected {
        None => nodes
            .is_empty()
            .then(|| format!("JSON query matched nothing: {query}")),
        Some(expected) => {
            let matched = nodes.iter().any(|node| json_value_eq(node, expected));
            (!matched).then(|| format!("JSON query {query} != {expected}"))
        }
    }
}

/// Compare a queried JSON node to an expected string: strings match their inner
/// value, everything else matches its compact JSON text (`true`, `42`, …).
fn json_value_eq(value: &serde_json::Value, expected: &str) -> bool {
    match value {
        serde_json::Value::String(text) => text == expected,
        other => {
            let rendered = other.to_string();
            rendered == expected
        }
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

/// ICMP echo (ping). Uses a per-probe unprivileged datagram socket (no
/// `CAP_NET_RAW`), so it works in rootless Docker; one socket per probe avoids
/// datagram identifier collisions between concurrent monitors. The address family
/// (IPv4/IPv6) follows the resolved address.
async fn icmp(monitor: &Monitor) -> Outcome {
    let Some(addr) = resolve(&monitor.target).await else {
        return Outcome::down("could not resolve host".to_owned());
    };

    let kind = if addr.is_ipv4() { ICMP::V4 } else { ICMP::V6 };
    let config = PingConfig::builder()
        .kind(kind)
        .sock_type_hint(Type::DGRAM)
        .build();
    let client = match PingClient::new(&config) {
        Ok(client) => client,
        // Usually a missing privilege: no unprivileged-ping permission and no
        // CAP_NET_RAW. Surface it clearly rather than as a generic failure.
        Err(err) => {
            return Outcome::down(format!(
                "icmp socket unavailable ({err}); needs net.ipv4.ping_group_range or CAP_NET_RAW"
            ));
        }
    };

    let mut pinger = client.pinger(addr, PingIdentifier(0)).await;
    pinger.timeout(monitor.timeout());
    match pinger.ping(PingSequence(0), &[0u8; 16]).await {
        Ok((_packet, rtt)) => {
            let latency = millis(rtt);
            Outcome {
                up: true,
                degraded: over_threshold(latency, monitor.degraded_over_ms),
                latency_ms: Some(latency),
                status_code: None,
                error: None,
            }
        }
        Err(SurgeError::Timeout { .. }) => Outcome::down("request timed out".to_owned()),
        Err(err) => Outcome::down(format!("icmp error: {err}")),
    }
}

/// Resolve an ICMP target to a single IP: an IP literal is used directly,
/// otherwise DNS is consulted and the first address is taken (so an IPv4-only or
/// IPv6-only host resolves to the family it actually has).
async fn resolve(target: &str) -> Option<IpAddr> {
    if let Ok(ip) = target.parse::<IpAddr>() {
        return Some(ip);
    }
    tokio::net::lookup_host((target, 0u16))
        .await
        .ok()?
        .next()
        .map(|addr| addr.ip())
}

/// Apply every configured header to the request. reqwest *appends* headers, so
/// each distinct header is kept - none overwrites another.
fn with_headers(mut request: RequestBuilder, headers: &HashMap<String, Secret>) -> RequestBuilder {
    for (name, value) in headers {
        request = request.header(name, value.as_ref());
    }
    request
}

fn over_threshold(latency_ms: i64, threshold: Option<i64>) -> bool {
    threshold.is_some_and(|limit| latency_ms > limit)
}

fn millis(elapsed: Duration) -> i64 {
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

/// Read the response body up to `cap` bytes (so a huge body can't exhaust memory).
async fn read_body(mut response: reqwest::Response, cap: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    while buf.len() < cap {
        match response.chunk().await {
            // Copy at most the remaining budget so one huge chunk can't blow the bound.
            Ok(Some(chunk)) => {
                let take = (cap - buf.len()).min(chunk.len());
                buf.extend_from_slice(&chunk[..take]);
            }
            _ => break,
        }
    }
    buf
}

/// Collapse a byte body into a bounded, single-line snippet for failure detail.
fn snippet(body: &[u8]) -> String {
    // Fold the whitespace-separated words straight into one string, so there is
    // no intermediate `Vec<&str>` just to `join` it.
    String::from_utf8_lossy(body)
        .split_whitespace()
        .fold(String::new(), |mut acc, word| {
            if !acc.is_empty() {
                acc.push(' ');
            }
            acc.push_str(word);
            acc
        })
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

    fn http_monitor() -> Monitor {
        Monitor {
            id: "m".to_owned(),
            name: "M".to_owned(),
            kind: Kind::Http,
            target: "https://example.com".to_owned(),
            interval_secs: 60,
            timeout_secs: 10,
            expected_status: None,
            degraded_over_ms: None,
            slo_latency_ms: None,
            headers: HashMap::new(),
            keyword: None,
            keyword_invert: false,
            json_query: None,
            json_expected: None,
            max_body_kb: None,
            notify: None,
            proxy: None,
            push_token: None,
            check_cert: None,
            retention_days: None,
            group: None,
            depends_on: None,
        }
    }

    #[test]
    fn keyword_assertion() {
        let mut monitor = http_monitor();
        monitor.keyword = Some("OK".to_owned());
        assert!(check_assertions(&monitor, b"all OK here").is_none());
        assert!(check_assertions(&monitor, b"failure").is_some());

        monitor.keyword_invert = true;
        assert!(check_assertions(&monitor, b"failure").is_none());
        assert!(check_assertions(&monitor, b"all OK").is_some());
    }

    #[test]
    fn json_query_assertion() {
        // Expected value, string and non-string.
        assert!(check_json("$.status", Some("ok"), r#"{"status":"ok"}"#).is_none());
        assert!(check_json("$.status", Some("ok"), r#"{"status":"bad"}"#).is_some());
        assert!(check_json("$.healthy", Some("true"), r#"{"healthy":true}"#).is_none());
        // No expected value: the query just has to match something.
        assert!(check_json("$.data", None, r#"{"data":[1,2]}"#).is_none());
        assert!(check_json("$.missing", None, r#"{"data":1}"#).is_some());
        // Malformed JSON fails the assertion.
        assert!(check_json("$.x", Some("1"), "not json").is_some());
    }

    #[tokio::test]
    async fn applies_every_configured_header() {
        let client = Client::new();
        let mut headers = HashMap::new();
        headers.insert("Accept".to_owned(), Secret("text/html".to_owned()));
        headers.insert("X-Token".to_owned(), Secret("abc".to_owned()));

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

    #[tokio::test]
    async fn resolve_parses_ip_literals() {
        // IP literals short-circuit before any DNS lookup (no network in tests).
        assert_eq!(
            resolve("127.0.0.1").await,
            Some("127.0.0.1".parse().unwrap())
        );
        assert_eq!(
            resolve("2606:4700:4700::1111").await,
            Some("2606:4700:4700::1111".parse().unwrap())
        );
    }
}
