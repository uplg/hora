//! RDAP domain-expiration lookup ("your domain expires in 14 days").
//!
//! RDAP is the registries' JSON-over-HTTP replacement for whois: no text
//! parsing, a standard `events` array, and `rdap.org` as a community
//! bootstrap that redirects to the authoritative registry server. One lookup
//! a day per domain is plenty, so the watcher gates on the stored
//! `checked_at` rather than polling each tick.

use chrono::DateTime;

use crate::http::get_json_following;

/// The community RDAP bootstrap: redirects `/domain/{name}` to the
/// authoritative registry's RDAP server.
const BOOTSTRAP: &str = "https://rdap.org/domain";

/// How many bootstrap redirects to follow: the 30x from the bootstrap is
/// occasionally chained by a registry.
const MAX_REDIRECTS: usize = 5;

/// Look up `domain`'s expiration via RDAP: unix epoch seconds (UTC).
///
/// # Errors
///
/// Returns an error if the lookup fails (network, an unknown domain or TLD,
/// a 4xx/5xx from the registry) or the answer carries no expiration event.
pub(crate) async fn domain_expiration(
    client: &reqwest::Client,
    domain: &str,
) -> anyhow::Result<i64> {
    expiration_at(client, &format!("{BOOTSTRAP}/{domain}")).await
}

async fn expiration_at(client: &reqwest::Client, url: &str) -> anyhow::Result<i64> {
    let accept = [(reqwest::header::ACCEPT.as_str(), "application/rdap+json")];
    let body = match get_json_following(client, url, &accept, MAX_REDIRECTS).await? {
        Ok(body) => body,
        Err(status) => anyhow::bail!("registry answered HTTP {status}"),
    };
    expiration_event(&body).ok_or_else(|| anyhow::anyhow!("no expiration event in the RDAP answer"))
}

/// The `expiration` event's date from an RDAP domain object, as unix epoch
/// seconds. Registries answer `events: [{eventAction, eventDate}, ...]`
/// (RFC 9083); the date is RFC 3339.
fn expiration_event(body: &serde_json::Value) -> Option<i64> {
    body.get("events")?
        .as_array()?
        .iter()
        .find(|event| {
            event.get("eventAction").and_then(serde_json::Value::as_str) == Some("expiration")
        })?
        .get("eventDate")?
        .as_str()
        .and_then(|date| DateTime::parse_from_rfc3339(date).ok())
        .map(|date| date.timestamp())
}

/// The answer parsing for the fuzz targets in `fuzz/` (built with
/// `--cfg fuzzing` only).
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzz {
    /// [`super::expiration_event`].
    #[must_use]
    pub fn expiration_event(body: &serde_json::Value) -> Option<i64> {
        super::expiration_event(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_expiration_event() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{
                "objectClassName": "domain",
                "ldhName": "EXAMPLE.COM",
                "events": [
                    { "eventAction": "registration", "eventDate": "1995-08-14T04:00:00Z" },
                    { "eventAction": "expiration", "eventDate": "2026-08-13T04:00:00Z" },
                    { "eventAction": "last changed", "eventDate": "2025-08-14T07:01:44Z" }
                ]
            }"#,
        )
        .unwrap();
        let expected = DateTime::parse_from_rfc3339("2026-08-13T04:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(expiration_event(&body), Some(expected));
    }

    /// A tiny HTTP/1.1 server answering each request path from `routes`
    /// (`path -> raw response`), one connection per request.
    async fn serve(routes: Vec<(&'static str, String)>) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => request.extend_from_slice(&buf[..read]),
                    }
                }
                let line = String::from_utf8_lossy(&request).into_owned();
                let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                let response = routes.iter().find(|(route, _)| *route == path).map_or_else(
                    || "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n".to_owned(),
                    |(_, response)| response.clone(),
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        base
    }

    fn json_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/rdap+json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn follows_a_relative_redirect_whatever_the_client_policy() {
        let body =
            r#"{"events":[{"eventAction":"expiration","eventDate":"2030-01-01T00:00:00Z"}]}"#;
        let base = serve(vec![
            (
                "/domain/example.org",
                "HTTP/1.1 302 Found\r\nlocation: ../registry/example.org\r\n\
                 content-length: 0\r\nconnection: close\r\n\r\n"
                    .to_owned(),
            ),
            ("/registry/example.org", json_response(body)),
        ])
        .await;
        let expected = DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z")
            .unwrap()
            .timestamp();
        let url = format!("{base}/domain/example.org");
        // A client that never follows (the loop does) and one that does.
        for client in [
            crate::http::probe_client(None).unwrap(),
            crate::http::client(None).unwrap(),
        ] {
            assert_eq!(expiration_at(&client, &url).await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn oversized_answers_and_errors_are_refused() {
        let huge = format!(r#"{{"pad":"{}"}}"#, "x".repeat(crate::http::MAX_JSON_BYTES));
        let base = serve(vec![("/big", json_response(&huge))]).await;
        let client = crate::http::probe_client(None).unwrap();
        let err = expiration_at(&client, &format!("{base}/big"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("larger than"), "{err}");
        let err = expiration_at(&client, &format!("{base}/missing"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 404"), "{err}");
    }

    #[test]
    fn missing_or_malformed_events_yield_none() {
        let no_events: serde_json::Value = serde_json::json!({ "ldhName": "x.org" });
        assert_eq!(expiration_event(&no_events), None);

        let no_expiration = serde_json::json!({
            "events": [{ "eventAction": "registration", "eventDate": "1995-08-14T04:00:00Z" }]
        });
        assert_eq!(expiration_event(&no_expiration), None);

        let bad_date = serde_json::json!({
            "events": [{ "eventAction": "expiration", "eventDate": "not a date" }]
        });
        assert_eq!(expiration_event(&bad_date), None);
    }
}
