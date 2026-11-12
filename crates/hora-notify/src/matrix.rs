//! Matrix notifier (client-server API).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use reqwest::{Client, Url};
use serde::Serialize;

use crate::message::{Markup, Message};
use crate::util::{Unit, send_retrying};
use crate::{Event, Notifier};

/// No hard limit short of the 64 KiB event size, but a wall of text helps no
/// one in a room: cap it like the other chat channels, generously.
const HEAD_MAX: usize = 512;
const BODY_MAX: usize = 15_000;

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

    /// `…/_matrix/client/v3/rooms/{roomId}/send/m.room.message/{txnId}`, with the
    /// room id appended as one path segment (`!` and `:` need no escaping there).
    /// `None` only if the homeserver is not a usable base URL (validated at
    /// config load).
    ///
    /// The spec endpoint is a `PUT` with a client-chosen transaction id; the
    /// `POST …/send/m.room.message` form without one is a Synapse leniency that
    /// Tuwunel / Conduit answer with `404 M_UNRECOGNIZED`.
    fn message_url(&self, txn_id: &str) -> Option<Url> {
        let mut url = Url::parse(&self.homeserver).ok()?;
        url.path_segments_mut().ok()?.pop_if_empty().extend([
            "_matrix",
            "client",
            "v3",
            "rooms",
            self.room_id.as_str(),
            "send",
            "m.room.message",
            txn_id,
        ]);
        Some(url)
    }

    /// Transaction id unique per notification (per access token, per the spec):
    /// wall-clock nanoseconds plus a process-wide counter, so two events rendered
    /// in the same instant never collide. Retries of one notification reuse it
    /// on purpose: the homeserver then deduplicates a message it already stored
    /// when only the response was lost.
    fn txn_id() -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        format!("hora-{nanos}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    /// Plain `m.text`: the icon and headline, then the body lines.
    fn render(event: Event<'_>) -> String {
        Message::render(event)
            .fit(HEAD_MAX, BODY_MAX, Unit::Chars)
            .text_with(&Markup::PLAIN)
    }
}

#[derive(Serialize)]
struct RoomMessage<'a> {
    msgtype: &'a str,
    body: &'a str,
}

#[async_trait]
impl Notifier for MatrixNotifier {
    fn name(&self) -> &'static str {
        "matrix"
    }

    async fn notify(&self, event: Event<'_>) -> anyhow::Result<()> {
        let Some(url) = self.message_url(&Self::txn_id()) else {
            tracing::warn!("{}: homeserver is not a valid URL, skipping", self.name());
            anyhow::bail!("homeserver is not a valid URL");
        };
        let url = url.to_string();
        let text = Self::render(event);
        let body = RoomMessage {
            msgtype: "m.text",
            body: &text,
        };
        // The access token rides in the `Authorization` header (not the URL), but
        // pass it as the redaction secret so it can never surface in a log line.
        send_retrying(
            || {
                self.client
                    .put(&url)
                    .bearer_auth(&self.access_token)
                    .json(&body)
            },
            self.name(),
            &[self.access_token.as_str()],
        )
        .await
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
            cause: None,
            impacted: &[],
            vantage: None,
            event: None,
            local_only: false,
        });
        assert!(down.contains("is DOWN") && down.contains("boom"));

        let recovered = MatrixNotifier::render(Event::Recovered {
            monitor: "API",
            local_only: false,
        });
        assert!(recovered.contains("recovered"));

        let cert = MatrixNotifier::render(Event::CertExpiring {
            monitor: "API",
            secs_left: 3 * 86_400,
        });
        assert!(cert.contains("expires in 3 d"));

        // Same line structure as every other channel (was "degraded: unreachable…").
        let partition = MatrixNotifier::render(Event::PeerLinkDegraded {
            peer: "Hora B",
            witness: "Hora C",
        });
        assert!(
            partition.starts_with("\u{1F7E1} Hora B link degraded\nunreachable from here"),
            "{partition}"
        );
    }

    #[test]
    fn builds_message_url_under_homeserver() {
        let notifier = MatrixNotifier::new(
            Client::new(),
            "https://matrix.example.org/".to_owned(), // trailing slash
            "tok".to_owned(),
            "!abc:matrix.example.org".to_owned(),
        );
        let url = notifier
            .message_url("hora-1-0")
            .expect("valid url")
            .to_string();
        assert!(
            url.starts_with("https://matrix.example.org/_matrix/client/v3/rooms/"),
            "unexpected base: {url}"
        );
        // Spec shape: PUT …/send/{eventType}/{txnId}; `!` and `:` are legal path
        // characters, so the room id travels as one unescaped segment.
        assert!(
            url.ends_with("/rooms/!abc:matrix.example.org/send/m.room.message/hora-1-0"),
            "unexpected tail: {url}"
        );
        // Trailing slash on the homeserver must not produce an empty `rooms//`.
        assert!(!url.contains("rooms//"), "empty segment: {url}");
    }

    #[test]
    fn txn_ids_are_unique_and_path_safe() {
        let a = MatrixNotifier::txn_id();
        let b = MatrixNotifier::txn_id();
        assert_ne!(a, b);
        assert!(a.starts_with("hora-"));
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'));
    }
}
