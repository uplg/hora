//! ntfy notifier: POSTs a plain-text message to an ntfy topic.

use async_trait::async_trait;
use reqwest::Client;

use crate::util::{
    budget_burn_phrase, cert_expiry_phrase, latency_suffix, send_retrying, topology_suffix,
};
use crate::{Event, Notifier};

pub struct NtfyNotifier {
    client: Client,
    url: String,
    token: Option<String>,
}

impl NtfyNotifier {
    #[must_use]
    pub fn new(client: Client, url: String, token: Option<String>) -> Self {
        Self { client, url, token }
    }

    fn message(event: Event<'_>) -> (String, &'static str, u8) {
        match event {
            Event::Down {
                monitor,
                error,
                cause,
                impacted,
            } => {
                let suffix = topology_suffix(cause, impacted);
                let detail = error.map_or_else(String::new, |e| format!("\n{e}"));
                (
                    format!("DOWN: {monitor}{detail}{suffix}"),
                    "rotating_light",
                    4,
                )
            }
            Event::Degraded {
                monitor,
                latency_ms,
            } => (
                format!("DEGRADED: {monitor}{}", latency_suffix(latency_ms)),
                "warning",
                3,
            ),
            Event::Recovered { monitor } => {
                (format!("RECOVERED: {monitor}"), "white_check_mark", 2)
            }
            Event::CertExpiring { monitor, days_left } => (
                format!("CERT: {monitor} {}", cert_expiry_phrase(days_left)),
                "lock",
                3,
            ),
            Event::PeerLinkDegraded { peer, witness } => (
                format!("PEER: {peer} unreachable, but {witness} sees it up (partition)"),
                "warning",
                3,
            ),
            Event::CertChanged {
                monitor,
                old_fingerprint,
                new_fingerprint,
            } => (
                format!("CERT CHANGED: {monitor}\nold: {old_fingerprint}\nnew: {new_fingerprint}"),
                "lock",
                4,
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
                "fire",
                4,
            ),
        }
    }
}

#[async_trait]
impl Notifier for NtfyNotifier {
    fn name(&self) -> &'static str {
        "ntfy"
    }

    async fn notify(&self, event: Event<'_>) {
        let (message, tags, priority) = Self::message(event);
        let build = || {
            let mut req = self.client.post(&self.url).body(message.clone());
            req = req.header("Title", "Hora Alert");
            req = req.header("Tags", tags);
            req = req.header("Priority", priority.to_string());
            if let Some(token) = &self.token {
                req = req.bearer_auth(token);
            }
            req
        };
        send_retrying(build, "ntfy", &self.url).await;
    }
}
