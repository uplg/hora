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
                message: error,
                cause,
                impacted: if impacted.is_empty() {
                    None
                } else {
                    Some(impacted)
                },
                ..Payload::new("down", monitor)
            },
            Event::Degraded {
                monitor,
                latency_ms,
            } => Payload {
                latency_ms,
                ..Payload::new("degraded", monitor)
            },
            Event::Recovered { monitor } => Payload::new("recovered", monitor),
            Event::CertExpiring { monitor, days_left } => Payload {
                days_left: Some(days_left),
                ..Payload::new("cert_expiring", monitor)
            },
            Event::PeerLinkDegraded { peer, witness } => Payload {
                witness: Some(witness),
                ..Payload::new("peer_link_degraded", peer)
            },
            Event::CertChanged {
                monitor,
                old_fingerprint,
                new_fingerprint,
            } => Payload {
                old_fingerprint: Some(old_fingerprint),
                new_fingerprint: Some(new_fingerprint),
                ..Payload::new("cert_changed", monitor)
            },
            Event::BudgetBurn {
                monitor,
                burn_rate_x10,
                window,
                exhausted_in_secs,
            } => Payload {
                burn_rate_x10: Some(burn_rate_x10),
                burn_window: Some(window),
                exhausted_in_secs,
                ..Payload::new("budget_burn", monitor)
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
    #[serde(skip_serializing_if = "Option::is_none")]
    old_fingerprint: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_fingerprint: Option<&'a str>,
    /// Burn rate in tenths of the sustainable rate (144 = 14.4x).
    #[serde(skip_serializing_if = "Option::is_none")]
    burn_rate_x10: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    burn_window: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exhausted_in_secs: Option<i64>,
}

impl<'a> Payload<'a> {
    /// The bare payload; event-specific fields come in via struct update.
    fn new(event: &'static str, monitor: &'a str) -> Self {
        Self {
            event,
            monitor,
            message: None,
            cause: None,
            impacted: None,
            witness: None,
            days_left: None,
            latency_ms: None,
            old_fingerprint: None,
            new_fingerprint: None,
            burn_rate_x10: None,
            burn_window: None,
            exhausted_in_secs: None,
        }
    }
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
