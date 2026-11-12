//! HTTP(S) checks: status, redirects, latency and body assertions.

use super::FailureKind;
use crate::status::CheckStatus;
use std::collections::HashMap;
use std::time::Instant;

use reqwest::{Client, RequestBuilder};

use super::snapshot::{MAX_SNAPSHOT_BODY_CHARS, render_snapshot, snapshot_head, snippet};
use super::{Outcome, millis, over_threshold};
use crate::config::{HttpSpec, JsonAssert, Monitor, NumberAssert, Secret};
use crate::http::{RedirectGuard, is_downgrade};

pub(super) async fn http(client: &Client, monitor: &Monitor, spec: &HttpSpec) -> Outcome {
    let start = Instant::now();
    // One deadline for the whole request - every redirect hop and the body read -
    // so a chain of slow redirects can't outlive the monitor's timeout. Latency is
    // taken when the final response's headers arrive, before the body read, to
    // match the single-request timing this replaced.
    let attempt = async {
        let mut response = send_following_redirects(client, monitor, spec).await?;
        let latency = millis(start.elapsed());
        let code = response.status().as_u16();
        let status_ok = match &spec.expected_status {
            Some(expected) => expected.matches(code),
            None => response.status().is_success(),
        };
        // Read the body only when we need it: to detail a failure, or to run
        // a keyword/JSON assertion. Assertions get a larger budget. The head
        // (status line + headers) is captured first - reading the body
        // consumes the response - in case this turns into a failure snapshot.
        let assertions = spec.asserts_body();
        let (head, body, cut_at) = if !status_ok || assertions {
            let head = snapshot_head(&response);
            let cap = if assertions {
                spec.assertion_body_cap()
            } else {
                MAX_SNAPSHOT_BODY_CHARS
            };
            // A body cut short by a transport error is reported as such:
            // running the assertions on the fragment would blame the content
            // ("keyword missing") for what is a connection problem.
            // Past the cap the rest is dropped unread: a snapshot or an
            // assertion only ever looks at the first `cap` bytes.
            let body = crate::http::read_capped(&mut response, cap)
                .await
                .map_err(HttpError::Request)?;
            (head, body.bytes, body.truncated.then_some(cap))
        } else {
            (String::new(), Vec::new(), None)
        };
        Ok::<_, HttpError>((code, status_ok, head, body, cut_at, latency))
    };

    match tokio::time::timeout(monitor.timeout(), attempt).await {
        // Through a proxy, a 407 is the proxy refusing us, not the target
        // answering: blame the proxy.
        Ok(Ok((407, _, head, body, _, _))) if spec.proxy.is_some() => Outcome {
            status: CheckStatus::Down,
            latency_ms: None,
            status_code: None,
            error: Some("proxy failed: HTTP 407 Proxy Authentication Required".to_owned()),
            reason: Some(FailureKind::ConnectionFailed),
            snapshot: Some(render_snapshot(&head, &body)),
        },
        Ok(Ok((code, status_ok, head, body, cut_at, latency))) => {
            let (up, error, reason) = if !status_ok {
                let snippet = snippet(&body);
                let detail = if snippet.is_empty() {
                    format!("HTTP {code}")
                } else {
                    format!("HTTP {code}: {snippet}")
                };
                (false, Some(detail), Some(FailureKind::Http))
            } else if let Some(failure) = check_assertions(spec, &body) {
                // The assertion judged a body cut at the cap: say so, what
                // it looked for may well sit past the cut.
                let failure = match cut_at {
                    Some(cap) => format!(
                        "{failure} (body cut at {})",
                        crate::fmt::bytes(u64::try_from(cap).unwrap_or(u64::MAX))
                    ),
                    None => failure,
                };
                (false, Some(failure), Some(FailureKind::Content))
            } else {
                (true, None, None)
            };

            let status = if up {
                CheckStatus::up_unless(over_threshold(latency, monitor.degraded_over_ms))
            } else {
                CheckStatus::Down
            };
            Outcome {
                status,
                latency_ms: Some(latency),
                status_code: Some(i64::from(code)),
                error,
                reason,
                // The service answered something and the check failed: keep
                // what it answered for the incident record.
                snapshot: (!up).then(|| render_snapshot(&head, &body)),
            }
        }
        Ok(Err(HttpError::TooManyRedirects)) => Outcome::down(
            FailureKind::TooManyRedirects,
            "too many redirects".to_owned(),
        ),
        Ok(Err(HttpError::InsecureRedirect(to))) => Outcome::down(
            FailureKind::InsecureRedirect,
            format!("redirected to plain http ({to})"),
        ),
        // The category only: where the redirect pointed is the operator's
        // network as an outsider described it, not something to store.
        Ok(Err(HttpError::PrivateRedirect)) => Outcome::down(
            FailureKind::PrivateRedirect,
            "redirected to a private address".to_owned(),
        ),
        Ok(Err(HttpError::Request(err))) => request_failure(spec, &err),
        Err(_elapsed) => Outcome::down(FailureKind::Timeout, "request timed out".to_owned()),
    }
}

/// reqwest's default redirect ceiling; we follow them ourselves (below) and keep
/// the same bound.
const MAX_REDIRECTS: usize = 10;

/// Either a transport error or one of our own redirect verdicts. The probe
/// client never auto-follows, so reqwest can't produce a "too many redirects"
/// error of its own.
enum HttpError {
    Request(reqwest::Error),
    TooManyRedirects,
    /// An `https` hop redirected to plain `http`; holds the target origin.
    InsecureRedirect(String),
    /// A public target redirected to an internal address.
    PrivateRedirect,
}

/// Send the monitor's request, following redirects manually so the configured
/// headers - which may carry credentials (an API key, a bearer token) - are
/// re-attached only while the redirect stays on the original origin. reqwest
/// strips its own well-known sensitive headers across hosts but not arbitrary
/// custom ones, so without this a malicious or compromised target could 30x us
/// to an attacker host and harvest the header. Cross-origin hops are still
/// followed, just without the headers - unless they lead a public target to
/// an internal address, which is refused (see
/// [`RedirectGuard`](crate::http::RedirectGuard), which also states what a DNS
/// rebinding or a proxy leaves open).
///
/// The method does not change along the chain: a `HEAD` monitor sends `HEAD`
/// to every hop, a `GET` one `GET`. The rule that turns a request into a
/// `GET` on a 301, 302 or 303 is about methods with a body, which a monitor
/// cannot have.
async fn send_following_redirects(
    client: &Client,
    monitor: &Monitor,
    spec: &HttpSpec,
) -> Result<reqwest::Response, HttpError> {
    let target = &spec.url;
    // A monitor expecting a 3xx asserts the redirect itself (HTTP -> HTTPS,
    // apex -> www): following it would judge the destination instead, and the
    // probe could never pass. One 3xx among the accepted statuses is enough:
    // with `[200, 301]` a followed 301 could never count either.
    let follow = !spec
        .expected_status
        .as_ref()
        .is_some_and(crate::config::ExpectedStatus::accepts_redirect);
    let mut url = target.clone();
    let mut guard: Option<RedirectGuard> = None;
    for _ in 0..=MAX_REDIRECTS {
        // The per-request timeout overrides the client's 15s notifier backstop,
        // which may be *shorter* than the monitor's own; the caller's outer
        // deadline still bounds the whole chain.
        let mut request = client
            .request(spec.method.into(), url.clone())
            .timeout(monitor.timeout());
        if same_origin(target, &url) {
            request = with_headers(request, &spec.headers);
        }
        let response = request.send().await.map_err(HttpError::Request)?;
        // A redirected hop that reached an internal address after all is
        // dropped here, before anything reads its body.
        if let Some(guard) = guard {
            guard
                .check_peer(&response)
                .map_err(|_| HttpError::PrivateRedirect)?;
        }
        if !follow || !response.status().is_redirection() {
            return Ok(response);
        }
        // A 3xx without a usable Location (or 304 Not Modified) is the final
        // response; hand it back rather than chase nothing.
        let next = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|location| url.join(location).ok());
        match next {
            // Following it would carry the request (cookies, query string,
            // whatever the client sends next) in clear text: a monitor that
            // says https means it, so this is a down, not a silent downgrade.
            Some(next) if is_downgrade(&url, &next) => {
                return Err(HttpError::InsecureRedirect(
                    next.origin().ascii_serialization(),
                ));
            }
            Some(next) => {
                // The first redirect of the chain: `response` is the
                // target's own answer, which tells where the target is.
                let checked = if let Some(guard) = guard {
                    guard
                } else {
                    let proxied = spec.proxy.is_some();
                    *guard.insert(RedirectGuard::new(target, &response, proxied).await)
                };
                checked
                    .check_target(&next)
                    .await
                    .map_err(|_| HttpError::PrivateRedirect)?;
                url = next;
            }
            None => return Ok(response),
        }
    }
    Err(HttpError::TooManyRedirects)
}

/// Whether `url` shares an origin (scheme + host + port) with the original
/// `target` - the rule that decides whether the monitor's (possibly
/// credential-bearing) headers are re-attached across a redirect.
pub(super) fn same_origin(target: &reqwest::Url, url: &reqwest::Url) -> bool {
    target.origin() == url.origin()
}

/// Run the configured keyword/JSON assertions against the body; the first that
/// fails returns its reason. `None` means every assertion passed.
pub(super) fn check_assertions(spec: &HttpSpec, body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    if let Some(keyword) = &spec.keyword {
        let found = text.contains(keyword.text.as_str());
        if found == keyword.invert {
            return Some(if keyword.invert {
                format!("keyword present: {}", keyword.text)
            } else {
                format!("keyword missing: {}", keyword.text)
            });
        }
    }
    if let Some(json) = &spec.json
        && let Some(failure) = check_json(json, &text)
    {
        return Some(failure);
    }
    if let Some(number) = &spec.number {
        return check_number(number, &text);
    }
    None
}

/// Extract a number from the body via the configured regex (first capture
/// group, or the whole match for a group-less pattern) and check it against
/// the optional inclusive bounds. Returns a failure reason or `None`.
#[expect(
    clippy::cast_precision_loss,
    reason = "config bounds are human-scale thresholds, far below 2^52"
)]
pub(super) fn check_number(number: &NumberAssert, body: &str) -> Option<String> {
    let (pattern, min, max) = (number.regex.raw(), number.min, number.max);
    let Some(captures) = number.regex.captures(body) else {
        return Some(format!("number_regex matched nothing: {pattern}"));
    };
    let matched = captures
        .get(1)
        .or_else(|| captures.get(0))
        .map_or("", |capture| capture.as_str());
    let Ok(value) = matched.trim().parse::<f64>() else {
        return Some(format!("number_regex match is not a number: {matched}"));
    };
    if let Some(min) = min
        && value < min as f64
    {
        return Some(format!("number {value} below min {min}"));
    }
    if let Some(max) = max
        && value > max as f64
    {
        return Some(format!("number {value} above max {max}"));
    }
    None
}

/// Evaluate a `JSONPath` against the body. Returns a failure reason or `None`.
pub(super) fn check_json(json: &JsonAssert, body: &str) -> Option<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Some("response is not valid JSON".to_owned());
    };
    let query = json.query.raw();
    let nodes = json.query.query(&value).all();
    match json.expected.as_deref() {
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

/// Apply every configured header to the request. reqwest *appends* headers, so
/// each distinct header is kept - none overwrites another.
pub(super) fn with_headers(
    mut request: RequestBuilder,
    headers: &HashMap<String, Secret>,
) -> RequestBuilder {
    for (name, value) in headers {
        request = request.header(name, value.as_ref());
    }
    request
}

/// The outcome of a request that got no response.
fn request_failure(spec: &HttpSpec, err: &reqwest::Error) -> Outcome {
    // Out of descriptors or memory here: no verdict on the target.
    if let Some(detail) = super::local::exhaustion_in(err) {
        return Outcome::down(FailureKind::Local, detail);
    }
    // A proxy that refused us (407 on CONNECT, SOCKS credentials) is not the
    // target failing: say so.
    if spec.proxy.is_some()
        && let Some(cause) = proxy_failure(err)
    {
        return Outcome::down(FailureKind::ConnectionFailed, cause);
    }
    let (kind, detail) = describe(err);
    // The category alone ("connection failed") reads the same for a refused
    // port and an expired certificate: name the cause when the error chain
    // carries one (never the URL, which may hold secrets).
    let detail = match transport_cause(err) {
        Some(cause) => format!("{detail}: {cause}"),
        None => detail.to_owned(),
    };
    Outcome::down(kind, detail)
}

/// The root cause of a transport error worth naming - a TLS failure
/// (`invalid peer certificate: Expired`, `peer is incompatible`...) or a
/// refused / reset connection - found by walking the error chain. rustls and
/// io errors never embed the request URL.
fn transport_cause(err: &reqwest::Error) -> Option<String> {
    let mut source = std::error::Error::source(err);
    while let Some(current) = source {
        if let Some(tls) = tls_error(current) {
            return Some(format!("TLS {tls}"));
        }
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            match io.kind() {
                std::io::ErrorKind::ConnectionRefused => {
                    return Some("connection refused".to_owned());
                }
                std::io::ErrorKind::ConnectionReset => return Some("connection reset".to_owned()),
                _ => {}
            }
        }
        source = current.source();
    }
    None
}

/// The failure of the proxy itself, when that is what broke the request: an
/// HTTP proxy refusing the CONNECT tunnel (`407`, or any non-2xx answer) or a
/// SOCKS proxy refusing the handshake (bad credentials, unreachable proxy).
/// hyper-util's error types are not reachable from here (reqwest wraps them in
/// a private type), so they are recognised by their stable `Display`
/// prefixes; neither embeds the request URL.
fn proxy_failure(err: &reqwest::Error) -> Option<String> {
    let mut source = std::error::Error::source(err);
    let mut socks = false;
    while let Some(current) = source {
        let text = current.to_string();
        if text.starts_with("tunnel error: ") || text.starts_with("SOCKS error: ") {
            return Some(format!("proxy failed: {text}"));
        }
        socks |= text.starts_with("error connecting to socks proxy");
        source = current.source();
    }
    socks.then(|| "proxy failed: could not connect through the SOCKS proxy".to_owned())
}

/// The rustls error behind `err`, looking through the `io::Error` wrappers
/// hyper-rustls nests it in (`io::Error::source` skips the wrapped error
/// itself, so the chain alone never reaches it).
fn tls_error<'a>(
    mut err: &'a (dyn std::error::Error + 'static),
) -> Option<&'a tokio_rustls::rustls::Error> {
    loop {
        if let Some(tls) = err.downcast_ref::<tokio_rustls::rustls::Error>() {
            return Some(tls);
        }
        err = err.downcast_ref::<std::io::Error>()?.get_ref()?;
    }
}

/// A concise, URL-free description of a request error. The raw error embeds the
/// target URL (which may carry credentials), so we categorize instead.
fn describe(err: &reqwest::Error) -> (FailureKind, &'static str) {
    if err.is_timeout() {
        (FailureKind::Timeout, "request timed out")
    } else if err.is_connect() {
        (FailureKind::ConnectionFailed, "connection failed")
    } else if err.is_redirect() {
        (FailureKind::TooManyRedirects, "too many redirects")
    } else if err.is_body() || err.is_decode() {
        (FailureKind::InvalidBody, "invalid response body")
    } else {
        (FailureKind::RequestError, "request error")
    }
}
