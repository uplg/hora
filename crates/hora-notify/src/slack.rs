//! Slack incoming-webhook notifier.

use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;

use crate::util::{
    budget_burn_phrase, cert_expiry_phrase, domain_expiry_phrase, escape, latency_suffix,
    post_json, topology_suffix, vantage_suffix,
};
use crate::{Event, Notifier};

/// Posts alerts to a Slack channel through an incoming webhook.
pub struct SlackNotifier {
    client: Client,
    webhook_url: String,
}

impl SlackNotifier {
    #[must_use]
    pub fn new(client: Client, webhook_url: String) -> Self {
        Self {
            client,
            webhook_url,
        }
    }

    fn render(event: Event<'_>) -> String {
        match event {
            Event::Down {
                monitor,
                error,
                cause,
                impacted,
                vantage,
            } => format!(
                ":red_circle: *{}* is DOWN\n```{}```{}{}",
                escape(monitor),
                escape(error.unwrap_or("no response")).replace('`', "'"),
                topology_suffix(cause, impacted),
                escape(&vantage_suffix(vantage)),
            ),
            Event::Degraded {
                monitor,
                latency_ms,
            } => format!(
                ":large_orange_circle: *{}* is slow{}",
                escape(monitor),
                latency_suffix(latency_ms)
            ),
            Event::Recovered { monitor } => {
                format!(":large_green_circle: *{}* recovered", escape(monitor))
            }
            Event::CertExpiring { monitor, days_left } => format!(
                ":lock: *{}* TLS certificate {}",
                escape(monitor),
                cert_expiry_phrase(days_left)
            ),
            Event::DomainExpiring {
                monitor,
                domain,
                days_left,
            } => format!(
                ":globe_with_meridians: *{}* {}",
                escape(monitor),
                escape(&domain_expiry_phrase(domain, days_left))
            ),
            Event::Digest { period, summary } => format!(
                ":bar_chart: *Hora digest* ({})\n{}",
                escape(period),
                escape(summary)
            ),
            Event::PeerLinkDegraded { peer, witness } => format!(
                ":large_yellow_circle: *{}* link degraded\nunreachable from here, but seen up by {}",
                escape(peer),
                escape(witness),
            ),
            Event::CertChanged {
                monitor,
                old_fingerprint,
                new_fingerprint,
            } => format!(
                ":warning: *{}* TLS certificate changed unexpectedly\nold: `{}`\nnew: `{}`",
                escape(monitor),
                old_fingerprint,
                new_fingerprint,
            ),
            Event::BudgetBurn {
                monitor,
                burn_rate_x10,
                window,
                exhausted_in_secs,
            } => format!(
                ":fire: *{}* {}",
                escape(monitor),
                budget_burn_phrase(burn_rate_x10, window, exhausted_in_secs),
            ),
        }
    }
}

#[derive(Serialize)]
struct Payload<'a> {
    text: &'a str,
}

#[async_trait]
impl Notifier for SlackNotifier {
    fn name(&self) -> &'static str {
        "slack"
    }

    async fn notify(&self, event: Event<'_>) -> anyhow::Result<()> {
        let text = Self::render(event);
        let payload = Payload { text: &text };
        post_json(
            &self.client,
            &self.webhook_url,
            &payload,
            "slack",
            &[self.webhook_url.as_str()],
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_each_event() {
        let down = SlackNotifier::render(Event::Down {
            monitor: "API",
            error: Some("boom"),
            cause: None,
            impacted: &[],
            vantage: None,
        });
        assert!(down.contains("is DOWN") && down.contains("boom"));

        let recovered = SlackNotifier::render(Event::Recovered { monitor: "API" });
        assert!(recovered.contains("recovered"));

        let cert = SlackNotifier::render(Event::CertExpiring {
            monitor: "API",
            days_left: 3,
        });
        assert!(cert.contains("expires in 3 days"));
    }
}
