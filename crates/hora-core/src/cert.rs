//! TLS certificate expiry monitoring and certificate pinning.
//!
//! A periodic task opens a TLS connection to each HTTPS monitor, reads the leaf
//! certificate's `notAfter`, stores it, and emits a [`hora_notify::Event`] once
//! expiry is within the configured window. Verification is intentionally
//! skipped: we only read the validity dates, independent of chain trust.
//!
//! Additionally, if a monitor has a `cert_pin` configured, the SHA-256
//! fingerprint of the leaf public key is compared against it. A fingerprint
//! that matches neither the pin nor the last seen value triggers a
//! [`hora_notify::Event::CertChanged`] alert - once per new fingerprint, since
//! the observed value is then remembered.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use hora_notify::Event;
use sqlx::SqlitePool;
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::aws_lc_rs::default_provider;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, SignatureScheme,
};
use tracing::{error, info, warn};
use x509_parser::prelude::FromDer;

use crate::SECONDS_PER_DAY;
use crate::config::Config;
use crate::db;
use crate::notifications::Notifiers;

const CHECK_INTERVAL: Duration = Duration::from_hours(12);

/// RDAP lookups happen at most this often per monitor (just under a day, so
/// the 12h ticks land on a daily cadence): registries rate-limit, the answer
/// moves yearly, and the gate is the stored `checked_at`, so a restart never
/// re-queries early.
const DOMAIN_CHECK_SECS: i64 = 20 * 3600;

/// A verifier that accepts any certificate: we want to read the dates, not
/// establish trust.
#[derive(Debug)]
struct ExtractOnly;

impl ServerCertVerifier for ExtractOnly {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        // Delegate to the provider so new schemes (e.g. post-quantum) are picked
        // up automatically instead of being hardcoded.
        default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn client_config() -> anyhow::Result<Arc<ClientConfig>> {
    let config = ClientConfig::builder_with_provider(Arc::new(default_provider()))
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(ExtractOnly))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Connect, handshake, and return the leaf certificate's `notAfter` (unix secs)
/// and the SHA-256 fingerprint of the leaf public key.
async fn fetch(
    config: &Arc<ClientConfig>,
    host: &str,
    port: u16,
    timeout: Duration,
) -> anyhow::Result<(i64, String)> {
    let tcp = tokio::time::timeout(timeout, TcpStream::connect((host, port)))
        .await
        .map_err(|_elapsed| anyhow::anyhow!("tcp connect timed out"))??;

    let connector = TlsConnector::from(Arc::clone(config));
    let server_name = ServerName::try_from(host.to_owned())?;
    let stream = tokio::time::timeout(timeout, connector.connect(server_name, tcp))
        .await
        .map_err(|_elapsed| anyhow::anyhow!("tls handshake timed out"))??;

    let (_io, connection) = stream.get_ref();
    let leaf = connection
        .peer_certificates()
        .and_then(<[CertificateDer<'_>]>::first)
        .ok_or_else(|| anyhow::anyhow!("server presented no certificate"))?;

    let (_rest, parsed) = x509_parser::certificate::X509Certificate::from_der(leaf.as_ref())
        .map_err(|err| anyhow::anyhow!("failed to parse certificate: {err}"))?;

    let not_after = parsed.validity().not_after.timestamp();
    let fingerprint = sha256_hex(parsed.public_key().raw);
    Ok((not_after, fingerprint))
}

/// A one-shot read of a TLS endpoint's leaf certificate, for `hora probe`.
#[derive(Debug, Clone)]
pub struct CertInfo {
    /// Leaf `notAfter`, unix seconds.
    pub not_after: i64,
    /// Whole days from now until `not_after` (negative once expired).
    pub days_left: i64,
    /// SHA-256 fingerprint of the leaf public key - the value `cert_pin` checks.
    pub fingerprint: String,
}

/// Connect to an `https://…` URL, read its leaf certificate and return its
/// expiry and public-key fingerprint. Like the watcher, trust is intentionally
/// not verified: this reads the dates, not the chain. Reuses the watcher's
/// exact handshake path.
///
/// # Errors
///
/// Returns an error if no host/port can be derived from `target`, or the
/// TCP/TLS handshake fails within `timeout`.
pub async fn inspect(target: &str, timeout: Duration) -> anyhow::Result<CertInfo> {
    let (host, port) = host_port(target).ok_or_else(|| {
        anyhow::anyhow!("cannot determine host:port for a cert check from {target:?}")
    })?;
    let tls = client_config()?;
    let (not_after, fingerprint) = fetch(&tls, &host, port, timeout).await?;
    let now = chrono::Utc::now().timestamp();
    Ok(CertInfo {
        not_after,
        days_left: (not_after - now) / SECONDS_PER_DAY,
        fingerprint,
    })
}

/// Compute the SHA-256 hex digest of a byte slice. Byte-by-byte formatting:
/// digest 0.11 dropped the `LowerHex` impl on the output array.
fn sha256_hex(data: &[u8]) -> String {
    use std::fmt::Write as _;

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Extract `(host, port)` from a monitor target URL (port defaults to 443).
fn host_port(target: &str) -> Option<(String, u16)> {
    let url = reqwest::Url::parse(target).ok()?;
    let host = url.host_str()?.to_owned();
    let port = url.port_or_known_default()?;
    Some((host, port))
}

/// Spawn the certificate watcher: checks every HTTPS monitor every 12 hours,
/// plus the daily RDAP domain-expiry checks for monitors that opt in. A
/// shutdown signal lets it stop between ticks instead of being aborted.
#[must_use]
pub fn spawn_watcher(
    pool: SqlitePool,
    config: watch::Receiver<Arc<Config>>,
    notifier: Notifiers,
    client: reqwest::Client,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let tls = match client_config() {
            Ok(tls) => tls,
            Err(err) => {
                error!("could not build TLS client config, cert checks disabled: {err:#}");
                return;
            }
        };

        let mut warned: HashMap<String, bool> = HashMap::new();
        let mut domain_warned: HashMap<String, bool> = HashMap::new();
        let mut ticker = tokio::time::interval(CHECK_INTERVAL);

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => break,
            }
            let snapshot = config.borrow().clone();
            let threshold_days = i64::from(snapshot.alerts.cert_expiry_days);
            let now = chrono::Utc::now().timestamp();

            // Forget monitors that no longer exist so the alert-dedup maps stay bounded.
            warned.retain(|id, _| snapshot.monitors.iter().any(|m| &m.id == id));
            domain_warned.retain(|id, _| snapshot.monitors.iter().any(|m| &m.id == id));

            check_domains(
                &pool,
                &snapshot,
                &notifier,
                &client,
                &mut domain_warned,
                now,
            )
            .await;

            for monitor in snapshot.monitors.iter().filter(|m| m.checks_cert()) {
                let Some((host, port)) = host_port(&monitor.target) else {
                    warn!(monitor = %monitor.id, "cannot parse host for cert check");
                    continue;
                };

                match fetch(&tls, &host, port, monitor.timeout()).await {
                    Ok((not_after, fingerprint)) => {
                        if let Err(err) = db::upsert_cert(&pool, &monitor.id, not_after, now).await
                        {
                            warn!(monitor = %monitor.id, "failed to store cert info: {err:#}");
                        }
                        let days_left = (not_after - now) / SECONDS_PER_DAY;
                        info!(monitor = %monitor.id, days_left, "checked TLS certificate");

                        let expiring = days_left <= threshold_days;
                        let already_warned = warned.get(&monitor.id).copied().unwrap_or(false);
                        // Mute (and don't record the warned state) during maintenance,
                        // so the alert can still fire once the window ends.
                        let muted = snapshot.in_maintenance(&monitor.id, chrono::Utc::now());
                        if !muted {
                            if expiring && !already_warned {
                                notifier
                                    .load_full()
                                    .dispatch(
                                        Event::CertExpiring {
                                            monitor: &monitor.name,
                                            days_left,
                                        },
                                        monitor.notify.as_deref(),
                                    )
                                    .await;
                            }
                            warned.insert(monitor.id.clone(), expiring);
                        }

                        // Certificate pinning: compare BEFORE storing, alert,
                        // then remember the observed fingerprint so the same
                        // mismatch alerts once, not every check. A change during
                        // maintenance is muted like any other alert (a renewal
                        // mid-window is the deploy, not an attack) but still
                        // recorded.
                        if let Some(expected_pin) = &monitor.cert_pin {
                            let stored = match db::cert_pin_fingerprint(&pool, &monitor.id).await {
                                Ok(stored) => stored,
                                Err(err) => {
                                    warn!(monitor = %monitor.id, "failed to read cert pin: {err:#}");
                                    continue;
                                }
                            };
                            if !muted
                                && let Some(old) =
                                    pin_alert(expected_pin, stored.as_deref(), &fingerprint)
                            {
                                notifier
                                    .load_full()
                                    .dispatch(
                                        Event::CertChanged {
                                            monitor: &monitor.name,
                                            old_fingerprint: old,
                                            new_fingerprint: &fingerprint,
                                        },
                                        monitor.notify.as_deref(),
                                    )
                                    .await;
                            }
                            if stored.as_deref() != Some(fingerprint.as_str())
                                && let Err(err) =
                                    db::upsert_cert_pin(&pool, &monitor.id, &fingerprint, now).await
                            {
                                warn!(monitor = %monitor.id, "failed to store cert pin: {err:#}");
                            }
                        }
                    }
                    Err(err) => warn!(monitor = %monitor.id, "cert check failed: {err:#}"),
                }
            }
        }
    })
}

/// One pass of the RDAP domain-expiry checks: for each monitor with a
/// `domain_expiry`, refresh the registry's expiration date at most daily
/// (gated on the stored `checked_at`, so restarts never re-query early) and
/// alert once when it enters the warning window - the same edge-triggered,
/// maintenance-muted policy as the certificate expiry above.
async fn check_domains(
    pool: &SqlitePool,
    snapshot: &Config,
    notifier: &Notifiers,
    client: &reqwest::Client,
    domain_warned: &mut HashMap<String, bool>,
    now: i64,
) {
    let threshold_days = i64::from(snapshot.alerts.domain_expiry_days);
    for monitor in &snapshot.monitors {
        let Some(domain) = &monitor.domain_expiry else {
            continue;
        };
        let stored = match db::domain_expiry(pool, &monitor.id).await {
            Ok(stored) => stored,
            Err(err) => {
                warn!(monitor = %monitor.id, "failed to read domain expiry: {err:#}");
                continue;
            }
        };
        // A changed `domain_expiry` in the config re-queries immediately.
        let expires_at = match stored {
            Some((ref stored_domain, expires_at, checked_at))
                if stored_domain == domain && now - checked_at < DOMAIN_CHECK_SECS =>
            {
                expires_at
            }
            _ => match crate::rdap::domain_expiration(client, domain).await {
                Ok(expires_at) => {
                    if let Err(err) =
                        db::upsert_domain_expiry(pool, &monitor.id, domain, expires_at, now).await
                    {
                        warn!(monitor = %monitor.id, "failed to store domain expiry: {err:#}");
                    }
                    let days_left = (expires_at - now) / SECONDS_PER_DAY;
                    info!(monitor = %monitor.id, domain, days_left, "checked domain expiry (RDAP)");
                    expires_at
                }
                Err(err) => {
                    warn!(monitor = %monitor.id, domain, "RDAP domain check failed: {err:#}");
                    continue;
                }
            },
        };

        let days_left = (expires_at - now) / SECONDS_PER_DAY;
        let expiring = days_left <= threshold_days;
        let already_warned = domain_warned.get(&monitor.id).copied().unwrap_or(false);
        // Mute (without recording the warned state) during maintenance, so
        // the alert still fires once the window ends.
        if snapshot.in_maintenance(&monitor.id, chrono::Utc::now()) {
            continue;
        }
        if expiring && !already_warned {
            notifier
                .load_full()
                .dispatch(
                    Event::DomainExpiring {
                        monitor: &monitor.name,
                        domain,
                        days_left,
                    },
                    monitor.notify.as_deref(),
                )
                .await;
        }
        domain_warned.insert(monitor.id.clone(), expiring);
    }
}

/// The pinning verdict for one check: `Some(old)` when an alert should fire,
/// where `old` is the fingerprint to report as previous (the last seen one,
/// falling back to the configured pin on the very first check). No alert when
/// the observed key matches the pin, or when it was already seen - the caller
/// stores each observed fingerprint, so a mismatch alerts once per change
/// (and survives restarts) instead of on every check.
fn pin_alert<'a>(expected: &'a str, stored: Option<&'a str>, observed: &str) -> Option<&'a str> {
    // Case-insensitive on the configured pin: `parse()` canonicalizes it to
    // lowercase, but a mixed-case pin from any other path must not silently
    // disable pinning. `stored` is always our own lowercase sha256_hex.
    if observed.eq_ignore_ascii_case(expected) || stored == Some(observed) {
        return None;
    }
    Some(stored.unwrap_or(expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_alert_fires_once_per_new_fingerprint() {
        // Matches the pin: never alerts, whatever was seen before.
        assert_eq!(pin_alert("aaa", None, "aaa"), None);
        assert_eq!(pin_alert("aaa", Some("bbb"), "aaa"), None);
        // First mismatch: alert, reporting the pin as the previous value.
        assert_eq!(pin_alert("aaa", None, "bbb"), Some("aaa"));
        // Same mismatch already recorded: no re-alert.
        assert_eq!(pin_alert("aaa", Some("bbb"), "bbb"), None);
        // The key changed again: alert with the last seen value as previous.
        assert_eq!(pin_alert("aaa", Some("bbb"), "ccc"), Some("bbb"));
    }

    #[test]
    fn sha256_hex_is_lowercase_hex() {
        let digest = sha256_hex(b"hora");
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(digest, sha256_hex(b"hora"));
    }
}
