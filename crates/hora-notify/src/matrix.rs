//! Matrix notifier (client-server API).

use async_trait::async_trait;
use reqwest::{Client, Url};
use serde::Serialize;

use crate::util::{cert_expiry_phrase, send_retrying};
use crate::{Event, Notifier};

/// Posts alerts to a Matrix room as the bot whose access token is configured.
pub struct MatrixNotifier {
    client: Client,
    /// Homeserver base URL, e.g. `https://matrix.org` (a trailing slash is fine).
    homeserver: String,
    access_token: String,
    room_id: String,
}

impl MatrixNotifier {
    #[must_use]
    pub fn new(client: Client, homeserver: String, access_token: String, room_id: String) -> Self {
        Self {
            client,
            homeserver,
            access_token,
            room_id,
        }
    }

    /// `…/_matrix/client/v3/rooms/{roomId}/send/m.room.message`, with the room id
    /// percent-encoded as a path segment (it contains `!` and `:`). `None` only if
    /// the homeserver is not a usable base URL (validated at config load).
    fn message_url(&self) -> Option<Url> {
        let mut url = Url::parse(&self.homeserver).ok()?;
        url.path_segments_mut().ok()?.pop_if_empty().extend([
            "_matrix",
            "client",
            "v3",
            "rooms",
            self.room_id.as_str(),
            "send",
            "m.room.message",
        ]);
        Some(url)
    }

    fn render(event: Event<'_>) -> String {
        match event {
            Event::Down { monitor, error } => format!(
                "\u{1F534} {monitor} is DOWN\n{}",
                error.unwrap_or("no response")
            ),
            Event::Recovered { monitor } => format!("\u{1F7E2} {monitor} recovered"),
            Event::CertExpiring { monitor, days_left } => format!(
                "\u{1F510} {monitor} TLS certificate {}",
                cert_expiry_phrase(days_left)
            ),
        }
    }
}

#[derive(Serialize)]
struct Message<'a> {
    msgtype: &'a str,
    body: &'a str,
}

#[async_trait]
impl Notifier for MatrixNotifier {
    fn name(&self) -> &'static str {
        "matrix"
    }

    async fn notify(&self, event: Event<'_>) {
        let Some(url) = self.message_url() else {
            tracing::warn!("matrix: homeserver is not a valid URL, skipping");
            return;
        };
        let url = url.to_string();
        let text = Self::render(event);
        let body = Message {
            msgtype: "m.text",
            body: &text,
        };
        // The access token rides in the `Authorization` header (not the URL), but
        // pass it as the redaction secret so it can never surface in a log line.
        send_retrying(
            || {
                self.client
                    .post(&url)
                    .bearer_auth(&self.access_token)
                    .json(&body)
            },
            "matrix",
            &self.access_token,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_each_event() {
        let down = MatrixNotifier::render(Event::Down {
            monitor: "API",
            error: Some("boom"),
        });
        assert!(down.contains("is DOWN") && down.contains("boom"));

        let recovered = MatrixNotifier::render(Event::Recovered { monitor: "API" });
        assert!(recovered.contains("recovered"));

        let cert = MatrixNotifier::render(Event::CertExpiring {
            monitor: "API",
            days_left: 3,
        });
        assert!(cert.contains("expires in 3 days"));
    }

    #[test]
    fn builds_message_url_under_homeserver() {
        let notifier = MatrixNotifier::new(
            Client::new(),
            "https://matrix.example.org/".to_owned(), // trailing slash
            "tok".to_owned(),
            "!abc:matrix.example.org".to_owned(),
        );
        let url = notifier.message_url().expect("valid url").to_string();
        assert!(
            url.starts_with("https://matrix.example.org/_matrix/client/v3/rooms/"),
            "unexpected base: {url}"
        );
        assert!(
            url.ends_with("/send/m.room.message"),
            "unexpected tail: {url}"
        );
        // Trailing slash on the homeserver must not produce an empty `rooms//`.
        assert!(!url.contains("rooms//"), "empty segment: {url}");
    }
}
