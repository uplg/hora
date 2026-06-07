//! Notification abstraction.
//!
//! Alerting code emits an [`Event`]; each configured [`Notifier`] decides how
//! to deliver it. [`Dispatcher`] holds the channels under their routing names
//! and fans an event out to the matching ones. Telegram, Discord, Slack, a
//! generic JSON webhook and SMTP e-mail are the built-in backends; adding
//! another means implementing the trait and registering it.

pub mod discord;
pub mod email;
pub mod slack;
pub mod telegram;
mod util;
pub mod webhook;

use async_trait::async_trait;
use futures_util::future::join_all;

pub use discord::DiscordNotifier;
pub use email::{EmailConfig, EmailNotifier};
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
    },
    /// A previously-down monitor recovered.
    Recovered { monitor: &'a str },
    /// A monitor's TLS certificate is within the warning window (or expired).
    CertExpiring { monitor: &'a str, days_left: i64 },
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
