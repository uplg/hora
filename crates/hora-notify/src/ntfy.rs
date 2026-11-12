//! ntfy notifier: POSTs a plain-text message to an ntfy topic.

use async_trait::async_trait;
use reqwest::Client;
use reqwest::header::HeaderValue;

use crate::message::{Kind, Message, Urgency};
use crate::util::{Unit, send_retrying};
use crate::{AlertSeverity, Event, Notifier};

/// ntfy turns a body over 4096 *bytes* into an attachment (or refuses it when
/// attachments are off), so the text is measured in bytes.
const HEAD_MAX: usize = 256;
const BODY_MAX: usize = 3800;

pub struct NtfyNotifier {
    client: Client,
    url: String,
    token: Option<String>,
}

impl NtfyNotifier {
    #[must_use]
    pub fn new(client: Client, url: String, token: Option<String>) -> Self {
        Self { client, url, token }
    }

    /// The event as ntfy takes it: the headline as the `Title` header, the
    /// detail as the body.
    fn message(event: Event<'_>) -> Push {
        let message = Message::render(event).fit(HEAD_MAX, BODY_MAX, Unit::Bytes);
        let (title, body) = match HeaderValue::from_str(&message.headline()) {
            Ok(title) => (title, message.titled().1),
            // A header cannot carry a control character (a monitor name with
            // a newline or an escape in it): the title goes back to a fixed
            // one and the body says everything, headline first, so the alert
            // still names its monitor.
            Err(_) => (HeaderValue::from_static(FALLBACK_TITLE), message.plain()),
        };
        Push {
            title,
            body,
            tags: tag(message.kind),
            priority: priority(message.urgency()),
        }
    }
}

/// The title of a notification whose headline no header can carry.
const FALLBACK_TITLE: &str = "Hora Alert";

/// One ntfy publication: the body and the headers that dress it.
struct Push {
    title: HeaderValue,
    body: String,
    /// The `Tags` header, an emoji shortcode.
    tags: &'static str,
    priority: u8,
}

/// ntfy shows the tag as an emoji in front of the title.
fn tag(kind: Kind) -> &'static str {
    match kind {
        Kind::Down | Kind::Alert(AlertSeverity::Error | AlertSeverity::Critical) => {
            "rotating_light"
        }
        Kind::Degraded | Kind::PeerLinkDegraded | Kind::Alert(AlertSeverity::Warning) => "warning",
        Kind::Recovered => "white_check_mark",
        Kind::CertExpiring | Kind::CertChanged | Kind::CertUnreadable => "lock",
        Kind::DomainExpiring => "globe_with_meridians",
        Kind::Release => "package",
        Kind::Digest => "bar_chart",
        Kind::BudgetBurn => "fire",
        Kind::Alert(AlertSeverity::Info) => "information_source",
    }
}

/// ntfy priority runs 1 (min) to 5 (max).
fn priority(urgency: Urgency) -> u8 {
    match urgency {
        Urgency::Quiet => 2,
        Urgency::Notice | Urgency::Warning => 3,
        Urgency::High => 4,
        Urgency::Critical => 5,
    }
}

#[async_trait]
impl Notifier for NtfyNotifier {
    fn name(&self) -> &'static str {
        "ntfy"
    }

    async fn notify(&self, event: Event<'_>) -> anyhow::Result<()> {
        let push = Self::message(event);
        let build = || {
            let mut req = self.client.post(&self.url).body(push.body.clone());
            req = req.header("Title", push.title.clone());
            req = req.header("Tags", push.tags);
            req = req.header("Priority", push.priority.to_string());
            if let Some(token) = &self.token {
                req = req.bearer_auth(token);
            }
            req
        };
        // The topic URL itself is the capability; the bearer token must not
        // surface either if the server echoes it in a rejection body.
        let mut secrets = vec![self.url.as_str()];
        if let Some(token) = &self.token {
            secrets.push(token.as_str());
        }
        send_retrying(build, self.name(), &secrets).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_events_to_title_text_tags_and_priority() {
        let push = NtfyNotifier::message(Event::Down {
            monitor: "API",
            error: Some("boom"),
            cause: Some("DB"),
            impacted: &[],
            vantage: None,
            event: None,
            local_only: false,
        });
        // The headline is the title; the body does not say it again.
        assert_eq!(push.title, "API is DOWN");
        assert_eq!(push.body, "boom\ncaused by DB");
        assert_eq!((push.tags, push.priority), ("rotating_light", 4));

        let push = NtfyNotifier::message(Event::Recovered {
            monitor: "API",
            local_only: false,
        });
        assert_eq!(push.title, "API recovered");
        assert_eq!((push.tags, push.priority), ("white_check_mark", 2));

        let push = NtfyNotifier::message(Event::Alert {
            monitor: "API",
            severity: AlertSeverity::Critical,
            title: "disk full",
            message: "",
        });
        assert_eq!(push.title, "[CRITICAL] API: disk full");
        assert_eq!((push.tags, push.priority), ("rotating_light", 5));
    }

    #[test]
    fn a_name_no_header_can_carry_moves_to_the_body() {
        // UTF-8 travels in a header as it is...
        let push = NtfyNotifier::message(Event::Recovered {
            monitor: "Café ☕",
            local_only: false,
        });
        assert_eq!(push.title.as_bytes(), "Café ☕ recovered".as_bytes());

        // ...a line break does not: it would end the header, or the request.
        let push = NtfyNotifier::message(Event::down("API\r\nX-Injected: 1", Some("boom")));
        assert_eq!(push.title, FALLBACK_TITLE);
        assert!(push.body.starts_with("API\r\nX-Injected: 1 is DOWN\nboom"));
    }

    #[test]
    fn long_bodies_stay_under_the_byte_limit() {
        let summary = "€".repeat(4000); // 12 000 bytes
        let push = NtfyNotifier::message(Event::Digest {
            period: "week",
            summary: &summary,
        });
        assert!(push.body.len() <= 4096, "{} bytes", push.body.len());
    }
}
