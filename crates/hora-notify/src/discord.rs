//! Discord incoming-webhook notifier.

use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;

use crate::util::{cert_expiry_phrase, latency_suffix, post_json};
use crate::{Event, Notifier};

// Embed accent colours, matching the status badges: red / green / orange.
const COLOR_DOWN: u32 = 0x00E0_5D44;
const COLOR_UP: u32 = 0x0044_CC11;
const COLOR_CERT: u32 = 0x00FE_7D37;
const COLOR_DEGRADED: u32 = 0x00DF_B317;

/// Posts alerts to a Discord channel through an incoming webhook.
pub struct DiscordNotifier {
    client: Client,
    webhook_url: String,
}

impl DiscordNotifier {
    #[must_use]
    pub fn new(client: Client, webhook_url: String) -> Self {
        Self {
            client,
            webhook_url,
        }
    }

    fn embed(event: Event<'_>) -> Embed {
        match event {
            Event::Down { monitor, error } => Embed {
                // Fence the error so Discord shows it verbatim; strip backticks
                // that would otherwise break out of the code block.
                description: Some(format!(
                    "```{}```",
                    error.unwrap_or("no response").replace('`', "'")
                )),
                title: format!("\u{1F534} {monitor} is DOWN"),
                color: COLOR_DOWN,
            },
            Event::Degraded {
                monitor,
                latency_ms,
            } => Embed {
                title: format!("\u{1F7E0} {monitor} is slow{}", latency_suffix(latency_ms)),
                description: None,
                color: COLOR_DEGRADED,
            },
            Event::Recovered { monitor } => Embed {
                title: format!("\u{1F7E2} {monitor} recovered"),
                description: None,
                color: COLOR_UP,
            },
            Event::CertExpiring { monitor, days_left } => Embed {
                title: format!(
                    "\u{1F510} {monitor} TLS certificate {}",
                    cert_expiry_phrase(days_left)
                ),
                description: None,
                color: COLOR_CERT,
            },
            Event::PeerLinkDegraded { peer, witness } => Embed {
                title: format!("\u{1F7E1} {peer} link degraded"),
                description: Some(format!(
                    "Unreachable from here, but still seen up by {witness} (likely a network partition)."
                )),
                color: COLOR_DEGRADED,
            },
        }
    }
}

/// The rendered embed, before borrowing into the JSON payload.
struct Embed {
    title: String,
    description: Option<String>,
    color: u32,
}

#[derive(Serialize)]
struct Payload<'a> {
    embeds: [EmbedJson<'a>; 1],
}

#[derive(Serialize)]
struct EmbedJson<'a> {
    title: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    color: u32,
}

#[async_trait]
impl Notifier for DiscordNotifier {
    fn name(&self) -> &'static str {
        "discord"
    }

    async fn notify(&self, event: Event<'_>) {
        let embed = Self::embed(event);
        let payload = Payload {
            embeds: [EmbedJson {
                title: &embed.title,
                description: embed.description.as_deref(),
                color: embed.color,
            }],
        };
        post_json(
            &self.client,
            &self.webhook_url,
            &payload,
            "discord",
            &self.webhook_url,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_render_per_event() {
        let down = DiscordNotifier::embed(Event::Down {
            monitor: "API",
            error: Some("boom"),
        });
        assert!(down.title.contains("is DOWN"));
        assert!(down.description.expect("down has a body").contains("boom"));
        assert_eq!(down.color, COLOR_DOWN);

        let recovered = DiscordNotifier::embed(Event::Recovered { monitor: "API" });
        assert!(recovered.title.contains("recovered"));
        assert!(recovered.description.is_none());
        assert_eq!(recovered.color, COLOR_UP);

        let cert = DiscordNotifier::embed(Event::CertExpiring {
            monitor: "API",
            days_left: 3,
        });
        assert!(cert.title.contains("expires in 3 days"));
        assert_eq!(cert.color, COLOR_CERT);

        let degraded = DiscordNotifier::embed(Event::Degraded {
            monitor: "API",
            latency_ms: Some(1234),
        });
        assert!(degraded.title.contains("slow") && degraded.title.contains("1234ms"));
        assert_eq!(degraded.color, COLOR_DEGRADED);
    }

    #[test]
    fn down_body_neutralises_backticks() {
        let down = DiscordNotifier::embed(Event::Down {
            monitor: "API",
            error: Some("``` injection"),
        });
        let body = down.description.expect("down has a body");
        assert!(
            body.contains("''' injection"),
            "backticks not stripped: {body}"
        );
    }
}
