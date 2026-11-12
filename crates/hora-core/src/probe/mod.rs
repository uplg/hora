//! Probing logic: turn a monitor into a single [`Outcome`].
//!
//! One submodule per probe kind, plus the dual-stack (IPv4 + IPv6) fan-out,
//! the failure snapshot rendering and the public-visibility policy for
//! failure reasons.

mod dns;
mod family;
mod http;
mod icmp;
pub mod local;
mod reason;
mod snapshot;
mod tcp;
#[cfg(test)]
mod tests;

/// What a probe does with a monitored server's answer, for the fuzz targets
/// in `fuzz/` (built with `--cfg fuzzing` only).
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzz {
    use crate::config::HttpSpec;

    /// The body assertions of `spec` against `body`: the failure, if any.
    #[must_use]
    pub fn check_assertions(spec: &HttpSpec, body: &[u8]) -> Option<String> {
        super::http::check_assertions(spec, body)
    }

    /// The failure snapshot of an answer with these headers and this body,
    /// and the one-line snippet of the body.
    #[must_use]
    pub fn snapshot(headers: &reqwest::header::HeaderMap, body: &[u8]) -> (String, String) {
        let head = super::snapshot::render_head(
            reqwest::Version::HTTP_11,
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            headers,
        );
        (
            super::snapshot::render_snapshot(&head, body),
            super::snapshot::snippet(body),
        )
    }
}

pub use reason::{FailureKind, public_reason};

use std::net::IpAddr;
use std::time::Duration;

use reqwest::Client;

use crate::config::{Monitor, NetworkProbe};
use crate::status::CheckStatus;

use dns::dns;
use family::{DualStack, Family, dual_stack};
use http::http;
use icmp::icmp;
use tcp::tcp;

/// Result of a single probe.
#[derive(Debug)]
pub struct Outcome {
    pub status: CheckStatus,
    pub latency_ms: Option<i64>,
    pub status_code: Option<i64>,
    pub error: Option<String>,
    /// What kind of failure `error` describes (set with it), stored so the
    /// public page can show a safe category (see [`public_reason`]).
    pub reason: Option<FailureKind>,
    /// The failing HTTP response (status line, headers, start of the body),
    /// bounded; only set when the probe got a response back. Stored on the
    /// incident when the down is confirmed.
    pub snapshot: Option<String>,
}

impl Outcome {
    /// Whether the check passed (up or degraded).
    #[must_use]
    pub fn is_up(&self) -> bool {
        self.status.is_available()
    }

    /// Whether the check passed too slowly (or with a warning).
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        self.status == CheckStatus::Degraded
    }

    /// Whether the check could not run for lack of resources on this host:
    /// no verdict on the target, to be skipped rather than recorded.
    #[must_use]
    pub fn is_local_fault(&self) -> bool {
        self.reason == Some(FailureKind::Local)
    }

    /// A failed check that is this host's own exhaustion becomes a local
    /// fault: by the cause its error names when it names one, else by asking
    /// whether a socket can still be opened at all.
    pub(crate) fn excusing_exhaustion(self) -> Self {
        if self.is_up() || self.is_local_fault() {
            return self;
        }
        match local::exhausted_now() {
            Some(detail) => Self::down(FailureKind::Local, detail),
            None => self,
        }
    }

    pub(crate) fn down(kind: FailureKind, error: String) -> Self {
        Self {
            status: CheckStatus::Down,
            latency_ms: None,
            status_code: None,
            error: Some(error),
            reason: Some(kind),
            snapshot: None,
        }
    }

    /// The outcome of a passing check: up, or degraded when `slow`.
    pub(crate) fn up(slow: bool, latency_ms: Option<i64>, status_code: Option<i64>) -> Self {
        Self {
            status: CheckStatus::up_unless(slow),
            latency_ms,
            status_code,
            error: None,
            reason: None,
            snapshot: None,
        }
    }
}

/// Pause before a retry, long enough for a micro-blip (packet loss, a
/// connection reset mid-deploy) to pass, short next to any real interval.
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// Probe a monitor according to its kind, re-trying failures up to the
/// monitor's `probe_retries` (default 1). Only the final attempt is reported:
/// a blip that passes on retry never reaches the history, the page or the
/// error budget. Retries are logged so a flaky path stays visible in the logs
/// even when the recorded check ends up green.
///
/// Only the kinds with a live probe get here (`probe` is borrowed from the
/// monitor's own [`MonitorKind::network`](crate::config::MonitorKind::network)):
/// push monitors are judged from stored heartbeats, exec monitors by the exec
/// module.
#[must_use]
pub async fn run(client: &Client, monitor: &Monitor, probe: NetworkProbe<'_>) -> Outcome {
    let mut outcome = probe_once(client, monitor, probe).await;
    for attempt in 1..=monitor.probe_retries() {
        if outcome.is_up() || outcome.is_local_fault() {
            break;
        }
        tracing::info!(
            monitor = %monitor.id,
            attempt,
            error = outcome.error.as_deref().unwrap_or("unknown"),
            "probe failed, retrying"
        );
        tokio::time::sleep(RETRY_DELAY).await;
        outcome = probe_once(client, monitor, probe).await;
    }
    outcome.excusing_exhaustion()
}

async fn probe_once(client: &Client, monitor: &Monitor, probe: NetworkProbe<'_>) -> Outcome {
    if let Some(both) = DualStack::of(probe) {
        return dual_stack(monitor, both).await;
    }
    match probe {
        NetworkProbe::Http(spec) => http(client, monitor, spec).await,
        NetworkProbe::Tcp(spec) => tcp(monitor, spec).await,
        NetworkProbe::Icmp(spec) => icmp(monitor, spec).await,
        NetworkProbe::Dns(spec) => dns(monitor, spec).await,
    }
}

/// Resolve an ICMP target to a single IP: an IP literal is used directly,
/// otherwise DNS is consulted and the first address is taken (so an IPv4-only or
/// IPv6-only host resolves to the family it actually has). With a `family`, only
/// addresses of that family qualify.
async fn resolve(target: &str, family: Option<Family>) -> Option<IpAddr> {
    let wanted = |ip: IpAddr| family.is_none_or(|family| family.matches(ip));
    if let Ok(ip) = target.parse::<IpAddr>() {
        return wanted(ip).then_some(ip);
    }
    tokio::net::lookup_host((target, 0u16))
        .await
        .ok()?
        .map(|addr| addr.ip())
        .find(|ip| wanted(*ip))
}

fn over_threshold(latency_ms: i64, threshold: Option<i64>) -> bool {
    threshold.is_some_and(|limit| latency_ms > limit)
}

fn millis(elapsed: Duration) -> i64 {
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}
