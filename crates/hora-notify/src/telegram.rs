//! Telegram Bot API notifier.

use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;

use crate::util::{
    budget_burn_phrase, cert_expiry_phrase, domain_expiry_phrase, escape, latency_suffix,
    post_json, topology_suffix, vantage_suffix,
};
use crate::{Event, Notifier};

/// Sends alerts to a Telegram chat via the Bot API.
pub struct TelegramNotifier {
    client: Client,
    token: String,
    chat_id: String,
}

impl TelegramNotifier {
    #[must_use]
    pub fn new(client: Client, token: String, chat_id: String) -> Self {
        Self {
            client,
            token,
            chat_id,
        }
    }

    fn render(event: Event<'_>) -> String {
        match event {
            Event::Down {
                monitor,
                error,
                cause,
                impacted,
                vantage,
            } => format!(
                "\u{1F534} <b>{}</b> is DOWN\n<code>{}</code>{}{}",
                escape(monitor),
                escape(error.unwrap_or("no response")),
                escape(&topology_suffix(cause, impacted)),
                escape(&vantage_suffix(vantage)),
            ),
            Event::Degraded {
                monitor,
                latency_ms,
            } => format!(
                "\u{1F7E0} <b>{}</b> is slow{}",
                escape(monitor),
                latency_suffix(latency_ms)
            ),
            Event::Recovered { monitor } => {
                format!("\u{1F7E2} <b>{}</b> recovered", escape(monitor))
            }
            Event::CertExpiring { monitor, days_left } => format!(
                "\u{1F510} <b>{}</b> TLS certificate {}",
                escape(monitor),
                cert_expiry_phrase(days_left)
            ),
            Event::DomainExpiring {
                monitor,
                domain,
                days_left,
            } => format!(
                "\u{1F310} <b>{}</b> {}",
                escape(monitor),
                escape(&domain_expiry_phrase(domain, days_left)),
            ),
            Event::Digest { period, summary } => format!(
                "\u{1F4CA} <b>Hora digest</b> ({})\n{}",
                escape(period),
                escape(summary)
            ),
            Event::PeerLinkDegraded { peer, witness } => format!(
                "\u{1F7E1} <b>{}</b> link degraded\nunreachable from here, but seen up by {}",
                escape(peer),
                escape(witness),
            ),
            Event::CertChanged {
                monitor,
                old_fingerprint,
                new_fingerprint,
            } => format!(
                "\u{26A0}\u{FE0F} <b>{}</b> TLS certificate changed unexpectedly\nold: <code>{}</code>\nnew: <code>{}</code>",
                escape(monitor),
                escape(old_fingerprint),
                escape(new_fingerprint),
            ),
            Event::BudgetBurn {
                monitor,
                burn_rate_x10,
                window,
                exhausted_in_secs,
            } => format!(
                "\u{1F525} <b>{}</b> {}",
                escape(monitor),
                budget_burn_phrase(burn_rate_x10, window, exhausted_in_secs),
            ),
        }
    }
}

#[derive(Serialize)]
struct SendMessage<'a> {
    chat_id: &'a str,
    text: &'a str,
    parse_mode: &'a str,
}

#[async_trait]
impl Notifier for TelegramNotifier {
    fn name(&self) -> &'static str {
        "telegram"
    }

    async fn notify(&self, event: Event<'_>) -> anyhow::Result<()> {
        let text = Self::render(event);
        // The URL embeds the bot token, so it is what we redact from errors.
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let body = SendMessage {
            chat_id: &self.chat_id,
            text: &text,
            parse_mode: "HTML",
        };
        post_json(
            &self.client,
            &url,
            &body,
            "telegram",
            &[self.token.as_str()],
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_each_event() {
        let down = TelegramNotifier::render(Event::Down {
            monitor: "API",
            error: Some("boom"),
            cause: None,
            impacted: &[],
            vantage: None,
        });
        assert!(down.contains("is DOWN") && down.contains("boom"));

        let symptom = TelegramNotifier::render(Event::Down {
            monitor: "API",
            error: Some("timeout"),
            cause: Some("DB"),
            impacted: &[],
            vantage: None,
        });
        assert!(symptom.contains("caused by DB"));

        let root = TelegramNotifier::render(Event::Down {
            monitor: "DB",
            error: Some("refused"),
            cause: None,
            impacted: &["API", "Web"],
            vantage: None,
        });
        assert!(root.contains("impacts 2") && root.contains("API"));

        let recovered = TelegramNotifier::render(Event::Recovered { monitor: "API" });
        assert!(recovered.contains("recovered"));

        let degraded = TelegramNotifier::render(Event::Degraded {
            monitor: "API",
            latency_ms: Some(1234),
        });
        assert!(degraded.contains("slow") && degraded.contains("1234ms"));

        let cert = TelegramNotifier::render(Event::CertExpiring {
            monitor: "API",
            days_left: 3,
        });
        assert!(cert.contains("expires in 3 days"));

        let partition = TelegramNotifier::render(Event::PeerLinkDegraded {
            peer: "Hora B",
            witness: "Hora C",
        });
        assert!(partition.contains("link degraded") && partition.contains("Hora C"));
    }
}
