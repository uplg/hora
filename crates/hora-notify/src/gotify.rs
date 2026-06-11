//! Gotify notifier: POSTs a message to a Gotify server.

use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;

use crate::util::{
    budget_burn_phrase, cert_expiry_phrase, domain_expiry_phrase, latency_suffix, send_retrying,
    topology_suffix,
};
use crate::{Event, Notifier};

pub struct GotifyNotifier {
    client: Client,
    url: String,
    token: String,
}

impl GotifyNotifier {
    #[must_use]
    pub fn new(client: Client, url: String, token: String) -> Self {
        Self { client, url, token }
    }

    fn payload(event: Event<'_>) -> Payload {
        let (message, priority) = match event {
            Event::Down {
                monitor,
                error,
                cause,
                impacted,
            } => {
                let suffix = topology_suffix(cause, impacted);
                let detail = error.map_or_else(String::new, |e| format!("\n{e}"));
                (format!("DOWN: {monitor}{detail}{suffix}"), 8)
            }
            Event::Degraded {
                monitor,
                latency_ms,
            } => (
                format!("DEGRADED: {monitor}{}", latency_suffix(latency_ms)),
                5,
            ),
            Event::Recovered { monitor } => (format!("RECOVERED: {monitor}"), 2),
            Event::CertExpiring { monitor, days_left } => (
                format!("CERT: {monitor} {}", cert_expiry_phrase(days_left)),
                5,
            ),
            Event::DomainExpiring {
                monitor,
                domain,
                days_left,
            } => (
                format!(
                    "DOMAIN: {monitor} {}",
                    domain_expiry_phrase(domain, days_left)
                ),
                5,
            ),
            Event::Digest { period, summary } => (format!("DIGEST ({period}):\n{summary}"), 2),
            Event::PeerLinkDegraded { peer, witness } => (
                format!("PEER: {peer} unreachable, but {witness} sees it up (partition)"),
                5,
            ),
            Event::CertChanged {
                monitor,
                old_fingerprint,
                new_fingerprint,
            } => (
                format!("CERT CHANGED: {monitor}\nold: {old_fingerprint}\nnew: {new_fingerprint}"),
                8,
            ),
            Event::BudgetBurn {
                monitor,
                burn_rate_x10,
                window,
                exhausted_in_secs,
            } => (
                format!(
                    "BUDGET: {monitor} {}",
                    budget_burn_phrase(burn_rate_x10, window, exhausted_in_secs)
                ),
                8,
            ),
        };
        Payload {
            title: "Hora Alert".to_owned(),
            message,
            priority,
        }
    }
}

#[derive(Serialize)]
struct Payload {
    title: String,
    message: String,
    priority: u8,
}

#[async_trait]
impl Notifier for GotifyNotifier {
    fn name(&self) -> &'static str {
        "gotify"
    }

    async fn notify(&self, event: Event<'_>) -> anyhow::Result<()> {
        let payload = Self::payload(event);
        // The token travels as a header, not in the URL: query strings end up
        // in proxy and server access logs.
        let url = format!("{}/message", self.url.trim_end_matches('/'));
        let build = || {
            self.client
                .post(&url)
                .header("X-Gotify-Key", &self.token)
                .json(&payload)
        };
        send_retrying(build, "gotify", &[self.token.as_str()]).await
    }
}
