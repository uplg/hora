//! Generic JSON webhook notifier: POSTs a structured event for any consumer.

use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;
use tracing::warn;

use crate::{Event, Notifier};

/// Posts each alert as a JSON object to an arbitrary HTTP endpoint.
pub struct WebhookNotifier {
    client: Client,
    url: String,
}

impl WebhookNotifier {
    #[must_use]
    pub fn new(client: Client, url: String) -> Self {
        Self { client, url }
    }

    fn payload(event: Event<'_>) -> Payload<'_> {
        match event {
            Event::Down { monitor, error } => Payload {
                event: "down",
                monitor,
                message: error,
                days_left: None,
            },
            Event::Recovered { monitor } => Payload {
                event: "recovered",
                monitor,
                message: None,
                days_left: None,
            },
            Event::CertExpiring { monitor, days_left } => Payload {
                event: "cert_expiring",
                monitor,
                message: None,
                days_left: Some(days_left),
            },
        }
    }
}

#[derive(Serialize)]
struct Payload<'a> {
    event: &'static str,
    monitor: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    days_left: Option<i64>,
}

#[async_trait]
impl Notifier for WebhookNotifier {
    fn name(&self) -> &'static str {
        "webhook"
    }

    async fn notify(&self, event: Event<'_>) {
        let payload = Self::payload(event);
        match self.client.post(&self.url).json(&payload).send().await {
            Ok(response) if !response.status().is_success() => {
                warn!(status = %response.status(), "webhook rejected the request");
            }
            Ok(_) => {}
            // The URL may embed a secret; strip it from any error.
            Err(err) => warn!(
                "webhook request failed: {}",
                err.to_string().replace(&self.url, "<redacted>")
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_per_event() {
        let down = WebhookNotifier::payload(Event::Down {
            monitor: "API",
            error: Some("boom"),
        });
        assert_eq!(down.event, "down");
        assert_eq!(down.monitor, "API");
        assert_eq!(down.message, Some("boom"));

        let cert = WebhookNotifier::payload(Event::CertExpiring {
            monitor: "API",
            days_left: 5,
        });
        assert_eq!(cert.event, "cert_expiring");
        assert_eq!(cert.days_left, Some(5));
    }
}
