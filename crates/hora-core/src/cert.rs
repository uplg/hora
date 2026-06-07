//! TLS certificate expiry monitoring.
//!
//! A periodic task opens a TLS connection to each HTTPS monitor, reads the leaf
//! certificate's `notAfter`, stores it, and emits a [`hora_notify::Event`] once
//! expiry is within the configured window. Verification is intentionally
//! skipped: we only read the validity dates, independent of chain trust.

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
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
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

/// Connect, handshake, and return the leaf certificate's `notAfter` (unix secs).
async fn fetch(
    config: &Arc<ClientConfig>,
    host: &str,
    port: u16,
    timeout: Duration,
) -> anyhow::Result<i64> {
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
    Ok(parsed.validity().not_after.timestamp())
}

/// Extract `(host, port)` from a monitor target URL (port defaults to 443).
fn host_port(target: &str) -> Option<(String, u16)> {
    let url = reqwest::Url::parse(target).ok()?;
    let host = url.host_str()?.to_owned();
    let port = url.port_or_known_default()?;
    Some((host, port))
}

/// Spawn the certificate watcher: checks every HTTPS monitor every 12 hours.
pub fn spawn_watcher(pool: SqlitePool, config: watch::Receiver<Arc<Config>>, notifier: Notifiers) {
    tokio::spawn(async move {
        let tls = match client_config() {
            Ok(tls) => tls,
            Err(err) => {
                error!("could not build TLS client config, cert checks disabled: {err:#}");
                return;
            }
        };

        let mut warned: HashMap<String, bool> = HashMap::new();
        let mut ticker = tokio::time::interval(CHECK_INTERVAL);

        loop {
            ticker.tick().await;
            let snapshot = config.borrow().clone();
            let threshold_days = i64::from(snapshot.alerts.cert_expiry_days);
            let now = chrono::Utc::now().timestamp();

            // Forget monitors that no longer exist so the alert-dedup map stays bounded.
            warned.retain(|id, _| snapshot.monitors.iter().any(|m| &m.id == id));

            for monitor in snapshot.monitors.iter().filter(|m| m.checks_cert()) {
                let Some((host, port)) = host_port(&monitor.target) else {
                    warn!(monitor = %monitor.id, "cannot parse host for cert check");
                    continue;
                };

                match fetch(&tls, &host, port, monitor.timeout()).await {
                    Ok(not_after) => {
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
                    }
                    Err(err) => warn!(monitor = %monitor.id, "cert check failed: {err:#}"),
                }
            }
        }
    });
}
