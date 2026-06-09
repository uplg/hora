//! Shared helpers for the built-in notifiers.

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

/// `" (1234ms)"` for a known latency, empty otherwise - appended to a degraded
/// alert so the message says *how* slow the monitor got.
pub(crate) fn latency_suffix(latency_ms: Option<i64>) -> String {
    latency_ms.map_or_else(String::new, |ms| format!(" ({ms}ms)"))
}

/// Topology annotation for a down alert: `"caused by X"` when the monitor is a
/// symptom, `"impacts N: a, b, c"` when it is a root cause, empty otherwise.
pub(crate) fn topology_suffix(cause: Option<&str>, impacted: &[&str]) -> String {
    if let Some(cause) = cause {
        return format!("\ncaused by {cause}");
    }
    if impacted.is_empty() {
        return String::new();
    }
    format!("\nimpacts {}: {}", impacted.len(), impacted.join(", "))
}

/// Human phrasing for a budget-burn event, shared so every channel words the
/// rate and the exhaustion estimate the same way:
/// `"burning error budget at 14.4x (1h) - exhausted in ~23h at this rate"`.
pub(crate) fn budget_burn_phrase(
    burn_rate_x10: i64,
    window: &str,
    exhausted_in_secs: Option<i64>,
) -> String {
    let rate = if burn_rate_x10 % 10 == 0 {
        format!("{}x", burn_rate_x10 / 10)
    } else {
        format!("{}.{}x", burn_rate_x10 / 10, burn_rate_x10 % 10)
    };
    let eta = exhausted_in_secs.map_or_else(String::new, |secs| {
        if secs == 0 {
            " - budget already exhausted".to_owned()
        } else {
            format!(" - exhausted in ~{} at this rate", human_duration(secs))
        }
    });
    format!("burning error budget at {rate} ({window}){eta}")
}

/// `"2d 3h"`, `"6h"`, `"45m"`, `"30s"` - coarse on purpose, it's an estimate.
fn human_duration(secs: i64) -> String {
    if secs >= 2 * 86_400 {
        format!("{}d {}h", secs / 86_400, (secs % 86_400) / 3600)
    } else if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Human phrasing for a certificate-expiry event.
pub(crate) fn cert_expiry_phrase(days_left: i64) -> String {
    if days_left <= 0 {
        "has expired".to_owned()
    } else if days_left == 1 {
        "expires in 1 day".to_owned()
    } else {
        format!("expires in {days_left} days")
    }
}

/// Delivery attempts: the initial send plus two retries. The caller marks the
/// alert as sent regardless of the outcome, so a transient blip here would
/// otherwise silently drop the notification.
const MAX_ATTEMPTS: u32 = 3;

/// Send a request and log (never panic) on failure. `build` produces a fresh
/// request on each attempt, so it can be retried; `secret` is stripped from any
/// logged text so a token in the URL or a header never reaches the logs. On
/// rejection a bounded snippet of the response body is logged (it usually says
/// *why*, e.g. "chat not found").
///
/// Transient failures (a network error, an HTTP 5xx, or 429) are retried with a
/// short backoff. Client errors (4xx other than 429) are permanent, so they are
/// reported immediately without retrying. Every notifier goes through here, so
/// they all share one retry and redaction policy.
pub(crate) async fn send_retrying<F>(build: F, channel: &str, secret: &str)
where
    F: Fn() -> RequestBuilder,
{
    for attempt in 1..=MAX_ATTEMPTS {
        match build().send().await {
            Ok(response) if response.status().is_success() => return,
            Ok(response) => {
                let status = response.status();
                let transient =
                    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
                if transient && attempt < MAX_ATTEMPTS {
                    backoff(attempt).await;
                    continue;
                }
                let body = response.text().await.unwrap_or_default();
                let detail = redact(&snippet(&body), secret);
                warn!("{channel} rejected the notification (HTTP {status}): {detail}");
                return;
            }
            Err(err) => {
                if attempt < MAX_ATTEMPTS {
                    backoff(attempt).await;
                    continue;
                }
                warn!(
                    "{channel} request failed after {attempt} attempts: {}",
                    redact(&err.to_string(), secret)
                );
                return;
            }
        }
    }
}

/// POST `payload` as JSON, retrying transient failures (see [`send_retrying`]).
pub(crate) async fn post_json<T: Serialize>(
    client: &Client,
    url: &str,
    payload: &T,
    channel: &str,
    secret: &str,
) {
    send_retrying(|| client.post(url).json(payload), channel, secret).await;
}

/// Exponential backoff between delivery attempts: 200ms, then 400ms. Kept short
/// so a genuine outage alert is still delivered promptly.
async fn backoff(attempt: u32) {
    let ms = 200_u64 * 2_u64.pow(attempt - 1);
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

/// Strip `secret` from `text`. Empty secret = no-op (a `str::replace("", …)` would
/// otherwise splice the replacement between every character).
fn redact(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        text.to_owned()
    } else {
        text.replace(secret, "<redacted>")
    }
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

    #[test]
    fn escapes_metacharacters() {
        assert_eq!(escape("a<b>&c"), "a&lt;b&gt;&amp;c");
    }

    #[test]
    fn cert_phrasing() {
        assert_eq!(cert_expiry_phrase(-1), "has expired");
        assert_eq!(cert_expiry_phrase(0), "has expired");
        assert_eq!(cert_expiry_phrase(1), "expires in 1 day");
        assert_eq!(cert_expiry_phrase(3), "expires in 3 days");
    }

    #[test]
    fn redact_strips_secret_and_handles_empty() {
        assert_eq!(
            redact("token abc123 failed", "abc123"),
            "token <redacted> failed"
        );
        // An empty secret must not splice <redacted> between every character.
        assert_eq!(redact("hello", ""), "hello");
    }
}
