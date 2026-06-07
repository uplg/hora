//! Notification abstraction.
//!
//! Alerting code emits an [`Event`]; each configured [`Notifier`] decides how
//! to deliver it. [`Dispatcher`] fans an event out to every notifier. Telegram
//! is the only built-in channel today; adding another means implementing the
//! trait and registering it.

pub mod telegram;

use async_trait::async_trait;

pub use telegram::TelegramNotifier;

/// An alertable event. Borrows its data so emitting one is allocation-free.
#[derive(Debug, Clone, Copy)]
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

/// Fans an event out to every registered notifier, in order.
#[derive(Default)]
pub struct Dispatcher {
    notifiers: Vec<Box<dyn Notifier>>,
}

impl Dispatcher {
    #[must_use]
    pub fn new(notifiers: Vec<Box<dyn Notifier>>) -> Self {
        Self { notifiers }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.notifiers.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.notifiers.len()
    }

    /// Deliver `event` to every notifier.
    pub async fn dispatch(&self, event: Event<'_>) {
        for notifier in &self.notifiers {
            notifier.notify(event).await;
        }
    }
}
