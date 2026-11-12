//! Discord incoming-webhook notifier.

use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;

use crate::message::{Kind, Markup, Message};
use crate::util::{Unit, post_json};
use crate::{AlertSeverity, Event, Notifier};

// Embed accent colours, matching the status badges: red / green / orange.
const COLOR_DOWN: u32 = 0x00E0_5D44;
const COLOR_UP: u32 = 0x0044_CC11;
const COLOR_CERT: u32 = 0x00FE_7D37;
const COLOR_DEGRADED: u32 = 0x00DF_B317;
const COLOR_INFO: u32 = 0x0035_82F6;

/// Discord markdown for the embed. Prose is escaped (see [`escape_markdown`]);
/// a backtick inside code would close the fence early.
const MARKDOWN: Markup = Markup {
    text: escape_markdown,
    code: neutralise_backticks,
    bold: ("", ""),
    block: ("```", "```"),
    inline: ("`", "`"),
};

fn neutralise_backticks(code: &str) -> String {
    code.replace('`', "'")
}

/// Escape what Discord would read as markdown in prose that is not ours: a
/// monitor called `my_db_primary` came out with `db` in italics, and the text
/// of a pushed alert could plant a masked link (`[status](https://evil)`)
/// under Hora's name. A backslash makes the next punctuation literal.
///
/// - what opens emphasis, strikethrough, code and spoilers, and the escape
///   itself (asterisk, underscore, tilde, backtick, pipe, backslash), and
///   `<` (mentions, timestamps, `<url>`), everywhere;
/// - `#` (heading, `-#` subtext) and `>` (quote) where they would act, at the
///   start of a line;
/// - the `(` right after a `]`, which is what makes a masked link. Brackets
///   and parentheses on their own stay as typed (`[ERROR] API`, `(build 12)`).
///
/// A word that is a bare URL is left alone: Discord links it as it reads, and
/// a backslash inside would break the link (`https://host/a_b`).
fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    for line in text.split_inclusive('\n') {
        let mut at_start = true;
        for word in line.split_inclusive(char::is_whitespace) {
            if word.starts_with("https://") || word.starts_with("http://") {
                out.push_str(word);
                at_start = false;
                continue;
            }
            let mut previous = None;
            for c in word.chars() {
                let escape = match c {
                    '\\' | '*' | '_' | '~' | '`' | '|' | '<' => true,
                    '#' | '>' | '-' => at_start && (c != '-' || word.starts_with("-#")),
                    '(' => previous == Some(']'),
                    _ => false,
                };
                if escape {
                    out.push('\\');
                }
                out.push(c);
                at_start &= c.is_whitespace();
                previous = Some(c);
            }
        }
    }
    out
}

/// Embed limits: title 256 characters, description 4096.
const TITLE_LIMIT: usize = 256;
const DESCRIPTION_LIMIT: usize = 4096;
/// What the raw text is cut to. The margins cover the icon in the title and
/// the fences in the description.
const TITLE_MAX: usize = 250;
const DESCRIPTION_MAX: usize = 4000;

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
        let message = Message::render(event);
        let render = |title_max, description_max| {
            let message = message.clone().fit(title_max, description_max, Unit::Chars);
            // No bold in a title (it is one already), but emphasis and
            // escapes are read there too.
            (
                message.headline_with(&MARKDOWN),
                message.body_with(&MARKDOWN),
            )
        };
        let (mut title, mut description) = render(TITLE_MAX, DESCRIPTION_MAX);
        // The text is cut before it is escaped (a cut after could split an
        // escape), and escaping grows it, at worst twofold: a text made of
        // underscores is cut again at half the room, which always fits. Over
        // the limit Discord answers 400 and the alert is lost.
        if title.chars().count() > TITLE_LIMIT || description.chars().count() > DESCRIPTION_LIMIT {
            (title, description) = render(TITLE_MAX / 2, DESCRIPTION_MAX / 2);
        }
        Embed {
            title,
            description: (!description.is_empty()).then_some(description),
            color: color(message.kind),
        }
    }
}

fn color(kind: Kind) -> u32 {
    match kind {
        Kind::Down | Kind::Alert(AlertSeverity::Error | AlertSeverity::Critical) => COLOR_DOWN,
        Kind::Recovered | Kind::Digest => COLOR_UP,
        Kind::CertExpiring
        | Kind::DomainExpiring
        | Kind::Release
        | Kind::CertChanged
        | Kind::CertUnreadable => COLOR_CERT,
        Kind::Degraded
        | Kind::PeerLinkDegraded
        | Kind::BudgetBurn
        | Kind::Alert(AlertSeverity::Warning) => COLOR_DEGRADED,
        Kind::Alert(AlertSeverity::Info) => COLOR_INFO,
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

    async fn notify(&self, event: Event<'_>) -> anyhow::Result<()> {
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
            self.name(),
            &[self.webhook_url.as_str()],
        )
        .await
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
            cause: Some("DB"),
            impacted: &[],
            vantage: None,
            event: None,
            local_only: false,
        });
        assert_eq!(down.title, "\u{1F534} API is DOWN");
        assert_eq!(
            down.description.as_deref(),
            Some("```boom```\ncaused by DB")
        );
        assert_eq!(down.color, COLOR_DOWN);

        let recovered = DiscordNotifier::embed(Event::Recovered {
            monitor: "API",
            local_only: false,
        });
        assert!(recovered.title.contains("recovered"));
        assert!(recovered.description.is_none());
        assert_eq!(recovered.color, COLOR_UP);

        let cert = DiscordNotifier::embed(Event::CertExpiring {
            monitor: "API",
            secs_left: 3 * 86_400,
        });
        assert!(cert.title.contains("expires in 3 d"));
        assert_eq!(cert.color, COLOR_CERT);

        let degraded = DiscordNotifier::embed(Event::Degraded {
            monitor: "API",
            latency_ms: Some(1234),
            detail: None,
        });
        assert!(degraded.title.contains("slow") && degraded.title.contains("1234 ms"));
        assert_eq!(degraded.color, COLOR_DEGRADED);

        let alert = DiscordNotifier::embed(Event::Alert {
            monitor: "API",
            severity: AlertSeverity::Info,
            title: "deploy",
            message: "v2",
        });
        assert_eq!(alert.title, "\u{1F514} [INFO] API: deploy");
        assert_eq!(alert.description.as_deref(), Some("v2"));
        assert_eq!(alert.color, COLOR_INFO);
    }

    #[test]
    fn down_body_neutralises_backticks() {
        let down = DiscordNotifier::embed(Event::down("API", Some("``` injection")));
        let body = down.description.expect("down has a body");
        assert!(
            body.contains("''' injection"),
            "backticks not stripped: {body}"
        );
    }

    #[test]
    fn prose_is_escaped_not_rendered_as_markdown() {
        // A snake_case name must not come out in italics, in the title or in
        // the topology line of another monitor's alert.
        let down = DiscordNotifier::embed(Event::Down {
            monitor: "my_db_primary",
            error: Some("connection_refused *now*"),
            cause: Some("core_switch"),
            impacted: &[],
            vantage: None,
            event: None,
            local_only: false,
        });
        assert_eq!(down.title, "\u{1F534} my\\_db\\_primary is DOWN");
        let body = down.description.expect("down has a body");
        // Code is shown as is: nothing renders inside a fence.
        assert!(body.contains("```connection_refused *now*```"), "{body}");
        assert!(body.contains("caused by core\\_switch"), "{body}");

        // A producer's text cannot plant a masked link, a mention or a heading.
        let alert = DiscordNotifier::embed(Event::Alert {
            monitor: "API",
            severity: AlertSeverity::Error,
            title: "see [the status page](https://evil.example)",
            message: "# URGENT\n> **pay** <@&123> ||now||\n-# small",
        });
        assert_eq!(
            alert.title,
            "\u{1F514} [ERROR] API: see [the status page]\\(https://evil.example)"
        );
        assert_eq!(
            alert.description.as_deref(),
            Some("\\# URGENT\n\\> \\*\\*pay\\*\\* \\<@&123> \\|\\|now\\|\\|\n\\-# small")
        );
    }

    #[test]
    fn plain_prose_and_bare_urls_are_left_alone() {
        assert_eq!(
            escape_markdown("v1.9.2 is out (running v1.9.1) - 3 > 2 #1"),
            "v1.9.2 is out (running v1.9.1) - 3 > 2 #1"
        );
        // A backslash inside a bare URL would break the link.
        assert_eq!(
            escape_markdown("notes: https://example.com/my_repo/a_b and my_repo"),
            "notes: https://example.com/my_repo/a_b and my\\_repo"
        );
        assert_eq!(escape_markdown("a\\_b"), "a\\\\\\_b");
    }

    /// Escaping doubles a text made of markdown characters: it must still fit.
    #[test]
    fn escaped_alerts_fit_the_embed_limits() {
        let name = "_".repeat(300);
        let summary = "*".repeat(10_000);
        let alert = DiscordNotifier::embed(Event::Alert {
            monitor: &name,
            severity: AlertSeverity::Critical,
            title: &name,
            message: &summary,
        });
        assert!(alert.title.chars().count() <= 256, "{}", alert.title.len());
        let description = alert.description.expect("has a body");
        assert!(description.chars().count() <= 4096);
        assert!(description.starts_with("\\*\\*"));
    }

    #[test]
    fn long_alerts_fit_the_embed_limits() {
        let name = "n".repeat(100);
        let title = "t".repeat(200);
        let summary = "s".repeat(10_000);
        let alert = DiscordNotifier::embed(Event::Alert {
            monitor: &name,
            severity: AlertSeverity::Critical,
            title: &title,
            message: &summary,
        });
        assert!(alert.title.chars().count() <= 256);
        let description = alert.description.expect("has a body");
        assert!(description.chars().count() <= 4096);

        let digest = DiscordNotifier::embed(Event::Digest {
            period: "week",
            summary: &summary,
        });
        assert!(digest.description.expect("has a body").chars().count() <= 4096);
    }
}
