use std::collections::HashMap;
use std::net::SocketAddr;

use super::dns::{record_type, resolver_for};
use super::family::combine;
use super::http::{check_assertions, check_json, check_number, same_origin, with_headers};
use super::snapshot::{
    MAX_SNAPSHOT_BODY_CHARS, MAX_SNAPSHOT_HEADER_CHARS, MAX_SNAPSHOT_HEADERS, render_head,
    render_snapshot,
};
use super::*;
use crate::config::{
    Compiled, DnsRecord, ExpectedStatus, HttpMethod, HttpSpec, JsonAssert, KeywordAssert,
    MonitorKind, NumberAssert, Parsed, Secret,
};
use crate::http::is_downgrade;

#[test]
fn public_reason_collapses_detail() {
    // Remote-controlled or config-revealing detail is dropped.
    assert_eq!(
        public_reason(None, "HTTP 500: secret stack trace"),
        "HTTP 500"
    );
    assert_eq!(
        public_reason(None, "expected 1.2.3.4, got 6.6.6.6"),
        "unexpected DNS answer"
    );
    assert_eq!(
        public_reason(None, "keyword missing: internal-marker"),
        "content check failed"
    );
    assert_eq!(
        public_reason(None, "JSON query $.status != ok"),
        "content check failed"
    );
    assert_eq!(
        public_reason(None, "number 0 below min 1"),
        "content check failed"
    );
    assert_eq!(
        public_reason(None, "number_regex matched nothing: ships (\\d+)"),
        "content check failed"
    );
    assert_eq!(
        public_reason(
            None,
            "missed scheduled heartbeat (was due 03:00 UTC + 30 min grace)"
        ),
        "missed scheduled heartbeat"
    );
    assert_eq!(
        public_reason(None, "DNS lookup failed: proto error: io error"),
        "DNS lookup failed"
    );
    // Unknown text (raw TCP errors, push messages) falls back to generic.
    assert_eq!(
        public_reason(None, "Connection refused (os error 61)"),
        "check failed"
    );
    // Plugin output (device names, container ids) collapses to a category.
    assert_eq!(
        public_reason(None, "plugin timed out after 30 s"),
        "plugin check failed"
    );
    assert_eq!(
        public_reason(None, "plugin reported critical (exit 2)"),
        "plugin check failed"
    );
    // Safe statics pass through verbatim.
    assert_eq!(public_reason(None, "HTTP 503"), "HTTP 503");
    assert_eq!(
        public_reason(None, "request timed out"),
        "request timed out"
    );
    assert_eq!(
        public_reason(None, "no A records found"),
        "no A records found"
    );
    // A push msg crafted to look like a safe shape must not pass through:
    // the HTTP branch is anchored to a numeric status, the DNS one to the
    // record types the probe queries.
    assert_eq!(
        public_reason(None, "HTTP looks legit but is a push msg"),
        "check failed"
    );
    assert_eq!(public_reason(None, "HTTP 99999: nope"), "check failed");
    assert_eq!(
        public_reason(None, "no big deal, just injected records found"),
        "check failed"
    );
}

#[test]
fn a_stored_kind_decides_the_public_reason_whatever_the_wording() {
    // The job's own message is never shown, even when it reads like a safe
    // reason or a status line.
    assert_eq!(
        public_reason(Some(FailureKind::Pushed), "HTTP 503"),
        "check failed"
    );
    assert_eq!(
        public_reason(Some(FailureKind::Pushed), "request timed out"),
        "check failed"
    );
    // Plugin output becomes the category whatever it starts with.
    assert_eq!(
        public_reason(Some(FailureKind::Plugin), "/dev/sda1 91% full"),
        "plugin check failed"
    );
    assert_eq!(
        public_reason(Some(FailureKind::Http), "HTTP 500: secret stack trace"),
        "HTTP 500"
    );
    assert_eq!(
        public_reason(Some(FailureKind::NoRecords), "no AAAA records found"),
        "no AAAA records found"
    );
    assert_eq!(
        public_reason(
            Some(FailureKind::NoHeartbeatYet),
            "no heartbeat received yet"
        ),
        "no heartbeat received yet"
    );
    // Codes round-trip; an unknown one is not a kind.
    for kind in FailureKind::ALL {
        assert_eq!(FailureKind::from_code(kind.code()), Some(kind));
    }
    assert_eq!(FailureKind::from_code("solar_flare"), None);
}

#[test]
fn the_kinds_keep_the_public_words_of_the_old_classifier() {
    // What the probes write, kind and detail: the public page must read the
    // same with the kind as it did from the wording alone.
    let produced = [
        (FailureKind::Timeout, "request timed out"),
        (FailureKind::ConnectionFailed, "connection failed"),
        (FailureKind::ConnectionTimedOut, "connection timed out"),
        (FailureKind::TooManyRedirects, "too many redirects"),
        (FailureKind::InvalidBody, "invalid response body"),
        (FailureKind::RequestError, "request error"),
        (FailureKind::Unresolvable, "could not resolve host"),
        (FailureKind::Http, "HTTP 502: bad gateway"),
        (FailureKind::Http, "HTTP 404"),
        (FailureKind::Content, "keyword missing: marker"),
        (FailureKind::Content, "keyword present: marker"),
        (FailureKind::Content, "JSON query $.ok != true"),
        (FailureKind::Content, "response is not valid JSON"),
        (FailureKind::Content, "number 3 above max 2"),
        (FailureKind::DnsMismatch, "expected 1.2.3.4, got 6.6.6.6"),
        (FailureKind::DnsFailed, "DNS lookup failed: no connections"),
        (FailureKind::DnsFailed, "resolver setup failed: bad address"),
        (FailureKind::DnsTimeout, "DNS lookup timed out"),
        (FailureKind::NoRecords, "no MX records found"),
        (FailureKind::Ping, "icmp error: host unreachable"),
        (FailureKind::Ping, "icmp socket unavailable (denied)"),
        (FailureKind::Plugin, "plugin timed out after 30 s"),
        (FailureKind::Plugin, "exec wait failed: interrupted"),
        (FailureKind::Ipv4Failing, "IPv4 failing: timeout (IPv6 ok)"),
        (FailureKind::Ipv6Failing, "IPv6 failing: refused (IPv4 ok)"),
        (
            FailureKind::BothFamiliesFailing,
            "IPv4 and IPv6 failing: a; b",
        ),
        (FailureKind::MissingHeartbeat, "missing heartbeat"),
        (
            FailureKind::MissedSchedule,
            "missed scheduled heartbeat (was due 03:00 UTC + 30 min grace)",
        ),
        (FailureKind::Pushed, "backup failed"),
        (FailureKind::Other, "Connection refused (os error 61)"),
        (FailureKind::Other, "no IPv6 address for host"),
    ];
    for (kind, detail) in produced {
        assert_eq!(
            public_reason(Some(kind), detail),
            public_reason(None, detail),
            "{kind:?}: {detail}"
        );
    }
}

#[test]
fn same_origin_governs_header_forwarding() {
    let parse = |u: &str| reqwest::Url::parse(u).unwrap();
    let target = parse("https://api.example.com/v1");

    // Same host, scheme and port (path differs): headers stay attached.
    assert!(same_origin(
        &target,
        &parse("https://api.example.com/login")
    ));
    // Different host, scheme or port: headers are dropped.
    assert!(!same_origin(&target, &parse("https://evil.example.com/v1")));
    assert!(!same_origin(&target, &parse("http://api.example.com/v1")));
    assert!(!same_origin(
        &target,
        &parse("https://api.example.com:8443/v1")
    ));
}

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
fn a_down_outcome_is_stored_as_down() {
    let down = Outcome::down(FailureKind::Other, "x".to_owned());
    assert_eq!(down.status, CheckStatus::Down);
    assert!(!down.is_up() && !down.is_degraded());
}

fn number(pattern: &str, min: Option<i64>, max: Option<i64>, body: &str) -> Option<String> {
    let regex = Compiled::parse(pattern).unwrap();
    check_number(&NumberAssert { regex, min, max }, body)
}

fn json(query: &str, expected: Option<&str>, body: &str) -> Option<String> {
    let query = Compiled::parse(query).unwrap();
    let expected = expected.map(str::to_owned);
    check_json(&JsonAssert { query, expected }, body)
}

#[test]
fn snapshot_redacts_credential_headers() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("set-cookie", "session=s3cr3t; HttpOnly".parse().unwrap());
    headers.insert("www-authenticate", "Bearer realm=x".parse().unwrap());
    headers.insert("content-type", "text/html".parse().unwrap());
    let head = render_head(
        reqwest::Version::HTTP_11,
        reqwest::StatusCode::UNAUTHORIZED,
        &headers,
    );
    assert!(!head.contains("s3cr3t"), "{head}");
    assert!(head.contains("set-cookie: <redacted>"), "{head}");
    assert!(head.contains("www-authenticate: <redacted>"), "{head}");
    // Harmless headers keep their value.
    assert!(head.contains("content-type: text/html"), "{head}");
}

#[test]
fn dns_record_types_map_and_stay_public() {
    // Every configurable type maps onto the hickory type of the same
    // name, so the "no X records found" reason stays recognized.
    for record in DnsRecord::ALL {
        assert_eq!(record_type(record).to_string(), record.as_str());
        let reason = format!("no {} records found", record_type(record));
        assert_eq!(public_reason(None, &reason), reason);
    }
}

#[tokio::test]
async fn custom_resolver_takes_a_parsed_address() {
    // IPv6 resolvers work now that the address is parsed (brackets and
    // all) at load rather than split on the last colon here.
    for raw in ["9.9.9.9:53", "[2620:fe::fe]:53"] {
        let addr: SocketAddr = Parsed::new(raw).get().copied().expect(raw);
        assert!(resolver_for(Some(addr)).is_ok(), "{raw}");
    }
}

/// A one-shot HTTP server answering every connection with `response`;
/// returns its base URL and the number of requests served so far.
async fn serve(response: &'static str) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    (format!("http://{addr}"), hits)
}

/// A keep-alive HTTP server whose connections go mute after one answer: the
/// first request on a connection gets a 200, any later one on the same
/// connection is read and never answered. Returns its base URL and the number
/// of connections accepted so far.
async fn serve_once_per_connection() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = connections.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                if !matches!(socket.read(&mut buf).await, Ok(n) if n > 0) {
                    return;
                }
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await;
                // Stay open and swallow whatever comes next, answering nothing.
                while matches!(socket.read(&mut buf).await, Ok(n) if n > 0) {}
            });
        }
    });
    (format!("http://{addr}"), connections)
}

#[tokio::test]
async fn a_probe_never_reuses_a_connection() {
    // A connection that stops answering without closing must not poison the
    // probes that follow: each one opens its own.
    let (url, connections) = serve_once_per_connection().await;
    let client = crate::http::probe_client(None).unwrap();
    let mut monitor = http_monitor(&url);
    monitor.timeout_secs = 2;
    for round in 1..=3 {
        let outcome = http(&client, &monitor, spec(&monitor)).await;
        assert!(outcome.is_up(), "probe {round}: {:?}", outcome.error);
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), round);
    }
}

#[tokio::test]
async fn expected_redirect_is_not_followed() {
    let (url, hits) = serve(
            "HTTP/1.1 301 Moved Permanently\r\nLocation: /elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
    let client = crate::http::probe_client(None).unwrap();
    let mut monitor = http_monitor(&url);
    spec_mut(&mut monitor).expected_status = Some(ExpectedStatus::code(301));
    let outcome = http(&client, &monitor, spec(&monitor)).await;
    assert!(outcome.is_up(), "{:?}", outcome.error);
    assert_eq!(outcome.status_code, Some(301));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Without a 3xx expectation the redirect is chased (here into a loop
    // back to the same server, until the hop budget runs out).
    spec_mut(&mut monitor).expected_status = None;
    let outcome = http(&client, &monitor, spec(&monitor)).await;
    assert_eq!(outcome.error.as_deref(), Some("too many redirects"));
}

#[tokio::test]
async fn a_listed_status_is_up() {
    let (url, _hits) =
        serve("HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
    let client = crate::http::probe_client(None).unwrap();
    let mut monitor = http_monitor(&url);
    // Unset, only a 2xx is up.
    let outcome = http(&client, &monitor, spec(&monitor)).await;
    assert_eq!(outcome.error.as_deref(), Some("HTTP 401"));

    // An API behind authentication answers 401 when it is healthy.
    for accepted in ["200,401", "4xx", "2xx,401"] {
        spec_mut(&mut monitor).expected_status =
            Some(ExpectedStatus::parse_list(accepted).unwrap());
        let outcome = http(&client, &monitor, spec(&monitor)).await;
        assert!(outcome.is_up(), "{accepted}: {:?}", outcome.error);
        assert_eq!(outcome.status_code, Some(401));
    }
    // A list is still a list: what it leaves out is down.
    spec_mut(&mut monitor).expected_status = Some(ExpectedStatus::parse_list("200,403").unwrap());
    let outcome = http(&client, &monitor, spec(&monitor)).await;
    assert_eq!(outcome.error.as_deref(), Some("HTTP 401"));
}

#[tokio::test]
async fn a_list_holding_a_redirect_judges_the_first_answer() {
    let (url, hits) = serve(
        "HTTP/1.1 301 Moved Permanently\r\nLocation: /elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;
    let client = crate::http::probe_client(None).unwrap();
    let mut monitor = http_monitor(&url);
    // Followed, the 301 would never be the answer judged and could not count.
    spec_mut(&mut monitor).expected_status = Some(ExpectedStatus::parse_list("200,301").unwrap());
    let outcome = http(&client, &monitor, spec(&monitor)).await;
    assert!(outcome.is_up(), "{:?}", outcome.error);
    assert_eq!(outcome.status_code, Some(301));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// A server that sends `/` to `/next` and answers 200 anywhere else, without
/// a body; returns its base URL and the request lines it received.
async fn serve_recording() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let read = socket.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..read]).into_owned();
            let line = request.lines().next().unwrap_or_default().to_owned();
            let response = if line.contains(" / ") {
                "HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else {
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            };
            record.lock().unwrap().push(line);
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    (format!("http://{addr}"), seen)
}

#[tokio::test]
async fn a_head_monitor_sends_head_on_every_hop() {
    let (url, seen) = serve_recording().await;
    let client = crate::http::probe_client(None).unwrap();
    let mut monitor = http_monitor(&url);
    let outcome = http(&client, &monitor, spec(&monitor)).await;
    assert!(outcome.is_up(), "{:?}", outcome.error);
    assert_eq!(
        std::mem::take(&mut *seen.lock().unwrap()),
        ["GET / HTTP/1.1", "GET /next HTTP/1.1"]
    );

    spec_mut(&mut monitor).method = HttpMethod::Head;
    let outcome = http(&client, &monitor, spec(&monitor)).await;
    assert!(outcome.is_up(), "{:?}", outcome.error);
    assert_eq!(outcome.status_code, Some(200));
    // The redirect does not turn it into a GET.
    assert_eq!(
        *seen.lock().unwrap(),
        ["HEAD / HTTP/1.1", "HEAD /next HTTP/1.1"]
    );
}

#[tokio::test]
async fn truncated_body_is_a_body_error_not_an_assertion_failure() {
    // Announces 100 bytes, sends 7, closes: a transport failure mid-body.
    let (url, _hits) =
        serve("HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\npartial").await;
    let client = crate::http::probe_client(None).unwrap();
    let mut monitor = http_monitor(&url);
    spec_mut(&mut monitor).keyword = Some(KeywordAssert {
        text: "complete".to_owned(),
        invert: false,
    });
    let outcome = http(&client, &monitor, spec(&monitor)).await;
    assert!(!outcome.is_up());
    assert_eq!(outcome.error.as_deref(), Some("invalid response body"));
}

#[tokio::test]
async fn an_assertion_on_a_cut_body_says_so() {
    let body: &'static str = Box::leak(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: 2048\r\nConnection: close\r\n\r\n{}",
            "x".repeat(2048)
        )
        .into_boxed_str(),
    );
    let (url, _hits) = serve(body).await;
    let client = crate::http::probe_client(None).unwrap();
    let mut monitor = http_monitor(&url);
    spec_mut(&mut monitor).keyword = Some(KeywordAssert {
        text: "operational".to_owned(),
        invert: false,
    });
    spec_mut(&mut monitor).max_body_kb = Some(1);
    let outcome = http(&client, &monitor, spec(&monitor)).await;
    assert_eq!(
        outcome.error.as_deref(),
        Some("keyword missing: operational (body cut at 1.0 KiB)")
    );
}

#[tokio::test]
async fn compressed_bodies_are_decoded_before_the_assertions() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    // gzip("all systems operational"), sent whatever the request accepted.
    const GZIP: &[u8] = b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x02\xff\x4b\xcc\xc9\x51\x28\xae\x2c\
        \x2e\x49\xcd\x2d\x56\xc8\x2f\x48\x2d\x4a\x2c\xc9\xcc\xcf\x4b\xcc\x01\x00\x18\x3a\x50\x79\
        \x17\x00\x00\x00";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            GZIP.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(GZIP).await.unwrap();
    });
    let client = crate::http::probe_client(None).unwrap();
    let mut monitor = http_monitor(&format!("http://{addr}/"));
    spec_mut(&mut monitor).keyword = Some(KeywordAssert {
        text: "operational".to_owned(),
        invert: false,
    });
    let outcome = http(&client, &monitor, spec(&monitor)).await;
    assert!(outcome.is_up(), "{:?}", outcome.error);
}

#[tokio::test]
async fn transport_failures_name_their_cause() {
    let client = crate::http::probe_client(None).unwrap();
    // Plain HTTP where TLS was expected: rustls names the problem.
    let (url, _hits) =
        serve("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await;
    let monitor = http_monitor(&url.replace("http://", "https://"));
    let outcome = http(&client, &monitor, spec(&monitor)).await;
    let error = outcome.error.unwrap();
    assert!(error.starts_with("connection failed: TLS "), "{error}");

    // A closed port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let monitor = http_monitor(&format!("http://{addr}/"));
    let outcome = http(&client, &monitor, spec(&monitor)).await;
    assert_eq!(
        outcome.error.as_deref(),
        Some("connection failed: connection refused")
    );
}

#[tokio::test]
async fn a_proxy_refusing_us_is_blamed_not_the_target() {
    let (proxy, _hits) = serve(
        "HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;
    let client = crate::http::probe_client(Some(&proxy)).unwrap();
    // Plain http goes through the proxy as a request: the 407 is an answer.
    let mut monitor = http_monitor("http://target.invalid/");
    spec_mut(&mut monitor).proxy = Some(proxy.clone());
    let outcome = http(&client, &monitor, spec(&monitor)).await;
    assert_eq!(
        outcome.error.as_deref(),
        Some("proxy failed: HTTP 407 Proxy Authentication Required")
    );
    assert_eq!(outcome.reason, Some(FailureKind::ConnectionFailed));
    assert_eq!(outcome.status_code, None);

    // https tunnels through CONNECT: the 407 is a tunnel error.
    let mut monitor = http_monitor("https://target.invalid/");
    spec_mut(&mut monitor).proxy = Some(proxy);
    let outcome = http(&client, &monitor, spec(&monitor)).await;
    assert_eq!(
        outcome.error.as_deref(),
        Some("proxy failed: tunnel error: proxy authorization required")
    );
}

/// A public target answering with a redirect to an internal address is a
/// down with its own reason, and the internal service is never asked. The
/// "public target" is reached through a proxy (a local server answering for
/// it), which is the only way to have one in a test.
#[tokio::test]
async fn a_public_target_redirecting_inwards_is_refused() {
    let (inside, inside_hits) =
        serve("HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nsecret").await;
    let redirect: &'static str = Box::leak(
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {inside}/admin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_boxed_str(),
    );
    let (proxy, _hits) = serve(redirect).await;
    let client = crate::http::probe_client(Some(&proxy)).unwrap();
    let mut monitor = http_monitor("http://1.1.1.1/");
    spec_mut(&mut monitor).proxy = Some(proxy.clone());
    let outcome = http(&client, &monitor, spec(&monitor)).await;
    assert_eq!(
        outcome.error.as_deref(),
        Some("redirected to a private address")
    );
    assert_eq!(outcome.reason, Some(FailureKind::PrivateRedirect));
    assert_eq!(outcome.snapshot, None);
    assert_eq!(inside_hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(
        public_reason(outcome.reason, "redirected to a private address"),
        "redirected to a private address"
    );

    // An internal target may redirect to another internal address, as ever.
    let (front, _hits) = serve(redirect).await;
    let client = crate::http::probe_client(None).unwrap();
    let monitor = http_monitor(&front.replace("127.0.0.1", "localhost"));
    let outcome = http(&client, &monitor, spec(&monitor)).await;
    assert!(outcome.is_up(), "{:?}", outcome.error);
    assert_eq!(inside_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn only_https_to_http_is_a_downgrade() {
    let parse = |u: &str| reqwest::Url::parse(u).unwrap();
    assert!(is_downgrade(
        &parse("https://example.com/"),
        &parse("http://example.com/login")
    ));
    assert!(!is_downgrade(
        &parse("http://example.com/"),
        &parse("https://example.com/")
    ));
    assert!(!is_downgrade(
        &parse("https://example.com/"),
        &parse("https://www.example.com/")
    ));
    assert_eq!(
        public_reason(
            Some(FailureKind::InsecureRedirect),
            "redirected to plain http (http://x)"
        ),
        "redirected to plain http"
    );
}

fn http_monitor(target: &str) -> Monitor {
    Monitor::ad_hoc(crate::config::Kind::Http, target.to_owned()).unwrap()
}

fn spec(monitor: &Monitor) -> &HttpSpec {
    match &monitor.spec {
        MonitorKind::Http(spec) => spec,
        other => panic!("not an http monitor: {other:?}"),
    }
}

fn spec_mut(monitor: &mut Monitor) -> &mut HttpSpec {
    match &mut monitor.spec {
        MonitorKind::Http(spec) => spec,
        other => panic!("not an http monitor: {other:?}"),
    }
}

#[test]
fn keyword_assertion() {
    let mut monitor = http_monitor("https://example.com");
    spec_mut(&mut monitor).keyword = Some(KeywordAssert {
        text: "OK".to_owned(),
        invert: false,
    });
    assert!(check_assertions(spec(&monitor), b"all OK here").is_none());
    assert!(check_assertions(spec(&monitor), b"failure").is_some());

    if let Some(keyword) = &mut spec_mut(&mut monitor).keyword {
        keyword.invert = true;
    }
    assert!(check_assertions(spec(&monitor), b"failure").is_none());
    assert!(check_assertions(spec(&monitor), b"all OK").is_some());
}

#[test]
fn number_assertion() {
    let body = r#"<div class="stationRealtimeShipUnique">11</div>"#;
    let pattern = r#"stationRealtimeShipUnique">(\d+)<"#;
    // In bounds, at the bound, and out of bounds on both sides.
    assert!(number(pattern, Some(1), None, body).is_none());
    assert!(number(pattern, Some(11), Some(11), body).is_none());
    assert_eq!(
        number(pattern, Some(12), None, body).as_deref(),
        Some("number 11 below min 12")
    );
    assert_eq!(
        number(pattern, None, Some(10), body).as_deref(),
        Some("number 11 above max 10")
    );
    // No bounds: the extraction itself is the assertion.
    assert!(number(pattern, None, None, body).is_none());
    assert!(number(pattern, None, None, "<html>maintenance</html>").is_some());
    // A group-less pattern falls back to the whole match; decimals parse.
    assert!(number(r"[0-9.]+", Some(99), None, "uptime 99.5 %").is_none());
    // A match that is not a number fails rather than passes vacuously.
    assert!(number("<(div)>", Some(1), None, "<div>7</div>").is_some());
}

#[test]
fn json_query_assertion() {
    // Expected value, string and non-string.
    assert!(json("$.status", Some("ok"), r#"{"status":"ok"}"#).is_none());
    assert!(json("$.status", Some("ok"), r#"{"status":"bad"}"#).is_some());
    assert!(json("$.healthy", Some("true"), r#"{"healthy":true}"#).is_none());
    // No expected value: the query just has to match something.
    assert!(json("$.data", None, r#"{"data":[1,2]}"#).is_none());
    assert!(json("$.missing", None, r#"{"data":1}"#).is_some());
    // Malformed JSON fails the assertion.
    assert!(json("$.x", Some("1"), "not json").is_some());
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
        resolve("127.0.0.1", None).await,
        Some("127.0.0.1".parse().unwrap())
    );
    assert_eq!(
        resolve("2606:4700:4700::1111", None).await,
        Some("2606:4700:4700::1111".parse().unwrap())
    );
    // A family filter rejects a literal of the other family.
    assert_eq!(
        resolve("127.0.0.1", Some(Family::V4)).await,
        Some("127.0.0.1".parse().unwrap())
    );
    assert_eq!(resolve("127.0.0.1", Some(Family::V6)).await, None);
}

fn outcome(up: bool, latency_ms: Option<i64>, error: Option<&str>) -> Outcome {
    Outcome {
        status: if up {
            CheckStatus::Up
        } else {
            CheckStatus::Down
        },
        latency_ms,
        status_code: None,
        error: error.map(str::to_owned),
        reason: error.map(|_| FailureKind::Other),
        snapshot: None,
    }
}

#[test]
fn combine_requires_both_families() {
    // Both up: up, with the worst latency.
    let both = combine(
        &outcome(true, Some(20), None),
        &outcome(true, Some(35), None),
    );
    assert!(both.is_up());
    assert_eq!(both.latency_ms, Some(35));
    assert_eq!(both.error, None);

    // One family broken: down, the reason names it, the latency is the
    // surviving path's.
    let v6_dead = combine(
        &outcome(true, Some(20), None),
        &outcome(false, None, Some("connection timed out")),
    );
    assert!(!v6_dead.is_up());
    assert_eq!(v6_dead.latency_ms, Some(20));
    assert_eq!(
        v6_dead.error.as_deref(),
        Some("IPv6 failing: connection timed out (IPv4 ok)")
    );

    let v4_dead = combine(
        &outcome(false, None, Some("connection refused")),
        &outcome(true, Some(12), None),
    );
    assert!(!v4_dead.is_up());
    assert_eq!(
        v4_dead.error.as_deref(),
        Some("IPv4 failing: connection refused (IPv6 ok)")
    );

    // Both down: both reasons, no latency.
    let dark = combine(
        &outcome(false, None, Some("timeout")),
        &outcome(false, None, Some("refused")),
    );
    assert!(!dark.is_up());
    assert_eq!(dark.latency_ms, None);
    assert_eq!(
        dark.error.as_deref(),
        Some("IPv4 and IPv6 failing: timeout; refused")
    );
}

#[test]
fn combine_keeps_either_degraded() {
    let mut slow_v6 = outcome(true, Some(900), None);
    slow_v6.status = CheckStatus::Degraded;
    let both = combine(&outcome(true, Some(20), None), &slow_v6);
    assert!(both.is_up());
    assert!(both.is_degraded());
}

#[test]
fn snapshot_renders_status_headers_and_body_bounded() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("content-type", "text/html".parse().unwrap());
    headers.insert("retry-after", "120".parse().unwrap());

    let head = render_head(
        reqwest::Version::HTTP_2,
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        &headers,
    );
    assert_eq!(
        head,
        "HTTP/2.0 503 Service Unavailable\ncontent-type: text/html\nretry-after: 120"
    );

    // Body appended after a blank line; an empty body leaves the head alone.
    let full = render_snapshot(&head, b"<html>maintenance</html>");
    assert_eq!(full, format!("{head}\n\n<html>maintenance</html>"));
    assert_eq!(render_snapshot(&head, b"  \n "), head);

    // A hostile response can't bloat the record: the body is clipped to
    // the cap, oversized header values per line, excess headers counted.
    let huge = vec![b'x'; 100_000];
    let clipped = render_snapshot(&head, &huge);
    assert_eq!(
        clipped.len(),
        head.len() + 2 + MAX_SNAPSHOT_BODY_CHARS,
        "body bounded"
    );
    let mut many = reqwest::header::HeaderMap::new();
    for i in 0..30 {
        many.append(
            "x-filler",
            format!("{i}-{}", "v".repeat(500)).parse().unwrap(),
        );
    }
    let head = render_head(
        reqwest::Version::HTTP_11,
        reqwest::StatusCode::BAD_GATEWAY,
        &many,
    );
    let lines: Vec<&str> = head.lines().collect();
    assert_eq!(lines.len(), 1 + MAX_SNAPSHOT_HEADERS + 1);
    assert_eq!(lines[lines.len() - 1], "(6 more headers)");
    assert!(lines[1].len() <= MAX_SNAPSHOT_HEADER_CHARS);
}

#[test]
fn public_reason_collapses_dual_stack_detail() {
    assert_eq!(
        public_reason(
            None,
            "IPv6 failing: Connection refused (os error 61) (IPv4 ok)"
        ),
        "IPv6 failing (IPv4 ok)"
    );
    assert_eq!(
        public_reason(None, "IPv4 failing: no IPv4 address for host (IPv6 ok)"),
        "IPv4 failing (IPv6 ok)"
    );
    assert_eq!(
        public_reason(None, "IPv4 and IPv6 failing: timeout; refused"),
        "IPv4 and IPv6 failing"
    );
}
