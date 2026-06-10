//! Web layer: the server-rendered status page and the JSON API.

mod error;
mod handlers;
mod history;
mod metrics;
mod render;
mod routes;
mod summary;
mod text;

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use axum::http::{HeaderName, Request};
use hora_core::config::Config;
use sqlx::SqlitePool;
use tokio::sync::{Mutex, watch};
use tower_governor::errors::GovernorError;
use tower_governor::key_extractor::{KeyExtractor, PeerIpKeyExtractor};

use crate::summary::{Summary, build_summary};

pub use routes::router;

pub(crate) const SECONDS_PER_HOUR: i64 = 3_600;
pub(crate) const MAX_LATENCY_HOURS: i64 = 24 * 30;
pub(crate) const SUMMARY_CACHE_TTL: Duration = Duration::from_secs(5);
/// Cap on a pushed heartbeat message, so the endpoint can't bloat the database.
pub(crate) const MAX_PUSH_MSG_CHARS: usize = 500;
/// Cap on points returned by the latency endpoint (evenly downsampled beyond it).
pub(crate) const MAX_LATENCY_POINTS: usize = 2000;
/// Number of time buckets a 24h card sparkline is averaged into, so its size is
/// fixed no matter the check frequency (the chart is `CHART_W`px wide).
pub(crate) const SPARK_BUCKETS: i64 = 120;

pub(crate) const FAVICON_SVG: &str = include_str!("../assets/favicon.svg");
pub(crate) const FONT_WOFF2: &[u8] = include_bytes!("../assets/CalSans-SemiBold.woff2");

// The page is fully self-contained (inline styles, same-origin font/icon, no JS).
pub(crate) const CSP: &str = "default-src 'self'; script-src 'none'; style-src 'self' 'unsafe-inline'; \
     img-src 'self' data:; font-src 'self'; base-uri 'none'; frame-ancestors 'none'";

/// A cached summary, valid while the config is unchanged and within the TTL.
pub(crate) struct Cached {
    at: Instant,
    config: Arc<Config>,
    summary: Arc<Summary>,
}

/// Lock-free reads (one slot per audience) plus a single-flight build gate.
/// The `public` slot caches the summary filtered to public monitors; the
/// `full` slot the unfiltered view served to authenticated callers (admin
/// page views, Prometheus scrapes) - both bust on config reload.
#[derive(Default)]
pub(crate) struct Cache {
    public: ArcSwapOption<Cached>,
    full: ArcSwapOption<Cached>,
    build: Mutex<()>,
}

/// Shared application state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    pool: SqlitePool,
    config: watch::Receiver<Arc<Config>>,
    cache: Arc<Cache>,
    /// The scheduler's liveness beacon, written by the monitor loops and read by
    /// `/healthz` to report whether the scheduler is still ticking.
    last_tick: Arc<AtomicU64>,
}

impl AppState {
    #[must_use]
    pub fn new(
        pool: SqlitePool,
        config: watch::Receiver<Arc<Config>>,
        last_tick: Arc<AtomicU64>,
    ) -> Self {
        Self {
            pool,
            config,
            cache: Arc::new(Cache::default()),
            last_tick,
        }
    }
}

/// Rate-limit key extractor that trusts a configured header (e.g.
/// `cf-connecting-ip` behind Cloudflare) for the client IP, falling back to the
/// real TCP peer address when it is absent or unparseable.
///
/// The fallback is deliberately the peer socket - *not* `x-forwarded-for` or
/// other forwarded headers - because a direct client can set those freely: a
/// smart extractor would let an attacker mint a fresh rate-limit bucket per
/// request (and inflate the keyed-bucket map) simply by rotating the header.
/// Forwarded headers are honored only when an operator behind a trusted proxy
/// names one via `server.client_ip_header`.
#[derive(Clone)]
pub(crate) struct ConfiguredIp {
    header: Option<HeaderName>,
}

impl KeyExtractor for ConfiguredIp {
    type Key = IpAddr;

    fn extract<T>(&self, req: &Request<T>) -> Result<Self::Key, GovernorError> {
        if let Some(header) = &self.header
            && let Some(ip) = req
                .headers()
                .get(header)
                .and_then(|value| value.to_str().ok())
                .and_then(|raw| raw.split(',').next())
                .and_then(|first| first.trim().parse::<IpAddr>().ok())
        {
            return Ok(ip);
        }
        PeerIpKeyExtractor.extract(req)
    }
}

impl ConfiguredIp {
    /// Parse the configured header name once; an invalid name is ignored (with a
    /// warning) and the extractor falls back to the peer address.
    pub(crate) fn from_config(name: Option<&str>) -> Self {
        let header = name.and_then(|name| {
            name.parse::<HeaderName>()
                .inspect_err(|_| {
                    tracing::warn!("invalid server.client_ip_header {name:?}, ignoring");
                })
                .ok()
        });
        Self { header }
    }
}

// --- Summary cache (lock-free read + single-flight build) ----------------

/// Return a fresh-enough cached summary, or build exactly one (single-flight)
/// and cache it. `full` selects the unfiltered view (private monitors
/// included) served to authenticated callers. The cache busts immediately
/// when the config is reloaded.
pub(crate) async fn summary_for(
    pool: &SqlitePool,
    config: &Arc<Config>,
    cache: &Cache,
    full: bool,
) -> Arc<Summary> {
    let slot = if full { &cache.full } else { &cache.public };
    if let Some(fresh) = fresh_summary(slot, config) {
        return fresh;
    }
    // Only one task builds at a time; the rest wait and reuse the result.
    let _build = cache.build.lock().await;
    if let Some(fresh) = fresh_summary(slot, config) {
        return fresh;
    }
    let summary = Arc::new(build_summary(pool, config, full).await);
    slot.store(Some(Arc::new(Cached {
        at: Instant::now(),
        config: Arc::clone(config),
        summary: Arc::clone(&summary),
    })));
    summary
}

pub(crate) fn fresh_summary(
    slot: &ArcSwapOption<Cached>,
    config: &Arc<Config>,
) -> Option<Arc<Summary>> {
    let cached = slot.load_full()?;
    let fresh = Arc::ptr_eq(&cached.config, config) && cached.at.elapsed() < SUMMARY_CACHE_TTL;
    fresh.then(|| Arc::clone(&cached.summary))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configured_ip_prefers_header_then_falls_back_to_peer() {
        use axum::extract::ConnectInfo;
        use std::net::SocketAddr;

        let extractor = ConfiguredIp::from_config(Some("cf-connecting-ip"));
        let peer = SocketAddr::from(([198, 51, 100, 9], 4444));

        // Header present: it wins over the peer address.
        let with_header = Request::builder()
            .header("cf-connecting-ip", "203.0.113.7")
            .extension(ConnectInfo(peer))
            .body(())
            .expect("request");
        assert_eq!(
            extractor.extract(&with_header).unwrap(),
            "203.0.113.7".parse::<IpAddr>().unwrap()
        );

        // Header absent: fall back to the real peer address, NOT a spoofable
        // x-forwarded-for the client supplied.
        let without_header = Request::builder()
            .header("x-forwarded-for", "10.0.0.1")
            .extension(ConnectInfo(peer))
            .body(())
            .expect("request");
        assert_eq!(extractor.extract(&without_header).unwrap(), peer.ip());
    }
}
