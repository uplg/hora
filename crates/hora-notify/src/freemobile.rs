//! Free Mobile SMS notifier.
//!
//! Free Mobile subscribers can enable "Notifications par SMS" in their account
//! and get an API key. A simple GET to `smsapi.free-mobile.fr/sendmsg` then texts
//! the subscriber's own number. See
//! <https://mobile.free.fr/account/mes-options/notifications-sms>.

use async_trait::async_trait;
use reqwest::Client;

use crate::message::Message;
use crate::util::{Unit, send_retrying};
use crate::{Event, Notifier};

const ENDPOINT: &str = "https://smsapi.free-mobile.fr/sendmsg";

/// About three concatenated SMS parts (153 GSM-7 characters each): enough
/// for any alert, and a digest is better read elsewhere than as ten texts.
const HEAD_MAX: usize = 120;
const BODY_MAX: usize = 340;

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

    /// Plain text, no emoji: an SMS with any non-GSM character is billed as the
    /// pricier UCS-2 encoding (70 chars per part instead of 160) - which is
    /// also why a cut ends with `...` rather than the `…` character.
    fn render(event: Event<'_>) -> String {
        Message::render(event)
            .fit(HEAD_MAX, BODY_MAX, Unit::Chars)
            .plain()
            .replace('…', "...")
    }
}

#[async_trait]
impl Notifier for FreeMobileNotifier {
    fn name(&self) -> &'static str {
        "freemobile"
    }

    async fn notify(&self, event: Event<'_>) -> anyhow::Result<()> {
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
            self.name(),
            &[self.pass.as_str(), self.user.as_str()],
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_each_event_plain() {
        let down = FreeMobileNotifier::render(Event::down("API", Some("boom")));
        assert_eq!(down, "API is DOWN\nboom");

        let recovered = FreeMobileNotifier::render(Event::Recovered {
            monitor: "API",
            local_only: false,
        });
        assert!(recovered.contains("recovered"));

        let cert = FreeMobileNotifier::render(Event::CertExpiring {
            monitor: "API",
            secs_left: 3 * 86_400,
        });
        assert!(cert.contains("expires in 3 d"));
        // No emoji, to keep the SMS in the cheaper GSM-7 encoding.
        assert!(down.is_ascii() && recovered.is_ascii() && cert.is_ascii());
    }

    #[test]
    fn keeps_fingerprints_and_caps_length() {
        // The fingerprints used to be dropped from the SMS.
        let changed = FreeMobileNotifier::render(Event::CertChanged {
            monitor: "API",
            old_fingerprint: "aa:bb",
            new_fingerprint: "cc:dd",
        });
        assert!(changed.contains("old: aa:bb\nnew: cc:dd"), "{changed}");

        let summary = "x".repeat(5000);
        let digest = FreeMobileNotifier::render(Event::Digest {
            period: "week",
            summary: &summary,
        });
        assert!(digest.len() <= HEAD_MAX + BODY_MAX + 3, "{}", digest.len());
        assert!(digest.is_ascii() && digest.ends_with("..."));
    }
}
