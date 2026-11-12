//! SMTP e-mail notifier (via `lettre`, rustls/aws-lc-rs).

use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message as Mail, Tokio1Executor};
use tracing::warn;

use crate::message::Message;
use crate::util::Unit;
use crate::{Event, Notifier};

/// Header lines over 998 characters break RFC 5322; lettre folds, but a
/// subject this long is unreadable anyway.
const SUBJECT_MAX: usize = 200;
/// No relay rejects this, and a digest that long is no longer read.
const BODY_MAX: usize = 60_000;

/// Sends alerts as plain-text e-mails through an SMTP relay.
pub struct EmailNotifier {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    to: Mailbox,
}

/// Everything needed to build an [`EmailNotifier`]; mirrors the config channel.
pub struct EmailConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from: String,
    pub to: String,
    pub implicit_tls: bool,
}

impl EmailNotifier {
    /// Build the transport and resolve the addresses.
    ///
    /// # Errors
    ///
    /// Returns an error if an address is malformed or the TLS relay cannot be
    /// constructed.
    pub fn new(config: EmailConfig) -> anyhow::Result<Self> {
        let from = config
            .from
            .parse::<Mailbox>()
            .with_context(|| format!("invalid `from` address {:?}", config.from))?;
        let to = config
            .to
            .parse::<Mailbox>()
            .with_context(|| format!("invalid `to` address {:?}", config.to))?;

        // STARTTLS (587) by default; implicit TLS (465) when asked.
        let builder = if config.implicit_tls {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host)
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host)
        }
        .context("building the SMTP relay")?
        .port(config.port)
        // Cap a stalled relay so it can't hang the dispatch (lettre's default is
        // generous); the notifier client elsewhere has its own backstop.
        .timeout(Some(Duration::from_secs(15)));

        let builder = if config.username.is_empty() {
            builder
        } else {
            builder.credentials(Credentials::new(config.username, config.password))
        };

        Ok(Self {
            transport: builder.build(),
            from,
            to,
        })
    }

    /// Subject `[DOWN] API is DOWN` (a pushed alert: `[ERROR] API: title`);
    /// body: the headline, a blank line, then the detail.
    fn render(event: Event<'_>) -> (String, String) {
        let message = Message::render(event).fit(SUBJECT_MAX, BODY_MAX, Unit::Chars);
        let headline = message.headline();
        let detail = message.plain_body();
        let body = if detail.is_empty() {
            headline
        } else {
            format!("{headline}\n\n{detail}")
        };
        (message.tagged_headline(), body)
    }
}

#[async_trait]
impl Notifier for EmailNotifier {
    fn name(&self) -> &'static str {
        "email"
    }

    async fn notify(&self, event: Event<'_>) -> anyhow::Result<()> {
        let (subject, body) = Self::render(event);
        let message = match Mail::builder()
            .from(self.from.clone())
            .to(self.to.clone())
            .subject(subject)
            .body(body)
        {
            Ok(message) => message,
            Err(err) => {
                warn!("{} message build failed: {err}", self.name());
                anyhow::bail!("message build failed: {err}");
            }
        };
        // The error carries the SMTP host/response, never the password.
        if let Err(err) = self.transport.send(message).await {
            warn!("{} send failed: {err}", self.name());
            anyhow::bail!("send failed: {err}");
        }
        Ok(())
    }
}

/// What this channel would send for `event`: the fuzz target's way in (see
/// [`crate::fuzz`], built with `--cfg fuzzing` only).
#[cfg(fuzzing)]
pub(crate) fn fuzzed(event: Event<'_>) -> crate::fuzz::Sent {
    let (subject, body) = EmailNotifier::render(event);
    // The message as the relay would get it: lettre encodes the subject and
    // the body, which is where a hostile one would hurt.
    let mailbox = || {
        lettre::Address::new("hora", "example.com")
            .ok()
            .map(|address| Mailbox::new(None, address))
    };
    let built = mailbox().zip(mailbox()).and_then(|(from, to)| {
        Mail::builder()
            .from(from)
            .to(to)
            .subject(subject.clone())
            .body(body)
            .ok()
    });
    crate::fuzz::Sent {
        channel: "email",
        title: Some(subject),
        body: built.map_or_else(String::new, |mail| {
            String::from_utf8_lossy(&mail.formatted()).into_owned()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_subject_and_body() {
        let (subject, body) = EmailNotifier::render(Event::down("API", Some("boom")));
        assert!(subject.contains("[DOWN]") && subject.contains("API"));
        assert!(body.contains("boom"));

        let (subject, _) = EmailNotifier::render(Event::CertExpiring {
            monitor: "API",
            secs_left: 3 * 86_400,
        });
        assert!(subject.contains("expires in 3 d"));

        let (subject, body) = EmailNotifier::render(Event::Alert {
            monitor: "API",
            severity: crate::AlertSeverity::Error,
            title: "boom",
            message: "stack trace",
        });
        assert_eq!(subject, "[ERROR] API: boom");
        assert_eq!(body, "[ERROR] API: boom\n\nstack trace");
    }

    #[test]
    fn rejects_bad_address() {
        let result = EmailNotifier::new(EmailConfig {
            host: "smtp.example.com".to_owned(),
            port: 587,
            username: String::new(),
            password: String::new(),
            from: "not an address".to_owned(),
            to: "ops@example.com".to_owned(),
            implicit_tls: false,
        });
        assert!(result.is_err());
    }
}
