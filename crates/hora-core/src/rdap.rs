//! RDAP domain-expiration lookup ("your domain expires in 14 days").
//!
//! RDAP is the registries' JSON-over-HTTP replacement for whois: no text
//! parsing, a standard `events` array, and `rdap.org` as a community
//! bootstrap that redirects to the authoritative registry server. One lookup
//! a day per domain is plenty, so the watcher gates on the stored
//! `checked_at` rather than polling each tick.

use chrono::DateTime;

/// The community RDAP bootstrap: redirects `/domain/{name}` to the
/// authoritative registry's RDAP server.
const BOOTSTRAP: &str = "https://rdap.org/domain";

/// How many bootstrap redirects to follow by hand. The shared HTTP client
/// never auto-follows (probe headers must not cross origins), so the 30x from
/// the bootstrap - occasionally chained by a registry - is followed here.
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
    let mut url = format!("{BOOTSTRAP}/{domain}");
    for _ in 0..=MAX_REDIRECTS {
        let response = client
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/rdap+json")
            .send()
            .await?;
        if response.status().is_redirection() {
            let Some(next) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
            else {
                anyhow::bail!("redirect without a Location header");
            };
            url = next.to_owned();
            continue;
        }
        let status = response.status();
        anyhow::ensure!(status.is_success(), "registry answered HTTP {status}");
        let body: serde_json::Value = response.json().await?;
        return expiration_event(&body)
            .ok_or_else(|| anyhow::anyhow!("no expiration event in the RDAP answer"));
    }
    anyhow::bail!("too many redirects")
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
