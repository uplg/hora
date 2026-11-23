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
    options: PushoverOptions,
}

/// A channel's own settings, on top of the event's urgency: Pushover's
/// `priority` (-2 to 2) for every message the channel sends but a digest
/// (a report, never an alert: it stays quiet), a `recovery_priority` for
/// recoveries (unset = `priority`), the `sound`
/// the recipient hears, and for an emergency (priority 2) how often
/// Pushover repeats it until acknowledged (`retry`, seconds) and for how
/// long (`expire`). Unset, the urgency picks the priority and the
/// recipient's settings the sound. Validated with the config.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PushoverOptions {
    pub priority: Option<i8>,
    pub recovery_priority: Option<i8>,
    pub sound: Option<String>,
    pub retry: Option<u32>,
    pub expire: Option<u32>,
}

impl PushoverNotifier {
    #[must_use]
    pub fn new(client: Client, token: String, user: String) -> Self {
        Self {
            client,
            token,
            user,
            options: PushoverOptions::default(),
        }
    }

    /// The channel's own priority, sound and emergency settings.
    #[must_use]
    pub fn with_options(mut self, options: PushoverOptions) -> Self {
        self.options = options;
        self
    }

    /// What is posted for `event`: the message, with the channel's
    /// priority over the event's (its recovery priority for a recovery,
    /// none for a digest), its sound, and the emergency settings when sent
    /// as an emergency.
    fn payload(&self, event: Event<'_>) -> serde_json::Value {
        let own = match event {
            // A report, not an alert: a channel that pages for a down does
            // not page, let alone until acknowledged, for the weekly digest.
            Event::Digest { .. } => None,
            Event::Recovered { .. } => self.options.recovery_priority.or(self.options.priority),
            _ => self.options.priority,
        };
        let (title, message, priority) = Self::message(event);
        let priority = own.unwrap_or(priority);
        let mut payload = serde_json::json!({
            "token": self.token,
            "user": self.user,
            "message": message,
            "title": title,
            "priority": priority,
        });
        if let Some(sound) = &self.options.sound {
            payload["sound"] = sound.as_str().into();
        }
        // Pushover takes them for an emergency only.
        if priority == 2 {
            if let Some(retry) = self.options.retry {
                payload["retry"] = retry.into();
            }
            if let Some(expire) = self.options.expire {
                payload["expire"] = expire.into();
            }
        }
        payload
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
/// (emergency) requires retry/expire parameters, so only a channel that
/// sets them asks for it (see [`PushoverOptions`]).
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
        let payload = self.payload(event);
        let build = || self.client.post(PUSHOVER_API).json(&payload);
        // The user key is a quasi-secret too: it lets anyone message the user.
        send_retrying(
            build,
            self.name(),
            &[self.token.as_str(), self.user.as_str()],
        )
        .await
    }
}

/// What this channel would send for `event`: the fuzz target's way in (see
/// [`crate::fuzz`], built with `--cfg fuzzing` only).
#[cfg(fuzzing)]
pub(crate) fn fuzzed(event: Event<'_>) -> crate::fuzz::Sent {
    let (title, body, _priority) = PushoverNotifier::message(event);
    crate::fuzz::Sent {
        channel: "pushover",
        title: Some(title),
        body,
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

    /// A channel's priority wins over the event's, except for a digest; its
    /// sound and emergency settings ride along, and nothing unset does.
    #[test]
    fn channel_options_shape_the_payload() {
        let digest = || Event::Digest {
            period: "week",
            summary: "ok",
        };
        let plain = PushoverNotifier::new(Client::new(), "t".into(), "u".into());
        let payload = plain.payload(digest());
        assert_eq!(payload["priority"], -1);
        for key in ["sound", "retry", "expire"] {
            assert!(payload.get(key).is_none(), "{key}");
        }
        let emergency = PushoverNotifier::new(Client::new(), "t".into(), "u".into()).with_options(
            PushoverOptions {
                priority: Some(2),
                recovery_priority: None,
                sound: Some("siren".into()),
                retry: Some(60),
                expire: Some(3600),
            },
        );
        let payload = emergency.payload(Event::CertChanged {
            monitor: "API",
            old_fingerprint: "aa",
            new_fingerprint: "bb",
        });
        assert_eq!(payload["priority"], 2);
        assert_eq!(payload["sound"], "siren");
        assert_eq!(payload["retry"], 60);
        assert_eq!(payload["expire"], 3600);
        // The digest stays the quiet report it is, on a channel that pages.
        let payload = emergency.payload(digest());
        assert_eq!(payload["priority"], -1);
        assert!(payload.get("retry").is_none() && payload.get("expire").is_none());
        let quiet = PushoverNotifier::new(Client::new(), "t".into(), "u".into()).with_options(
            PushoverOptions {
                priority: Some(-1),
                sound: Some("none".into()),
                ..PushoverOptions::default()
            },
        );
        let payload = quiet.payload(Event::CertChanged {
            monitor: "API",
            old_fingerprint: "aa",
            new_fingerprint: "bb",
        });
        assert_eq!(payload["priority"], -1, "the channel's, not the event's 1");
        assert_eq!(payload["sound"], "none");
    }

    /// A recovery is sent at the channel's recovery priority, else at its
    /// priority; the emergency settings only ride on an emergency.
    #[test]
    fn recoveries_take_the_recovery_priority() {
        let down = Event::Down {
            monitor: "API",
            error: Some("timeout"),
            cause: None,
            impacted: &[],
            vantage: None,
            event: None,
            local_only: false,
            down_for_secs: None,
        };
        let recovered = Event::Recovered {
            monitor: "API",
            local_only: false,
        };
        let channel = |priority, recovery_priority| {
            PushoverNotifier::new(Client::new(), "t".into(), "u".into()).with_options(
                PushoverOptions {
                    priority,
                    recovery_priority,
                    retry: Some(60),
                    expire: Some(3600),
                    ..PushoverOptions::default()
                },
            )
        };
        let split = channel(Some(2), Some(0));
        let payload = split.payload(down);
        assert_eq!(payload["priority"], 2);
        assert_eq!(payload["retry"], 60);
        assert_eq!(payload["expire"], 3600);
        let payload = split.payload(recovered);
        assert_eq!(payload["priority"], 0);
        assert!(payload.get("retry").is_none() && payload.get("expire").is_none());
        // Unset: the channel's priority, emergency included.
        let payload = channel(Some(2), None).payload(recovered);
        assert_eq!(payload["priority"], 2);
        assert_eq!(payload["retry"], 60);
        // An emergency recovery only; a digest is no recovery.
        let loud_up = channel(Some(1), Some(2));
        assert_eq!(loud_up.payload(recovered)["priority"], 2);
        let payload = loud_up.payload(down);
        assert_eq!(payload["priority"], 1);
        assert!(payload.get("retry").is_none());
        let digest = Event::Digest {
            period: "week",
            summary: "ok",
        };
        assert_eq!(loud_up.payload(digest)["priority"], -1);
        // Neither set: the urgency mapping, recoveries quiet.
        assert_eq!(channel(None, None).payload(recovered)["priority"], -1);
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
