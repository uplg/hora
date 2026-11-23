//! Notification channels: where alerts go, and how each is reached.

use serde::Deserialize;

use super::secret::Secret;

/// A named notification channel. Several channels may share a `type` (e.g. two
/// Discord webhooks), and a monitor routes to specific ones by `name`.
///
/// `Debug` is derived: every credential field is a [`Secret`], which redacts
/// itself, so a `{config:?}` in a log line or panic message never leaks a
/// token or webhook URL (pinned by `channel_debug_redacts_secrets`).
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Channel {
    Telegram {
        name: String,
        token: Secret,
        chat_id: String,
    },
    Discord {
        name: String,
        webhook_url: Secret,
    },
    Slack {
        name: String,
        webhook_url: Secret,
    },
    Webhook {
        name: String,
        url: Secret,
    },
    Matrix {
        name: String,
        /// Homeserver base URL, e.g. `https://matrix.org`.
        homeserver: String,
        /// Access token of the sending (bot) user.
        token: Secret,
        /// Target room id, e.g. `!abcdef:matrix.org`.
        room_id: String,
    },
    FreeMobile {
        name: String,
        /// Free Mobile account identifier (the login from the SMS-notifications option).
        user: String,
        /// API key generated in the Free Mobile "Notifications par SMS" option.
        pass: Secret,
    },
    Email {
        name: String,
        host: String,
        #[serde(default = "default_smtp_port")]
        port: u16,
        #[serde(default)]
        username: String,
        #[serde(default)]
        password: Secret,
        from: String,
        to: String,
        /// Implicit TLS (port 465) instead of STARTTLS (the default, port 587).
        #[serde(default)]
        implicit_tls: bool,
    },
    Ntfy {
        name: String,
        /// ntfy topic URL, e.g. `https://ntfy.sh/my-topic`.
        url: Secret,
        /// Optional access token for private ntfy servers.
        #[serde(default)]
        token: Option<Secret>,
    },
    Gotify {
        name: String,
        /// Gotify server URL, e.g. `https://gotify.example.com`.
        url: Secret,
        /// Application token.
        token: Secret,
    },
    Pushover {
        name: String,
        /// Pushover API token (application key).
        token: Secret,
        /// Pushover user key (recipient).
        user: Secret,
        /// Pushover priority (-2 to 2) for every message of this channel
        /// (recoveries: `recovery_priority`, if set; digests stay quiet),
        /// instead of one picked by the event; 2 (emergency) needs `retry`
        /// and `expire`.
        #[serde(default)]
        priority: Option<i8>,
        /// Pushover priority (-2 to 2) for recoveries; unset = `priority`.
        #[serde(default)]
        recovery_priority: Option<i8>,
        /// The sound the recipient hears (`"none"` for silence); unset = the
        /// recipient's own setting.
        #[serde(default)]
        sound: Option<String>,
        /// Either priority 2: seconds between Pushover's repeats until the
        /// message is acknowledged (at least 30).
        #[serde(default)]
        retry: Option<u32>,
        /// Either priority 2: how long Pushover keeps repeating, seconds (at
        /// most 10800).
        #[serde(default)]
        expire: Option<u32>,
    },
}

impl Channel {
    /// A Pushover channel's own priorities, sound and emergency settings
    /// (none for any other channel).
    #[must_use]
    pub fn pushover_options(&self) -> hora_notify::pushover::PushoverOptions {
        match self {
            Self::Pushover {
                priority,
                recovery_priority,
                sound,
                retry,
                expire,
                ..
            } => hora_notify::pushover::PushoverOptions {
                priority: *priority,
                recovery_priority: *recovery_priority,
                sound: sound.clone(),
                retry: *retry,
                expire: *expire,
            },
            _ => hora_notify::pushover::PushoverOptions::default(),
        }
    }

    /// The routing name a monitor refers to.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Telegram { name, .. }
            | Self::Discord { name, .. }
            | Self::Slack { name, .. }
            | Self::Webhook { name, .. }
            | Self::Matrix { name, .. }
            | Self::FreeMobile { name, .. }
            | Self::Email { name, .. }
            | Self::Ntfy { name, .. }
            | Self::Gotify { name, .. }
            | Self::Pushover { name, .. } => name,
        }
    }

    /// Whether the channel has everything it needs to send. One that lacks a
    /// required field (see [`Channel::empty_required_field`]) is disabled:
    /// left out of the dispatcher rather than failing at every send, while
    /// the others keep working.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.empty_required_field().is_none()
    }

    /// The first required field that is empty - typically a secret whose
    /// `${VAR}` is not set. The load warns about it by name and `hora doctor`
    /// lists the channel as disabled: it used to go away without a word, and
    /// the first sign was an outage nobody heard about.
    #[must_use]
    pub fn empty_required_field(&self) -> Option<&'static str> {
        self.required_fields()
            .into_iter()
            .find(|(_field, value)| value.is_empty())
            .map(|(field, _value)| field)
    }

    /// The fields the channel cannot send without, each with its value.
    fn required_fields(&self) -> Vec<(&'static str, &str)> {
        match self {
            Self::Telegram { token, chat_id, .. } => {
                vec![("token", token.as_ref()), ("chat_id", chat_id)]
            }
            Self::Discord { webhook_url, .. } | Self::Slack { webhook_url, .. } => {
                vec![("webhook_url", webhook_url.as_ref())]
            }
            Self::Webhook { url, .. } | Self::Ntfy { url, .. } => vec![("url", url.as_ref())],
            Self::Matrix {
                homeserver,
                token,
                room_id,
                ..
            } => vec![
                ("homeserver", homeserver),
                ("token", token.as_ref()),
                ("room_id", room_id),
            ],
            Self::FreeMobile { user, pass, .. } => vec![("user", user), ("pass", pass.as_ref())],
            Self::Email { host, from, to, .. } => vec![("host", host), ("from", from), ("to", to)],
            Self::Gotify { url, token, .. } => {
                vec![("url", url.as_ref()), ("token", token.as_ref())]
            }
            Self::Pushover { token, user, .. } => {
                vec![("token", token.as_ref()), ("user", user.as_ref())]
            }
        }
    }
}

fn default_smtp_port() -> u16 {
    587
}
