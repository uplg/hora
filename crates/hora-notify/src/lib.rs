//! Notification abstraction.
//!
//! Alerting code emits an [`Event`]; each configured [`Notifier`] decides how
//! to deliver it. [`Dispatcher`] holds the channels under their routing names
//! and fans an event out to the matching ones. Telegram, Discord, Slack, Matrix,
//! a generic JSON webhook, SMTP e-mail, Free Mobile SMS, ntfy, Gotify and
//! Pushover are the built-in backends; adding another means implementing the
//! trait and registering it.

pub mod discord;
pub mod email;
pub mod freemobile;
pub mod gotify;
pub mod matrix;
pub mod ntfy;
pub mod pushover;
pub mod slack;
pub mod telegram;
mod util;
pub mod webhook;

use async_trait::async_trait;
use futures_util::future::join_all;

pub use discord::DiscordNotifier;
pub use email::{EmailConfig, EmailNotifier};
pub use freemobile::FreeMobileNotifier;
pub use gotify::GotifyNotifier;
pub use matrix::MatrixNotifier;
pub use ntfy::NtfyNotifier;
pub use pushover::PushoverNotifier;
pub use slack::SlackNotifier;
pub use telegram::TelegramNotifier;
pub use webhook::WebhookNotifier;

/// An alertable event. Borrows its data so emitting one is allocation-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event<'a> {
    /// A monitor is confirmed down.
    Down {
        monitor: &'a str,
        error: Option<&'a str>,
        /// The upstream monitor causing this failure (topology annotation).
        cause: Option<&'a str>,
        /// Downstream monitors impacted by this root-cause failure.
        impacted: &'a [&'a str],
    },
    /// A monitor is up but degraded: slower than its `degraded_over_ms` budget.
    Degraded {
        monitor: &'a str,
        latency_ms: Option<i64>,
    },
    /// A previously-down (or degraded) monitor is fully healthy again.
    Recovered { monitor: &'a str },
    /// A monitor's TLS certificate is within the warning window (or expired).
    CertExpiring { monitor: &'a str, days_left: i64 },
    /// A peer is unreachable from here, but a third-party witness still sees it
    /// up: likely a network partition on the local-to-peer link, not a peer
    /// outage. Lower severity than [`Event::Down`].
    PeerLinkDegraded { peer: &'a str, witness: &'a str },
    /// A monitor's TLS certificate has changed unexpectedly (different public key
    /// fingerprint). This may indicate a MITM attack or an unexpected renewal.
    CertChanged {
        monitor: &'a str,
        old_fingerprint: &'a str,
        new_fingerprint: &'a str,
    },
    /// A monitor is burning its availability error budget abnormally fast
    /// (Google-SRE burn-rate alerting). Fires while the monitor may still be
    /// "up" between blips - which is exactly the point.
    BudgetBurn {
        monitor: &'a str,
        /// Burn rate in tenths of the sustainable rate (144 = 14.4x).
        burn_rate_x10: i64,
        /// The lookback that triggered: `"1h"` (fast burn) or `"6h"` (slow).
        window: &'a str,
        /// Estimated seconds until the budget is fully spent at this rate.
        exhausted_in_secs: Option<i64>,
    },
}

/// A delivery channel for [`Event`]s.
#[async_trait]
pub trait Notifier: Send + Sync {
    /// Channel name, used in logs.
    fn name(&self) -> &'static str;

    /// Deliver one event. Implementations must not panic; log and move on.
    async fn notify(&self, event: Event<'_>);
}

/// A registered channel: its routing name plus the delivery backend.
struct Channel {
    name: String,
    notifier: Box<dyn Notifier>,
}

/// Holds the configured channels and fans events out to the matching ones.
#[derive(Default)]
pub struct Dispatcher {
    channels: Vec<Channel>,
}

impl Dispatcher {
    #[must_use]
    pub fn new(channels: Vec<(String, Box<dyn Notifier>)>) -> Self {
        let channels = channels
            .into_iter()
            .map(|(name, notifier)| Channel { name, notifier })
            .collect();
        Self { channels }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.channels.len()
    }

    /// Deliver `event` to the matching channels concurrently: all of them when
    /// `only` is `None`, otherwise just those whose name appears in the list. A
    /// slow channel never holds up the others (or the monitor loop behind them).
    pub async fn dispatch(&self, event: Event<'_>, only: Option<&[String]>) {
        let deliveries = self
            .channels
            .iter()
            .filter(|channel| {
                only.is_none_or(|names| names.iter().any(|name| name == &channel.name))
            })
            .map(|channel| channel.notifier.notify(event));
        join_all(deliveries).await;
    }
}
