//! TCP connect checks.

use super::FailureKind;
use std::time::Instant;

use super::family::Family;
use super::{Outcome, millis, over_threshold};
use crate::config::{Monitor, TcpSpec};

/// Connect to the `host:port` target, trying its addresses happy-eyeballs
/// style (see [`crate::connect::connect`]): one dead address of a dual-stack
/// name does not turn the check into a timeout.
pub(super) async fn tcp(monitor: &Monitor, spec: &TcpSpec) -> Outcome {
    let start = Instant::now();
    let Some((host, port)) = crate::config::split_host_port(&spec.target) else {
        // Validated at load; only an ad-hoc target can get here.
        return Outcome::down(FailureKind::Other, "invalid host:port target".to_owned());
    };
    let result = crate::connect::connect(host, port, monitor.timeout()).await;
    connected(monitor, start, result)
}

/// TCP for one address family: resolve the `host:port` target ourselves and
/// connect to the addresses of that family.
pub(super) async fn tcp_family(monitor: &Monitor, spec: &TcpSpec, family: Family) -> Outcome {
    let start = Instant::now();
    // Bounded like the connect below: an unanswered resolver is a failed
    // probe, not a monitor loop parked for good.
    let lookup = tokio::time::timeout(
        monitor.timeout(),
        tokio::net::lookup_host(spec.target.as_str()),
    );
    let Ok(Ok(addrs)) = lookup.await else {
        return Outcome::down(
            FailureKind::Unresolvable,
            "could not resolve host".to_owned(),
        );
    };
    let addrs: Vec<_> = addrs.filter(|addr| family.matches(addr.ip())).collect();
    if addrs.is_empty() {
        return Outcome::down(
            FailureKind::Other,
            format!("no {} address for host", family.label()),
        );
    }
    let result = tokio::time::timeout(monitor.timeout(), crate::connect::race(addrs))
        .await
        .unwrap_or_else(|_elapsed| {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "connection timed out",
            ))
        });
    connected(monitor, start, result)
}

fn connected(
    monitor: &Monitor,
    start: Instant,
    result: std::io::Result<tokio::net::TcpStream>,
) -> Outcome {
    match result {
        Ok(_stream) => {
            let latency = millis(start.elapsed());
            Outcome::up(
                over_threshold(latency, monitor.degraded_over_ms),
                Some(latency),
                None,
            )
        }
        Err(err) if err.kind() == std::io::ErrorKind::TimedOut => Outcome::down(
            FailureKind::ConnectionTimedOut,
            "connection timed out".to_owned(),
        ),
        Err(err) if super::local::is_exhaustion(&err) => {
            Outcome::down(FailureKind::Local, err.to_string())
        }
        Err(err) => Outcome::down(FailureKind::Other, err.to_string()),
    }
}
