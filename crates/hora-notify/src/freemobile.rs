//! Free Mobile SMS notifier.
//!
//! Free Mobile subscribers can enable "Notifications par SMS" in their account
//! and get an API key. A simple GET to `smsapi.free-mobile.fr/sendmsg` then texts
//! the subscriber's own number. See
//! <https://mobile.free.fr/account/mes-options/notifications-sms>.

use async_trait::async_trait;
use reqwest::Client;

use crate::util::{cert_expiry_phrase, latency_suffix, send_retrying};
use crate::{Event, Notifier};

const ENDPOINT: &str = "https://smsapi.free-mobile.fr/sendmsg";

/// Sends alerts as an SMS to the subscriber's own number via the Free Mobile API.
pub struct FreeMobileNotifier {
    client: Client,
    user: String,
    pass: String,
}

impl FreeMobileNotifier {
    #[must_use]
    pub fn new(client: Client, user: String, pass: String) -> Self {
        Self { client, user, pass }
    }

    /// Plain-text, no emoji: an SMS with any non-GSM character is billed as the
    /// pricier UCS-2 encoding (70 chars per part instead of 160).
    fn render(event: Event<'_>) -> String {
        match event {
            Event::Down { monitor, error } => {
                format!("DOWN: {monitor} - {}", error.unwrap_or("no response"))
            }
            Event::Degraded {
                monitor,
                latency_ms,
            } => format!("SLOW: {monitor}{}", latency_suffix(latency_ms)),
            Event::Recovered { monitor } => format!("UP: {monitor} recovered"),
            Event::CertExpiring { monitor, days_left } => {
                format!("CERT: {monitor} {}", cert_expiry_phrase(days_left))
            }
            Event::PeerLinkDegraded { peer, witness } => {
                format!("LINK: {peer} unreachable here, seen up by {witness}")
            }
        }
    }
}

#[async_trait]
impl Notifier for FreeMobileNotifier {
    fn name(&self) -> &'static str {
        "freemobile"
    }

    async fn notify(&self, event: Event<'_>) {
        let msg = Self::render(event);
        // `pass` travels in the query string, so it is what we redact from logs;
        // reqwest builds and percent-encodes the query for us.
        let params = [
            ("user", self.user.as_str()),
            ("pass", self.pass.as_str()),
            ("msg", msg.as_str()),
        ];
        send_retrying(
            || self.client.get(ENDPOINT).query(&params),
            "freemobile",
            &self.pass,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_each_event_plain() {
        let down = FreeMobileNotifier::render(Event::Down {
            monitor: "API",
            error: Some("boom"),
        });
        assert!(down.starts_with("DOWN: API") && down.contains("boom"));

        let recovered = FreeMobileNotifier::render(Event::Recovered { monitor: "API" });
        assert!(recovered.contains("recovered"));

        let cert = FreeMobileNotifier::render(Event::CertExpiring {
            monitor: "API",
            days_left: 3,
        });
        assert!(cert.contains("expires in 3 days"));
        // No emoji, to keep the SMS in the cheaper GSM-7 encoding.
        assert!(down.is_ascii() && recovered.is_ascii() && cert.is_ascii());
    }
}
