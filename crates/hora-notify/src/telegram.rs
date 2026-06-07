//! Telegram Bot API notifier.

use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;
use tracing::warn;

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
            Event::CertExpiring { monitor, days_left } => {
                let when = if days_left <= 0 {
                    "has expired".to_owned()
                } else if days_left == 1 {
                    "expires in 1 day".to_owned()
                } else {
                    format!("expires in {days_left} days")
                };
                format!(
                    "\u{1F510} <b>{}</b> TLS certificate {when}",
                    escape(monitor)
                )
            }
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
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let body = SendMessage {
            chat_id: &self.chat_id,
            text: &text,
            parse_mode: "HTML",
        };
        match self.client.post(url).json(&body).send().await {
            Ok(response) if !response.status().is_success() => {
                warn!(status = %response.status(), "telegram rejected the message");
            }
            Ok(_) => {}
            // reqwest errors embed the request URL, which contains the bot token -
            // strip it so it never reaches the logs.
            Err(err) => warn!(
                "telegram request failed: {}",
                err.to_string().replace(&self.token, "<redacted>")
            ),
        }
    }
}

/// Escape the characters that are special in Telegram's HTML parse mode.
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
    fn escapes_html() {
        assert_eq!(escape("a<b>&c"), "a&lt;b&gt;&amp;c");
    }

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
