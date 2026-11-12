//! Pushover notifier: POSTs a message to the Pushover API.

use async_trait::async_trait;
use reqwest::Client;

use crate::message::{Message, Urgency};
use crate::util::{Unit, send_retrying};
use crate::{Event, Notifier};

const PUSHOVER_API: &str = "https://api.pushover.net/1/messages.json";

/// Pushover rejects a title over 250 characters and a message over 1024.
const HEAD_MAX: usize = 250;
const BODY_MAX: usize = 1024;

pub struct PushoverNotifier {
    client: Client,
    token: String,
    user: String,
}

impl PushoverNotifier {
    #[must_use]
    pub fn new(client: Client, token: String, user: String) -> Self {
        Self {
            client,
            token,
            user,
        }
    }

    /// The title (the event's headline), the message under it and the
    /// priority.
    fn message(event: Event<'_>) -> (String, String, i8) {
        let message = Message::render(event).fit(HEAD_MAX, BODY_MAX, Unit::Chars);
        let (title, text) = message.titled();
        (title, text, priority(message.urgency()))
    }
}

/// Pushover priority: -1 quiet, 0 normal, 1 high. We stop at 1 - priority 2
/// (emergency) would require retry/expire parameters.
fn priority(urgency: Urgency) -> i8 {
    match urgency {
        Urgency::Quiet => -1,
        Urgency::Notice | Urgency::Warning => 0,
        Urgency::High | Urgency::Critical => 1,
    }
}

#[async_trait]
impl Notifier for PushoverNotifier {
    fn name(&self) -> &'static str {
        "pushover"
    }

    async fn notify(&self, event: Event<'_>) -> anyhow::Result<()> {
        let (title, message, priority) = Self::message(event);
        let build = || {
            self.client.post(PUSHOVER_API).json(&serde_json::json!({
                "token": self.token,
                "user": self.user,
                "message": message,
                "title": title,
                "priority": priority,
            }))
        };
        // The user key is a quasi-secret too: it lets anyone message the user.
        send_retrying(
            build,
            self.name(),
            &[self.token.as_str(), self.user.as_str()],
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AlertSeverity;

    #[test]
    fn maps_events_to_text_and_priority() {
        let (title, text, priority) = PushoverNotifier::message(Event::CertChanged {
            monitor: "API",
            old_fingerprint: "aa",
            new_fingerprint: "bb",
        });
        // The notification list names the monitor; the body does not repeat it.
        assert_eq!(title, "API TLS certificate changed unexpectedly");
        assert!(text.starts_with("old: aa\nnew: bb"), "{text}");
        assert_eq!(priority, 1);

        let (_, _, priority) = PushoverNotifier::message(Event::Digest {
            period: "week",
            summary: "ok",
        });
        assert_eq!(priority, -1);
    }

    #[test]
    fn long_alerts_fit_the_title_and_message_limits() {
        let name = "n".repeat(300);
        let message = "m".repeat(3000);
        let (title, text, _) = PushoverNotifier::message(Event::Alert {
            monitor: &name,
            severity: AlertSeverity::Error,
            title: "disk full",
            message: &message,
        });
        assert!(title.chars().count() <= 250, "{}", title.chars().count());
        assert!(title.starts_with("[ERROR] nnn"));
        assert!(text.chars().count() <= 1024, "{}", text.chars().count());
    }
}
