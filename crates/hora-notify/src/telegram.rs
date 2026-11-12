//! Telegram Bot API notifier.

use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;

use crate::message::{Markup, Message};
use crate::util::{Unit, escape, post_json};
use crate::{Event, Notifier};

/// Telegram HTML: `<b>`, `<code>`, everything else escaped.
const HTML: Markup = Markup {
    text: escape,
    code: escape,
    bold: ("<b>", "</b>"),
    block: ("<code>", "</code>"),
    inline: ("<code>", "</code>"),
};

/// `sendMessage` accepts 4096 characters *after* entity parsing, so the
/// markup does not count; the margin covers the icon.
const HEAD_MAX: usize = 512;
const BODY_MAX: usize = 3500;

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
        Message::render(event)
            .fit(HEAD_MAX, BODY_MAX, Unit::Chars)
            .text_with(&HTML)
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

    async fn notify(&self, event: Event<'_>) -> anyhow::Result<()> {
        let text = Self::render(event);
        // The URL embeds the bot token, so it is what we redact from errors.
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let body = SendMessage {
            chat_id: &self.chat_id,
            text: &text,
            parse_mode: "HTML",
        };
        post_json(
            &self.client,
            &url,
            &body,
            self.name(),
            &[self.token.as_str()],
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AlertSeverity;

    #[test]
    fn renders_each_event() {
        let down = TelegramNotifier::render(Event::down("API", Some("boom")));
        assert_eq!(down, "\u{1F534} <b>API</b> is DOWN\n<code>boom</code>");

        let symptom = TelegramNotifier::render(Event::Down {
            monitor: "API",
            error: Some("timeout"),
            cause: Some("DB"),
            impacted: &[],
            vantage: None,
            event: None,
            local_only: false,
        });
        assert!(symptom.contains("caused by DB"));

        let root = TelegramNotifier::render(Event::Down {
            monitor: "DB",
            error: Some("refused"),
            cause: None,
            impacted: &["API", "Web"],
            vantage: None,
            event: None,
            local_only: false,
        });
        assert!(root.contains("impacts 2") && root.contains("API"));

        let recovered = TelegramNotifier::render(Event::Recovered {
            monitor: "API",
            local_only: false,
        });
        assert!(recovered.contains("recovered"));

        let degraded = TelegramNotifier::render(Event::Degraded {
            monitor: "API",
            latency_ms: Some(1234),
            detail: Some("disk 91% full"),
        });
        assert!(degraded.contains("slow") && degraded.contains("1234 ms"));
        assert!(degraded.contains("disk 91% full"), "{degraded}");

        let cert = TelegramNotifier::render(Event::CertExpiring {
            monitor: "API",
            secs_left: 3 * 86_400,
        });
        assert!(cert.contains("expires in 3 d"));

        let partition = TelegramNotifier::render(Event::PeerLinkDegraded {
            peer: "Hora B",
            witness: "Hora C",
        });
        assert!(partition.contains("link degraded") && partition.contains("Hora C"));

        let changed = TelegramNotifier::render(Event::CertChanged {
            monitor: "API",
            old_fingerprint: "aa",
            new_fingerprint: "bb",
        });
        assert!(changed.contains("old: <code>aa</code>\nnew: <code>bb</code>"));
    }

    #[test]
    fn escapes_every_interpolation() {
        let down = TelegramNotifier::render(Event::Down {
            monitor: "<API>",
            error: Some("a<b"),
            cause: Some("D&B"),
            impacted: &[],
            vantage: None,
            event: None,
            local_only: false,
        });
        assert!(down.contains("<b>&lt;API&gt;</b>"));
        assert!(down.contains("<code>a&lt;b</code>"));
        assert!(down.contains("caused by D&amp;B"));
    }

    #[test]
    fn long_alerts_fit_the_message_limit() {
        // Title and detail at the API caps, plus a pile of tags: over 4096.
        let title = "t".repeat(200);
        let message = "m".repeat(5000);
        let text = TelegramNotifier::render(Event::Alert {
            monitor: "API",
            severity: AlertSeverity::Critical,
            title: &title,
            message: &message,
        });
        // Measured without the markup, as Telegram counts it.
        let visible = text.replace("<b>", "").replace("</b>", "");
        assert!(
            visible.chars().count() <= 4096,
            "{}",
            visible.chars().count()
        );
        assert!(text.ends_with('…'));
    }
}
