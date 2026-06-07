//! Web layer: the server-rendered status page and the JSON API.

mod error;
mod handlers;
mod render;
mod routes;
mod summary;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use axum::http::{HeaderName, Request};
use hora_core::config::Config;
use sqlx::SqlitePool;
use tokio::sync::{Mutex, watch};
use tower_governor::errors::GovernorError;
use tower_governor::key_extractor::{KeyExtractor, SmartIpKeyExtractor};

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

/// Lock-free reads (`value`) plus a single-flight build gate (`build`).
#[derive(Default)]
pub(crate) struct Cache {
    value: ArcSwapOption<Cached>,
    build: Mutex<()>,
}

/// Shared application state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    pool: SqlitePool,
    config: watch::Receiver<Arc<Config>>,
    cache: Arc<Cache>,
}

impl AppState {
    #[must_use]
    pub fn new(pool: SqlitePool, config: watch::Receiver<Arc<Config>>) -> Self {
        Self {
            pool,
            config,
            cache: Arc::new(Cache::default()),
        }
    }
}

/// Rate-limit key extractor that trusts a configured header (e.g.
/// `cf-connecting-ip` behind Cloudflare) for the client IP, falling back to the
/// smart detection (x-forwarded-for / x-real-ip / forwarded / peer) when it is
/// absent or unparseable.
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
        SmartIpKeyExtractor.extract(req)
    }
}

impl ConfiguredIp {
    /// Parse the configured header name once; an invalid name is ignored (with a
    /// warning) and the extractor behaves like `SmartIpKeyExtractor`.
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
/// and cache it. The cache busts immediately when the config is reloaded.
pub(crate) async fn summary_for(
    pool: &SqlitePool,
    config: &Arc<Config>,
    cache: &Cache,
) -> Arc<Summary> {
    if let Some(fresh) = fresh_summary(cache, config) {
        return fresh;
    }
    // Only one task builds at a time; the rest wait and reuse the result.
    let _build = cache.build.lock().await;
    if let Some(fresh) = fresh_summary(cache, config) {
        return fresh;
    }
    let summary = Arc::new(build_summary(pool, config).await);
    cache.value.store(Some(Arc::new(Cached {
        at: Instant::now(),
        config: Arc::clone(config),
        summary: Arc::clone(&summary),
    })));
    summary
}

pub(crate) fn fresh_summary(cache: &Cache, config: &Arc<Config>) -> Option<Arc<Summary>> {
    let cached = cache.value.load_full()?;
    let fresh = Arc::ptr_eq(&cached.config, config) && cached.at.elapsed() < SUMMARY_CACHE_TTL;
    fresh.then(|| Arc::clone(&cached.summary))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configured_ip_prefers_header_then_falls_back() {
        let extractor = ConfiguredIp::from_config(Some("cf-connecting-ip"));

        // Header present: it wins over x-forwarded-for.
        let with_header = Request::builder()
            .header("cf-connecting-ip", "203.0.113.7")
            .header("x-forwarded-for", "10.0.0.1")
            .body(())
            .expect("request");
        assert_eq!(
            extractor.extract(&with_header).unwrap(),
            "203.0.113.7".parse::<IpAddr>().unwrap()
        );

        // Header absent: fall back to x-forwarded-for.
        let without_header = Request::builder()
            .header("x-forwarded-for", "10.0.0.1")
            .body(())
            .expect("request");
        assert_eq!(
            extractor.extract(&without_header).unwrap(),
            "10.0.0.1".parse::<IpAddr>().unwrap()
        );
    }
}
