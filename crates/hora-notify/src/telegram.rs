//! Telegram Bot API notifier.

use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;

use crate::util::{cert_expiry_phrase, escape, post_json};
use crate::{Event, Notifier};

/// Sends alerts to a Telegram chat via the Bot API.
pub struct TelegramNotifier {
    client: Client,
    token: String,
    chat_id: String,
}

impl TelegramNotifier {
    #[must_use]
    pub fn new(client: Client, token: String, chat_id: String) -> Self {
        Self {
            client,
            token,
            chat_id,
        }
    }

    fn render(event: Event<'_>) -> String {
        match event {
            Event::Down { monitor, error } => format!(
                "\u{1F534} <b>{}</b> is DOWN\n<code>{}</code>",
                escape(monitor),
                escape(error.unwrap_or("no response")),
            ),
            Event::Recovered { monitor } => {
                format!("\u{1F7E2} <b>{}</b> recovered", escape(monitor))
            }
            Event::CertExpiring { monitor, days_left } => format!(
                "\u{1F510} <b>{}</b> TLS certificate {}",
                escape(monitor),
                cert_expiry_phrase(days_left)
            ),
        }
    }
}

#[derive(Serialize)]
struct SendMessage<'a> {
    chat_id: &'a str,
    text: &'a str,
    parse_mode: &'a str,
}

#[async_trait]
impl Notifier for TelegramNotifier {
    fn name(&self) -> &'static str {
        "telegram"
    }

    async fn notify(&self, event: Event<'_>) {
        let text = Self::render(event);
        // The URL embeds the bot token, so it is what we redact from errors.
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let body = SendMessage {
            chat_id: &self.chat_id,
            text: &text,
            parse_mode: "HTML",
        };
        post_json(&self.client, &url, &body, "telegram", &self.token).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_each_event() {
        let down = TelegramNotifier::render(Event::Down {
            monitor: "API",
            error: Some("boom"),
        });
        assert!(down.contains("is DOWN") && down.contains("boom"));

        let recovered = TelegramNotifier::render(Event::Recovered { monitor: "API" });
        assert!(recovered.contains("recovered"));

        let cert = TelegramNotifier::render(Event::CertExpiring {
            monitor: "API",
            days_left: 3,
        });
        assert!(cert.contains("expires in 3 days"));
    }
}
