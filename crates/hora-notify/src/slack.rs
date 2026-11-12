//! Slack incoming-webhook notifier.

use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;

use crate::message::{Markup, Message};
use crate::util::{Unit, escape, post_json};
use crate::{Event, Notifier};

/// Slack mrkdwn: `& < >` escaped everywhere, `*bold*`, backtick code (a
/// backtick inside code would close it early).
const MRKDWN: Markup = Markup {
    text: escape,
    code: escape_code,
    bold: ("*", "*"),
    block: ("```", "```"),
    inline: ("`", "`"),
};

fn escape_code(code: &str) -> String {
    escape(code).replace('`', "'")
}

/// Slack cuts `text` at 40 000 characters; escaping can grow the text up to
/// five-fold (`&` -> `&amp;`), so cap the raw text well below a fifth of it.
const HEAD_MAX: usize = 512;
const BODY_MAX: usize = 7000;

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
        Message::render(event)
            .fit(HEAD_MAX, BODY_MAX, Unit::Chars)
            .text_with(&MRKDWN)
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
            self.name(),
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
            event: None,
            local_only: false,
        });
        assert_eq!(down, "\u{1F534} *API* is DOWN\n```boom```");

        let recovered = SlackNotifier::render(Event::Recovered {
            monitor: "API",
            local_only: false,
        });
        assert!(recovered.contains("recovered"));

        let cert = SlackNotifier::render(Event::CertExpiring {
            monitor: "API",
            secs_left: 3 * 86_400,
        });
        assert!(cert.contains("expires in 3 d"));
    }

    #[test]
    fn escapes_topology_and_fingerprints() {
        // The topology line and the fingerprints used to go out unescaped.
        let down = SlackNotifier::render(Event::Down {
            monitor: "API",
            error: Some("x"),
            cause: Some("<!channel>"),
            impacted: &[],
            vantage: None,
            event: None,
            local_only: false,
        });
        assert!(down.contains("caused by &lt;!channel&gt;"), "{down}");

        let changed = SlackNotifier::render(Event::CertChanged {
            monitor: "API",
            old_fingerprint: "<a>",
            new_fingerprint: "b`c",
        });
        assert!(changed.contains("old: `&lt;a&gt;`"), "{changed}");
        assert!(changed.contains("new: `b'c`"), "{changed}");
    }
}
