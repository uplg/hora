//! Gotify notifier: POSTs a message to a Gotify server.

use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;

use crate::message::{Message, Urgency};
use crate::util::{Unit, send_retrying};
use crate::{Event, Notifier};

/// Gotify stores any length, but its clients show a notification, not a
/// document: cap it like the chat channels.
const HEAD_MAX: usize = 512;
const BODY_MAX: usize = 15_000;

pub struct GotifyNotifier {
    client: Client,
    url: String,
    token: String,
}

impl GotifyNotifier {
    #[must_use]
    pub fn new(client: Client, url: String, token: String) -> Self {
        Self { client, url, token }
    }

    fn payload(event: Event<'_>) -> Payload {
        let message = Message::render(event).fit(HEAD_MAX, BODY_MAX, Unit::Chars);
        let (title, text) = message.titled();
        Payload {
            title,
            priority: priority(message.urgency()),
            message: text,
        }
    }
}

/// Gotify priority runs 0..10 (4-7 normal, 8+ high).
fn priority(urgency: Urgency) -> u8 {
    match urgency {
        Urgency::Quiet => 2,
        Urgency::Notice => 3,
        Urgency::Warning => 5,
        Urgency::High => 8,
        Urgency::Critical => 9,
    }
}

#[derive(Serialize)]
struct Payload {
    title: String,
    message: String,
    priority: u8,
}

#[async_trait]
impl Notifier for GotifyNotifier {
    fn name(&self) -> &'static str {
        "gotify"
    }

    async fn notify(&self, event: Event<'_>) -> anyhow::Result<()> {
        let payload = Self::payload(event);
        // The token travels as a header, not in the URL: query strings end up
        // in proxy and server access logs.
        let url = format!("{}/message", self.url.trim_end_matches('/'));
        let build = || {
            self.client
                .post(&url)
                .header("X-Gotify-Key", &self.token)
                .json(&payload)
        };
        send_retrying(build, self.name(), &[self.token.as_str()]).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AlertSeverity;

    #[test]
    fn maps_events_to_text_and_priority() {
        let down = GotifyNotifier::payload(Event::Down {
            monitor: "API",
            error: None,
            cause: None,
            impacted: &[],
            vantage: None,
            event: None,
            local_only: false,
        });
        // The headline is the title, so the body does not say it again.
        assert_eq!(down.title, "API is DOWN");
        assert_eq!(down.message, "no response");
        assert_eq!(down.priority, 8);

        // Gotify refuses an empty message: an event without detail says its
        // headline twice.
        let recovered = GotifyNotifier::payload(Event::Recovered {
            monitor: "API",
            local_only: false,
        });
        assert_eq!(recovered.title, "API recovered");
        assert_eq!(recovered.message, "API recovered");

        let release = GotifyNotifier::payload(Event::ReleaseAvailable(crate::Release {
            monitor: "Chat",
            project: "o/r",
            current: "v1",
            latest: "v2",
            url: "https://x/r",
            notes: None,
        }));
        assert_eq!(release.priority, 3);

        let info = GotifyNotifier::payload(Event::Alert {
            monitor: "API",
            severity: AlertSeverity::Info,
            title: "deploy",
            message: "",
        });
        assert_eq!(info.priority, 2);
    }
}
