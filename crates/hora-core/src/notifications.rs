//! Build the active notification channels from configuration.
//!
//! The set of channels is rebuilt on every config reload and swapped in
//! atomically, so changing credentials or adding a channel takes effect live,
//! without a restart.

use std::sync::Arc;

use arc_swap::ArcSwap;
use hora_notify::{Dispatcher, Notifier, TelegramNotifier};
use reqwest::Client;

use crate::config::Config;

/// A hot-swappable set of notification channels shared across tasks.
pub type Notifiers = Arc<ArcSwap<Dispatcher>>;

/// Build the dispatcher for the current configuration.
#[must_use]
pub fn build(config: &Config, client: &Client) -> Dispatcher {
    let mut notifiers: Vec<Box<dyn Notifier>> = Vec::new();

    let telegram = &config.telegram;
    if !telegram.token.is_empty() && !telegram.chat_id.is_empty() {
        notifiers.push(Box::new(TelegramNotifier::new(
            client.clone(),
            telegram.token.clone(),
            telegram.chat_id.clone(),
        )));
    }

    Dispatcher::new(notifiers)
}

/// Build the shared, hot-swappable notifier handle from the initial config.
#[must_use]
pub fn shared(config: &Config, client: &Client) -> Notifiers {
    Arc::new(ArcSwap::from_pointee(build(config, client)))
}
