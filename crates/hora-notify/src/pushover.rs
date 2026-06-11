//! Pushover notifier: POSTs a message to the Pushover API.

use async_trait::async_trait;
use reqwest::Client;

use crate::util::{
    budget_burn_phrase, cert_expiry_phrase, domain_expiry_phrase, latency_suffix, send_retrying,
    topology_suffix,
};
use crate::{Event, Notifier};

const PUSHOVER_API: &str = "https://api.pushover.net/1/messages.json";

pub struct PushoverNotifier {
    client: Client,
    token: String,
    user: String,
}

impl PushoverNotifier {
    #[must_use]
    pub fn new(client: Client, token: String, user: String) -> Self {
        Self {
            client,
            token,
            user,
        }
    }

    fn message(event: Event<'_>) -> (String, i8) {
        match event {
            Event::Down {
                monitor,
                error,
                cause,
                impacted,
            } => {
                let suffix = topology_suffix(cause, impacted);
                let detail = error.map_or_else(String::new, |e| format!("\n{e}"));
                (format!("DOWN: {monitor}{detail}{suffix}"), 1)
            }
            Event::Degraded {
                monitor,
                latency_ms,
            } => (
                format!("DEGRADED: {monitor}{}", latency_suffix(latency_ms)),
                0,
            ),
            Event::Recovered { monitor } => (format!("RECOVERED: {monitor}"), -1),
            Event::CertExpiring { monitor, days_left } => (
                format!("CERT: {monitor} {}", cert_expiry_phrase(days_left)),
                0,
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
                0,
            ),
            Event::Digest { period, summary } => (format!("DIGEST ({period}):\n{summary}"), -1),
            Event::PeerLinkDegraded { peer, witness } => (
                format!("PEER: {peer} unreachable, but {witness} sees it up (partition)"),
                0,
            ),
            Event::CertChanged {
                monitor,
                old_fingerprint,
                new_fingerprint,
            } => (
                format!("CERT CHANGED: {monitor}\nold: {old_fingerprint}\nnew: {new_fingerprint}"),
                1,
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
                1,
            ),
        }
    }
}

#[async_trait]
impl Notifier for PushoverNotifier {
    fn name(&self) -> &'static str {
        "pushover"
    }

    async fn notify(&self, event: Event<'_>) -> anyhow::Result<()> {
        let (message, priority) = Self::message(event);
        let build = || {
            self.client.post(PUSHOVER_API).json(&serde_json::json!({
                "token": self.token,
                "user": self.user,
                "message": message,
                "title": "Hora Alert",
                "priority": priority,
            }))
        };
        // The user key is a quasi-secret too: it lets anyone message the user.
        send_retrying(
            build,
            "pushover",
            &[self.token.as_str(), self.user.as_str()],
        )
        .await
    }
}
