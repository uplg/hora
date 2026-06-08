//! Slack incoming-webhook notifier.

use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;

use crate::util::{cert_expiry_phrase, escape, latency_suffix, post_json};
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
            Event::Down { monitor, error } => format!(
                ":red_circle: *{}* is DOWN\n```{}```",
                escape(monitor),
                // Neutralise backticks so the error can't break out of the block.
                escape(error.unwrap_or("no response")).replace('`', "'"),
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

    async fn notify(&self, event: Event<'_>) {
        let text = Self::render(event);
        let payload = Payload { text: &text };
        post_json(
            &self.client,
            &self.webhook_url,
            &payload,
            "slack",
            &self.webhook_url,
        )
        .await;
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
