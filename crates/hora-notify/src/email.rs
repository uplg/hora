//! SMTP e-mail notifier (via `lettre`, rustls/aws-lc-rs).

use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use tracing::warn;

use crate::{Event, Notifier};

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

    fn render(event: Event<'_>) -> (String, String) {
        match event {
            Event::Down {
                monitor,
                error,
                cause,
                impacted,
            } => {
                let suffix = crate::util::topology_suffix(cause, impacted);
                (
                    format!("[DOWN] {monitor}"),
                    format!(
                        "{monitor} is DOWN\n\n{}{suffix}",
                        error.unwrap_or("no response")
                    ),
                )
            }
            Event::Degraded {
                monitor,
                latency_ms,
            } => (
                format!("[SLOW] {monitor}"),
                format!(
                    "{monitor} is up but responding slowly{}.",
                    crate::util::latency_suffix(latency_ms)
                ),
            ),
            Event::Recovered { monitor } => (
                format!("[OK] {monitor} recovered"),
                format!("{monitor} recovered."),
            ),
            Event::CertExpiring { monitor, days_left } => {
                let when = crate::util::cert_expiry_phrase(days_left);
                (
                    format!("[TLS] {monitor} certificate {when}"),
                    format!("The TLS certificate for {monitor} {when}."),
                )
            }
            Event::PeerLinkDegraded { peer, witness } => (
                format!("[LINK] {peer} link degraded"),
                format!(
                    "{peer} is unreachable from here, but still seen up by {witness} \
                     (likely a network partition rather than an outage)."
                ),
            ),
            Event::CertChanged {
                monitor,
                old_fingerprint,
                new_fingerprint,
            } => (
                format!("[TLS] {monitor} certificate changed unexpectedly"),
                format!(
                    "The TLS certificate for {monitor} has changed unexpectedly.\n\
                     Old fingerprint: {old_fingerprint}\n\
                     New fingerprint: {new_fingerprint}\n\n\
                     This may indicate a MITM attack or an unexpected certificate renewal."
                ),
            ),
            Event::BudgetBurn {
                monitor,
                burn_rate_x10,
                window,
                exhausted_in_secs,
            } => (
                format!("[BUDGET] {monitor} error budget burn"),
                format!(
                    "{monitor} is {}.",
                    crate::util::budget_burn_phrase(burn_rate_x10, window, exhausted_in_secs)
                ),
            ),
        }
    }
}

#[async_trait]
impl Notifier for EmailNotifier {
    fn name(&self) -> &'static str {
        "email"
    }

    async fn notify(&self, event: Event<'_>) {
        let (subject, body) = Self::render(event);
        let message = match Message::builder()
            .from(self.from.clone())
            .to(self.to.clone())
            .subject(subject)
            .body(body)
        {
            Ok(message) => message,
            Err(err) => {
                warn!("email message build failed: {err}");
                return;
            }
        };
        // The error carries the SMTP host/response, never the password.
        if let Err(err) = self.transport.send(message).await {
            warn!("email send failed: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_subject_and_body() {
        let (subject, body) = EmailNotifier::render(Event::Down {
            monitor: "API",
            error: Some("boom"),
            cause: None,
            impacted: &[],
        });
        assert!(subject.contains("[DOWN]") && subject.contains("API"));
        assert!(body.contains("boom"));

        let (subject, _) = EmailNotifier::render(Event::CertExpiring {
            monitor: "API",
            days_left: 3,
        });
        assert!(subject.contains("expires in 3 days"));
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
