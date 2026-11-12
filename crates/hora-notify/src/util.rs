//! Shared helpers for the built-in notifiers.

use std::time::Duration;

use reqwest::{Client, RequestBuilder};
use serde::Serialize;
use tracing::warn;

/// Escape the characters special to HTML / Slack mrkdwn (`& < >`).
pub(crate) fn escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `"2 d 3 h"`, `"6 h"`, `"45 min"`, `"30 s"` - a span in SI notation, coarse
/// on purpose: it phrases a burn estimate, the time a certificate has left
/// and how long a channel has been failing, not a measurement. One unit under
/// two days, days and hours from there (`"2 d"` on the hour).
///
/// Twin of `hora_core::fmt::elapsed` (hora-notify sits below hora-core and
/// cannot call it), so a span reads the same in a notification and in
/// `hora top`. Public, though no part of the notifier API, so that hora-core
/// can hold the two together: its `the_notify_twin_writes_the_same_spans`
/// test fails when either one changes alone.
#[must_use]
pub fn human_duration(secs: u64) -> String {
    if secs >= 2 * 86_400 {
        match (secs % 86_400) / 3600 {
            0 => format!("{} d", secs / 86_400),
            hours => format!("{} d {hours} h", secs / 86_400),
        }
    } else if secs >= 3600 {
        format!("{} h", secs / 3600)
    } else if secs >= 60 {
        format!("{} min", secs / 60)
    } else {
        format!("{secs} s")
    }
}

/// What a channel's length limit counts: characters (most APIs) or UTF-8
/// bytes (ntfy, which turns a longer body into an attachment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unit {
    Chars,
    Bytes,
}

/// The length of `text` in `unit`.
pub(crate) fn measure(text: &str, unit: Unit) -> usize {
    match unit {
        Unit::Chars => text.chars().count(),
        Unit::Bytes => text.len(),
    }
}

const ELLIPSIS: char = '…';

/// `text` cut to at most `max` (in `unit`) on a char boundary, ending with an
/// ellipsis when anything was cut so the reader knows there was more. Never
/// splits a multi-byte character; a budget too small for the ellipsis itself
/// yields a bare cut (or nothing at all for `max == 0`).
pub(crate) fn truncate(text: &str, max: usize, unit: Unit) -> String {
    if measure(text, unit) <= max {
        return text.to_owned();
    }
    let ellipsis = match unit {
        Unit::Chars => 1,
        Unit::Bytes => ELLIPSIS.len_utf8(),
    };
    let (room, mark) = if max >= ellipsis {
        (max - ellipsis, true)
    } else {
        (max, false)
    };
    let mut used = 0;
    let mut out = String::with_capacity(room.min(text.len()) + 3);
    for ch in text.chars() {
        let cost = match unit {
            Unit::Chars => 1,
            Unit::Bytes => ch.len_utf8(),
        };
        if used + cost > room {
            break;
        }
        used += cost;
        out.push(ch);
    }
    if mark {
        out.push(ELLIPSIS);
    }
    out
}

/// Marker error for a notification the API refused as too large (HTTP 413).
/// The channel itself answered, so the dispatcher does not count it toward the
/// channel's failure streak - one oversized alert must not make the watchdog
/// report a healthy channel as broken. Still logged and returned as a delivery
/// failure. A 400 is deliberately *not* treated this way: messages are already
/// cut to each API's limits, and the 400s that remain are misconfigurations
/// (Telegram "chat not found", a Pushover user key) the watchdog must report.
#[derive(Debug)]
pub(crate) struct PayloadRejected(pub String);

impl std::fmt::Display for PayloadRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PayloadRejected {}

/// Marker error for a notification the API kept answering 429 to. Like
/// [`PayloadRejected`], the channel itself is alive - it is asking us to slow
/// down, typically while many monitors go down at once - so the dispatcher
/// does not count it toward the channel's failure streak. Still logged and
/// returned as a delivery failure.
#[derive(Debug)]
pub(crate) struct RateLimited(pub String);

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RateLimited {}

/// Whether a delivery error says nothing about the channel's health (see
/// [`PayloadRejected`] and [`RateLimited`]).
pub(crate) fn spares_health(err: &anyhow::Error) -> bool {
    err.is::<PayloadRejected>() || err.is::<RateLimited>()
}

/// The longest a `Retry-After` is honoured for. An outage alert must still go
/// out promptly, and the whole delivery has to fit the dispatcher's
/// per-channel deadline.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(5);

/// The wait a 429 asks for: its `Retry-After` in whole seconds (the form the
/// chat APIs send), capped at [`RETRY_AFTER_CAP`]. `None` when the header is
/// absent or in another form, which falls back to the short backoff.
fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let seconds: u64 = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(seconds).min(RETRY_AFTER_CAP))
}

/// Delivery attempts: the initial send plus two retries. The caller marks the
/// alert as sent regardless of the outcome, so a transient blip here would
/// otherwise silently drop the notification.
const MAX_ATTEMPTS: u32 = 3;

/// Send a request and log (never panic) on failure. `build` produces a fresh
/// request on each attempt, so it can be retried; every `secrets` entry is
/// stripped from any logged text so a token in the URL, a header or a query
/// never reaches the logs. On rejection a bounded snippet of the response body
/// is logged (it usually says *why*, e.g. "chat not found").
///
/// Transient failures (a network error, an HTTP 5xx, or 429) are retried with a
/// short backoff; a 429 that carries `Retry-After` waits that long instead
/// (capped). Client errors (4xx other than 429) are permanent, so they are
/// reported immediately without retrying. Every notifier goes through here, so
/// they all share one retry and redaction policy.
/// The failure is both logged here (one policy for the daemon, which fires and
/// forgets) and returned, so a caller that *does* care - `hora test-alert`'s
/// exit code - can tell that delivery failed without re-deriving the detail.
pub(crate) async fn send_retrying<F>(
    build: F,
    channel: &str,
    secrets: &[&str],
) -> anyhow::Result<()>
where
    F: Fn() -> RequestBuilder,
{
    for attempt in 1..=MAX_ATTEMPTS {
        match build().send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                let status = response.status();
                let transient =
                    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
                let throttled = status == reqwest::StatusCode::TOO_MANY_REQUESTS;
                if transient && attempt < MAX_ATTEMPTS {
                    // A rate limit names its own wait; retrying sooner only
                    // burns the remaining attempts against the same limit.
                    match retry_after(&response).filter(|_| throttled) {
                        Some(wait) => tokio::time::sleep(wait).await,
                        None => backoff(attempt).await,
                    }
                    continue;
                }
                let body = rejection_body(response).await;
                let detail = redact(&snippet(&body), secrets);
                warn!("{channel} rejected the notification (HTTP {status}): {detail}");
                let message = format!("rejected (HTTP {status}): {detail}");
                if payload_rejected(status) {
                    return Err(PayloadRejected(message).into());
                }
                if throttled {
                    return Err(RateLimited(message).into());
                }
                anyhow::bail!(message);
            }
            Err(err) => {
                if attempt < MAX_ATTEMPTS {
                    backoff(attempt).await;
                    continue;
                }
                let detail = redact(&err.to_string(), secrets);
                warn!("{channel} request failed after {attempt} attempts: {detail}");
                anyhow::bail!("request failed after {attempt} attempts: {detail}");
            }
        }
    }
    unreachable!("every iteration either returns, bails or continues within MAX_ATTEMPTS")
}

/// How much of a rejection's body is read: the log line shows 200 characters
/// of it, and 4 KiB holds those in any encoding.
const REJECTION_BODY_MAX: usize = 4096;

/// The start of a rejected request's body, at most [`REJECTION_BODY_MAX`]
/// bytes of it. The endpoint is whatever the operator configured (or whatever
/// answers in its place): reading the whole body to log a snippet would let
/// it make the daemon buffer as much as it cares to send. Dropping the
/// response once the limit is reached closes the connection on the rest. A
/// body that fails midway keeps what arrived: the status is the verdict, the
/// text only a hint.
async fn rejection_body(mut response: reqwest::Response) -> String {
    let mut body = Vec::new();
    while body.len() < REJECTION_BODY_MAX {
        let Ok(Some(chunk)) = response.chunk().await else {
            break;
        };
        let room = REJECTION_BODY_MAX - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
    String::from_utf8_lossy(&body).into_owned()
}

/// Whether `status` means "this request's content was too large" (see
/// [`PayloadRejected`]) rather than "the channel is broken" (400/401/403/404:
/// bad chat id, revoked token, deleted webhook - those must trip the watchdog).
fn payload_rejected(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::PAYLOAD_TOO_LARGE
}

/// POST `payload` as JSON, retrying transient failures (see [`send_retrying`]).
pub(crate) async fn post_json<T: Serialize>(
    client: &Client,
    url: &str,
    payload: &T,
    channel: &str,
    secrets: &[&str],
) -> anyhow::Result<()> {
    send_retrying(|| client.post(url).json(payload), channel, secrets).await
}

/// Exponential backoff between delivery attempts: 200ms, then 400ms. Kept short
/// so a genuine outage alert is still delivered promptly.
async fn backoff(attempt: u32) {
    let ms = 200_u64 * 2_u64.pow(attempt - 1);
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// Strip every secret from `text` - raw, and in the two percent-encoded forms
/// a URL serializer may have written it in (strict RFC 3986 `%XX`, and
/// form-urlencoding where space is `+`), since an error that echoes a URL
/// echoes the *encoded* secret, not the one the operator typed. Empty secrets
/// are skipped (a `str::replace("", …)` would otherwise splice the replacement
/// between every character).
fn redact(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_owned();
    for secret in secrets {
        if secret.is_empty() {
            continue;
        }
        out = out.replace(secret, "<redacted>");
        for encoded in [percent_encode(secret, false), percent_encode(secret, true)] {
            if encoded != *secret {
                out = out.replace(&encoded, "<redacted>");
            }
        }
    }
    out
}

/// Percent-encode `value` byte-wise: every byte outside the unreserved set
/// becomes `%XX` (uppercase hex, as URL serializers emit). `form` selects
/// `application/x-www-form-urlencoded` (reqwest's query/form serializer):
/// space is `+` and `*` stays bare; the strict RFC 3986 variant keeps `~` bare
/// instead.
///
/// The strict variant is the twin of `hora_core::fmt::percent_encode` (out of
/// reach from this crate); keep the two byte-for-byte identical.
fn percent_encode(value: &str, form: bool) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' => out.push(byte as char),
            b'~' if !form => out.push('~'),
            b'*' if form => out.push('*'),
            b' ' if form => out.push('+'),
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// A bounded, single-line snippet of a response body for log output.
fn snippet(body: &str) -> String {
    // Fold straight into one string: no intermediate `Vec<&str>` just to `join`.
    body.split_whitespace()
        .fold(String::new(), |mut acc, word| {
            if !acc.is_empty() {
                acc.push(' ');
            }
            acc.push_str(word);
            acc
        })
        .chars()
        .take(200)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server answering each request with the next of `replies` (the last
    /// one from then on). Returns its URL and when each request arrived.
    async fn replying(
        replies: &'static [&'static str],
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<std::time::Instant>>>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let arrivals = std::sync::Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = [0; 4096];
                if socket.read(&mut request).await.unwrap_or(0) == 0 {
                    continue;
                }
                let index = {
                    let mut arrivals = arrivals.lock().unwrap();
                    arrivals.push(std::time::Instant::now());
                    (arrivals.len() - 1).min(replies.len() - 1)
                };
                let _ = socket.write_all(replies[index].as_bytes()).await;
            }
        });
        (url, seen)
    }

    const THROTTLED_1S: &str = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    const THROTTLED_NOW: &str = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    const OK: &str = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

    /// A 429 is retried when the API says so, not 200 ms later against the
    /// same limit: the alert that used to be dropped is delivered.
    #[tokio::test]
    async fn a_rate_limit_waits_its_retry_after() {
        let (url, seen) = replying(&[THROTTLED_1S, OK]).await;
        let client = Client::new();
        send_retrying(|| client.post(&url), "test", &[])
            .await
            .expect("delivered on the retry");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[1] - seen[0] >= Duration::from_secs(1));
    }

    /// A channel that keeps answering 429 is alive: the failure is reported,
    /// but as one the dispatcher keeps out of the channel's health.
    #[tokio::test]
    async fn a_lasting_rate_limit_spares_the_channel_health() {
        let (url, seen) = replying(&[THROTTLED_NOW]).await;
        let client = Client::new();
        let err = send_retrying(|| client.post(&url), "test", &[])
            .await
            .expect_err("still throttled");
        assert!(spares_health(&err));
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    /// A server answering every request with a 400 whose body never ends.
    /// Returns its URL.
    async fn rejecting_without_end() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = [0; 4096];
                    if socket.read(&mut request).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let head = "HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\nSTART ";
                    if socket.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    // Until the client hangs up.
                    while socket.write_all(&[b'x'; 1024]).await.is_ok() {}
                });
            }
        });
        url
    }

    /// The body of a rejection is read up to its limit and no further: an
    /// endpoint streaming without end used to hold the delivery (and the
    /// memory) for as long as it kept sending.
    #[tokio::test]
    async fn a_rejection_body_is_read_up_to_its_limit() {
        let url = rejecting_without_end().await;
        let client = Client::new();

        let response = client.post(&url).send().await.expect("answered");
        let body = tokio::time::timeout(Duration::from_secs(10), rejection_body(response))
            .await
            .expect("the read stops at the limit");
        assert_eq!(body.len(), REJECTION_BODY_MAX);
        assert!(body.starts_with("START xxx"));

        let err = tokio::time::timeout(
            Duration::from_secs(10),
            send_retrying(|| client.post(&url), "test", &[]),
        )
        .await
        .expect("the delivery ends")
        .expect_err("rejected");
        assert!(
            err.to_string()
                .starts_with("rejected (HTTP 400 Bad Request): START xxx")
        );
    }

    #[test]
    fn escapes_metacharacters() {
        assert_eq!(escape("a<b>&c"), "a&lt;b&gt;&amp;c");
    }

    #[test]
    fn human_duration_formats() {
        assert_eq!(human_duration(30), "30 s");
        assert_eq!(human_duration(90), "1 min");
        assert_eq!(human_duration(3600), "1 h");
        assert_eq!(human_duration(9000), "2 h");
        assert_eq!(human_duration(200_000), "2 d 7 h");
    }

    #[test]
    fn truncate_counts_chars_and_marks_the_cut() {
        assert_eq!(truncate("hello", 5, Unit::Chars), "hello");
        assert_eq!(truncate("hello", 4, Unit::Chars), "hel…");
        // Multi-byte characters count once and are never split.
        assert_eq!(truncate("héllo wörld", 4, Unit::Chars), "hél…");
        assert_eq!(truncate("hello", 1, Unit::Chars), "…");
        assert_eq!(truncate("hello", 0, Unit::Chars), "");
    }

    #[test]
    fn truncate_by_bytes_stays_on_a_char_boundary() {
        // "€" is 3 bytes; the ellipsis is 3 bytes too.
        let cut = truncate("€€€€", 8, Unit::Bytes);
        assert_eq!(cut, "€…");
        assert!(cut.len() <= 8);
        let cut = truncate("€€€€", 7, Unit::Bytes);
        assert_eq!(cut, "€…");
        // Too small for the ellipsis: a bare cut, still on a boundary.
        assert_eq!(truncate("ab€", 2, Unit::Bytes), "ab");
        assert_eq!(truncate("€€", 2, Unit::Bytes), "");
    }

    #[test]
    fn payload_statuses_are_classified() {
        assert!(payload_rejected(reqwest::StatusCode::PAYLOAD_TOO_LARGE));
        assert!(!payload_rejected(reqwest::StatusCode::BAD_REQUEST));
        // A revoked token or deleted webhook is a broken channel, not a bad payload.
        assert!(!payload_rejected(reqwest::StatusCode::UNAUTHORIZED));
        assert!(!payload_rejected(reqwest::StatusCode::FORBIDDEN));
        assert!(!payload_rejected(reqwest::StatusCode::NOT_FOUND));
    }

    #[test]
    fn redact_strips_secrets_and_handles_empty() {
        assert_eq!(
            redact("token abc123 failed", &["abc123"]),
            "token <redacted> failed"
        );
        // An empty secret must not splice <redacted> between every character.
        assert_eq!(redact("hello", &[""]), "hello");
        // Every secret in the set is stripped, not just the first.
        assert_eq!(
            redact("url https://h/t1 token t0k3n", &["https://h/t1", "t0k3n"]),
            "url <redacted> token <redacted>"
        );
    }

    #[test]
    fn redact_strips_encoded_forms() {
        // A secret echoed back inside a URL appears percent-encoded.
        assert_eq!(
            redact("GET /send?pass=p%40ss%3Aword failed", &["p@ss:word"]),
            "GET /send?pass=<redacted> failed"
        );
        // Form-urlencoding writes a space as `+`.
        assert_eq!(
            redact("pass=my+pass rejected", &["my pass"]),
            "pass=<redacted> rejected"
        );
        // And as %20 in strict RFC 3986 contexts (URL paths).
        assert_eq!(redact("at /my%20pass/x", &["my pass"]), "at /<redacted>/x");
    }
}
