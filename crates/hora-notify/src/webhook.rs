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

    #[expect(
        clippy::too_many_lines,
        reason = "one flat arm per event, each a struct update"
    )]
    fn payload(event: Event<'_>) -> Payload<'_> {
        match event {
            Event::Down {
                monitor,
                error,
                cause,
                impacted,
                vantage,
                event,
                local_only,
            } => Payload {
                message: error,
                local_only: Some(local_only),
                cause,
                impacted: if impacted.is_empty() {
                    None
                } else {
                    Some(impacted)
                },
                vantage,
                change: event,
                ..Payload::new("down", monitor)
            },
            Event::Degraded {
                monitor,
                latency_ms,
                detail,
            } => Payload {
                latency_ms,
                message: detail,
                ..Payload::new("degraded", monitor)
            },
            Event::Recovered {
                monitor,
                local_only,
            } => Payload {
                local_only: Some(local_only),
                ..Payload::new("recovered", monitor)
            },
            Event::CertExpiring { monitor, secs_left } => Payload {
                days_left: Some(whole_days(secs_left)),
                secs_left: Some(secs_left),
                ..Payload::new("cert_expiring", monitor)
            },
            Event::DomainExpiring {
                monitor,
                domain,
                secs_left,
            } => Payload {
                domain: Some(domain),
                days_left: Some(whole_days(secs_left)),
                secs_left: Some(secs_left),
                ..Payload::new("domain_expiring", monitor)
            },
            Event::ReleaseAvailable(release) => Payload {
                project: Some(release.project),
                current: Some(release.current),
                latest: Some(release.latest),
                url: Some(release.url),
                message: release.notes,
                ..Payload::new("release_available", release.monitor)
            },
            Event::Digest { period, summary } => Payload {
                message: Some(summary),
                period: Some(period),
                ..Payload::new("digest", "*")
            },
            Event::PeerLinkDegraded { peer, witness } => Payload {
                witness: Some(witness),
                ..Payload::new("peer_link_degraded", peer)
            },
            Event::CertUnreadable { monitor, error } => Payload {
                message: Some(error),
                ..Payload::new("cert_unreadable", monitor)
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
            Event::Alert {
                monitor,
                severity,
                title,
                message,
            } => Payload {
                severity: Some(severity.as_str()),
                title: Some(title),
                message: (!message.is_empty()).then_some(message),
                ..Payload::new("alert", monitor)
            },
        }
    }
}

/// `secs` in whole days, rounded towards negative infinity. Plain division
/// truncates towards zero, which made `days_left` 0 both for a certificate
/// with 23 hours left and for one expired 23 hours ago; a consumer testing
/// `days_left < 0` now sees the second.
const fn whole_days(secs: i64) -> i64 {
    secs.div_euclid(86_400)
}

#[derive(Serialize)]
struct Payload<'a> {
    event: &'static str,
    monitor: &'a str,
    /// Severity of a pushed alert (`info`/`warning`/`error`/`critical`).
    #[serde(skip_serializing_if = "Option::is_none")]
    severity: Option<&'a str>,
    /// The title of a pushed alert.
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cause: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    impacted: Option<&'a [&'a str]>,
    /// On down and recovered events: the down was seen from this node only
    /// (every peer that answered sees the target up); with
    /// `alerts.notify_unconfirmed` set it went to those channels rather than
    /// the usual ones.
    #[serde(skip_serializing_if = "Option::is_none")]
    local_only: Option<bool>,
    /// Multi-vantage verdict, on down events when peers were asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    vantage: Option<&'a str>,
    /// Correlated event marker, on down events when one was recorded shortly
    /// before ("deploy api v2.3, 3 min before").
    #[serde(skip_serializing_if = "Option::is_none")]
    change: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    witness: Option<&'a str>,
    /// The registered domain, on domain-expiry events.
    #[serde(skip_serializing_if = "Option::is_none")]
    domain: Option<&'a str>,
    /// The watched project (`owner/repo`), on release events.
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<&'a str>,
    /// The version that runs, on release events.
    #[serde(skip_serializing_if = "Option::is_none")]
    current: Option<&'a str>,
    /// The latest published release, on release events.
    #[serde(skip_serializing_if = "Option::is_none")]
    latest: Option<&'a str>,
    /// The release's page, on release events.
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<&'a str>,
    /// The covered period, on digest events.
    #[serde(skip_serializing_if = "Option::is_none")]
    period: Option<&'a str>,
    /// Whole days until the expiry, rounded down: 0 during the last day, -1
    /// from the moment it has passed (see [`whole_days`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    days_left: Option<i64>,
    /// Seconds until the expiry, negative once it has passed; on the events
    /// that carry `days_left`.
    #[serde(skip_serializing_if = "Option::is_none")]
    secs_left: Option<i64>,
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
            severity: None,
            title: None,
            message: None,
            cause: None,
            impacted: None,
            local_only: None,
            vantage: None,
            change: None,
            witness: None,
            domain: None,
            project: None,
            current: None,
            latest: None,
            url: None,
            period: None,
            days_left: None,
            secs_left: None,
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

    async fn notify(&self, event: Event<'_>) -> anyhow::Result<()> {
        let payload = Self::payload(event);
        post_json(
            &self.client,
            &self.url,
            &payload,
            self.name(),
            &[self.url.as_str()],
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AlertSeverity;

    #[test]
    fn a_release_payload_names_what_is_out_and_what_runs() {
        let payload = WebhookNotifier::payload(Event::ReleaseAvailable(crate::Release {
            monitor: "Chat",
            project: "matrix-construct/tuwunel",
            current: "1.9.1",
            latest: "v1.9.2",
            url: "https://github.com/matrix-construct/tuwunel/releases/tag/v1.9.2",
            notes: None,
        }));
        assert_eq!(
            serde_json::to_value(&payload).unwrap(),
            serde_json::json!({
                "event": "release_available",
                "monitor": "Chat",
                "project": "matrix-construct/tuwunel",
                "current": "1.9.1",
                "latest": "v1.9.2",
                "url": "https://github.com/matrix-construct/tuwunel/releases/tag/v1.9.2",
            })
        );
    }

    #[test]
    fn payload_per_event() {
        let down = WebhookNotifier::payload(Event::down("API", Some("boom")));
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
            vantage: None,
            event: None,
            local_only: false,
        });
        assert_eq!(symptom.cause, Some("DB"));

        let root = WebhookNotifier::payload(Event::Down {
            monitor: "DB",
            error: Some("refused"),
            cause: None,
            impacted: &["API", "Web"],
            vantage: None,
            event: None,
            local_only: false,
        });
        assert_eq!(root.impacted, Some(["API", "Web"].as_slice()));

        let cert = WebhookNotifier::payload(Event::CertExpiring {
            monitor: "API",
            secs_left: 5 * 86_400 + 7,
        });
        assert_eq!(cert.event, "cert_expiring");
        assert_eq!(cert.days_left, Some(5));
        assert_eq!(cert.secs_left, Some(5 * 86_400 + 7));

        // The last day reads 0, an expired certificate -1: never the same.
        let days = |secs_left| {
            let payload = WebhookNotifier::payload(Event::CertExpiring {
                monitor: "API",
                secs_left,
            });
            (payload.days_left, payload.secs_left)
        };
        assert_eq!(days(23 * 3600), (Some(0), Some(23 * 3600)));
        assert_eq!(days(-23 * 3600), (Some(-1), Some(-23 * 3600)));
        assert_eq!(days(-86_400), (Some(-1), Some(-86_400)));
        assert_eq!(days(-86_401), (Some(-2), Some(-86_401)));
        let domain = WebhookNotifier::payload(Event::DomainExpiring {
            monitor: "API",
            domain: "example.com",
            secs_left: -1,
        });
        assert_eq!((domain.days_left, domain.secs_left), (Some(-1), Some(-1)));

        let degraded = WebhookNotifier::payload(Event::Degraded {
            monitor: "API",
            latency_ms: Some(1234),
            detail: Some("disk 91% full"),
        });
        assert_eq!(degraded.message, Some("disk 91% full"));
        assert_eq!(degraded.event, "degraded");
        assert_eq!(degraded.latency_ms, Some(1234));

        let partition = WebhookNotifier::payload(Event::PeerLinkDegraded {
            peer: "Hora B",
            witness: "Hora C",
        });
        assert_eq!(partition.event, "peer_link_degraded");
        assert_eq!(partition.monitor, "Hora B");
        assert_eq!(partition.witness, Some("Hora C"));

        let alert = WebhookNotifier::payload(Event::Alert {
            monitor: "ekb-api",
            severity: AlertSeverity::Error,
            title: "Patch materialization failed",
            message: "3/12 operations failed",
        });
        assert_eq!(alert.event, "alert");
        assert_eq!(alert.monitor, "ekb-api");
        assert_eq!(alert.severity, Some("error"));
        assert_eq!(alert.title, Some("Patch materialization failed"));
        assert_eq!(alert.message, Some("3/12 operations failed"));

        // An alert with only a title omits the message field entirely.
        let titled = WebhookNotifier::payload(Event::Alert {
            monitor: "ekb-api",
            severity: AlertSeverity::Info,
            title: "deploy started",
            message: "",
        });
        assert!(titled.message.is_none());
    }
}
