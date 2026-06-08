//! Generic JSON webhook notifier: POSTs a structured event for any consumer.

use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;

use crate::util::post_json;
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
            Event::Down {
                monitor,
                error,
                cause,
                impacted,
            } => Payload {
                event: "down",
                monitor,
                message: error,
                cause,
                impacted: if impacted.is_empty() {
                    None
                } else {
                    Some(impacted)
                },
                witness: None,
                days_left: None,
                latency_ms: None,
            },
            Event::Degraded {
                monitor,
                latency_ms,
            } => Payload {
                event: "degraded",
                monitor,
                message: None,
                cause: None,
                impacted: None,
                witness: None,
                days_left: None,
                latency_ms,
            },
            Event::Recovered { monitor } => Payload {
                event: "recovered",
                monitor,
                message: None,
                cause: None,
                impacted: None,
                witness: None,
                days_left: None,
                latency_ms: None,
            },
            Event::CertExpiring { monitor, days_left } => Payload {
                event: "cert_expiring",
                monitor,
                message: None,
                cause: None,
                impacted: None,
                witness: None,
                days_left: Some(days_left),
                latency_ms: None,
            },
            Event::PeerLinkDegraded { peer, witness } => Payload {
                event: "peer_link_degraded",
                monitor: peer,
                message: None,
                cause: None,
                impacted: None,
                witness: Some(witness),
                days_left: None,
                latency_ms: None,
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
    cause: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    impacted: Option<&'a [&'a str]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    witness: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    days_left: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latency_ms: Option<i64>,
}

#[async_trait]
impl Notifier for WebhookNotifier {
    fn name(&self) -> &'static str {
        "webhook"
    }

    async fn notify(&self, event: Event<'_>) {
        let payload = Self::payload(event);
        post_json(&self.client, &self.url, &payload, "webhook", &self.url).await;
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
            cause: None,
            impacted: &[],
        });
        assert_eq!(down.event, "down");
        assert_eq!(down.monitor, "API");
        assert_eq!(down.message, Some("boom"));
        assert!(down.cause.is_none());
        assert!(down.impacted.is_none());

        let symptom = WebhookNotifier::payload(Event::Down {
            monitor: "API",
            error: Some("timeout"),
            cause: Some("DB"),
            impacted: &[],
        });
        assert_eq!(symptom.cause, Some("DB"));

        let root = WebhookNotifier::payload(Event::Down {
            monitor: "DB",
            error: Some("refused"),
            cause: None,
            impacted: &["API", "Web"],
        });
        assert_eq!(root.impacted, Some(["API", "Web"].as_slice()));

        let cert = WebhookNotifier::payload(Event::CertExpiring {
            monitor: "API",
            days_left: 5,
        });
        assert_eq!(cert.event, "cert_expiring");
        assert_eq!(cert.days_left, Some(5));

        let degraded = WebhookNotifier::payload(Event::Degraded {
            monitor: "API",
            latency_ms: Some(1234),
        });
        assert_eq!(degraded.event, "degraded");
        assert_eq!(degraded.latency_ms, Some(1234));

        let partition = WebhookNotifier::payload(Event::PeerLinkDegraded {
            peer: "Hora B",
            witness: "Hora C",
        });
        assert_eq!(partition.event, "peer_link_degraded");
        assert_eq!(partition.monitor, "Hora B");
        assert_eq!(partition.witness, Some("Hora C"));
    }
}
