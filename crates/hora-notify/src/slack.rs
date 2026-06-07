//! Slack incoming-webhook notifier.

use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;
use tracing::warn;

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
            Event::Recovered { monitor } => {
                format!(":large_green_circle: *{}* recovered", escape(monitor))
            }
            Event::CertExpiring { monitor, days_left } => {
                let when = if days_left <= 0 {
                    "has expired".to_owned()
                } else if days_left == 1 {
                    "expires in 1 day".to_owned()
                } else {
                    format!("expires in {days_left} days")
                };
                format!(":lock: *{}* TLS certificate {when}", escape(monitor))
            }
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
        match self
            .client
            .post(&self.webhook_url)
            .json(&Payload { text: &text })
            .send()
            .await
        {
            Ok(response) if !response.status().is_success() => {
                warn!(status = %response.status(), "slack rejected the message");
            }
            Ok(_) => {}
            // The webhook URL embeds a secret token; strip it from any error.
            Err(err) => warn!(
                "slack request failed: {}",
                err.to_string().replace(&self.webhook_url, "<redacted>")
            ),
        }
    }
}

/// Escape the characters Slack treats specially in message text.
fn escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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

    #[test]
    fn escapes_metacharacters() {
        assert_eq!(escape("a<b>&c"), "a&lt;b&gt;&amp;c");
    }
}
