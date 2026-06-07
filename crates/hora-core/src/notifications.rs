//! Build the active notification channels from configuration.
//!
//! The set of channels is rebuilt on every config reload and swapped in
//! atomically, so changing credentials or adding a channel takes effect live,
//! without a restart.

use std::sync::Arc;

use arc_swap::ArcSwap;
use hora_notify::{
    DiscordNotifier, Dispatcher, EmailConfig, EmailNotifier, Notifier, SlackNotifier,
    TelegramNotifier, WebhookNotifier,
};
use reqwest::Client;

use crate::config::{Channel, Config};

/// A hot-swappable set of notification channels shared across tasks.
pub type Notifiers = Arc<ArcSwap<Dispatcher>>;

/// Build the dispatcher for the current configuration. Channels whose secret is
/// empty (e.g. an unset `${VAR}`) are skipped rather than failing at send time.
#[must_use]
pub fn build(config: &Config, client: &Client) -> Dispatcher {
    let channels = config
        .channels
        .iter()
        .filter(|channel| channel.is_configured())
        .filter_map(|channel| {
            let notifier: Box<dyn Notifier> = match channel {
                Channel::Telegram { token, chat_id, .. } => Box::new(TelegramNotifier::new(
                    client.clone(),
                    token.as_ref().to_owned(),
                    chat_id.clone(),
                )),
                Channel::Discord { webhook_url, .. } => Box::new(DiscordNotifier::new(
                    client.clone(),
                    webhook_url.as_ref().to_owned(),
                )),
                Channel::Slack { webhook_url, .. } => Box::new(SlackNotifier::new(
                    client.clone(),
                    webhook_url.as_ref().to_owned(),
                )),
                Channel::Webhook { url, .. } => Box::new(WebhookNotifier::new(
                    client.clone(),
                    url.as_ref().to_owned(),
                )),
                Channel::Email {
                    host,
                    port,
                    username,
                    password,
                    from,
                    to,
                    implicit_tls,
                    ..
                } => match EmailNotifier::new(EmailConfig {
                    host: host.clone(),
                    port: *port,
                    username: username.clone(),
                    password: password.as_ref().to_owned(),
                    from: from.clone(),
                    to: to.clone(),
                    implicit_tls: *implicit_tls,
                }) {
                    Ok(notifier) => Box::new(notifier),
                    Err(err) => {
                        // A misconfigured relay disables just this channel.
                        tracing::warn!("channel {}: {err:#}", channel.name());
                        return None;
                    }
                },
            };
            Some((channel.name().to_owned(), notifier))
        })
        .collect();

    Dispatcher::new(channels)
}

/// Build the shared, hot-swappable notifier handle from the initial config.
#[must_use]
pub fn shared(config: &Config, client: &Client) -> Notifiers {
    Arc::new(ArcSwap::from_pointee(build(config, client)))
}
