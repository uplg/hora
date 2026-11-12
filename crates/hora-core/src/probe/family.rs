//! Dual-stack probing: run a check over IPv4 and IPv6 separately and combine
//! the two results.

use super::FailureKind;
use crate::status::CheckStatus;
use std::net::IpAddr;
use std::sync::OnceLock;

use reqwest::Client;

use super::Outcome;
use super::http::http;
use super::icmp::icmp_family;
use super::tcp::tcp_family;
use crate::config::{HttpSpec, IcmpSpec, Monitor, NetworkProbe, TcpSpec};

/// One IP address family of a dual-stack probe.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Family {
    V4,
    V6,
}

impl Family {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::V4 => "IPv4",
            Self::V6 => "IPv6",
        }
    }

    fn other(self) -> Self {
        match self {
            Self::V4 => Self::V6,
            Self::V6 => Self::V4,
        }
    }

    pub(super) fn matches(self, ip: IpAddr) -> bool {
        match self {
            Self::V4 => ip.is_ipv4(),
            Self::V6 => ip.is_ipv6(),
        }
    }

    /// The family's unspecified address; binding a client's local end to it
    /// restricts its connections to this family.
    fn unspecified(self) -> IpAddr {
        match self {
            Self::V4 => IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            Self::V6 => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        }
    }
}

/// Probe both address families concurrently and merge the outcomes: a service
/// whose IPv6 (or IPv4) is silently dead behind a healthy sibling goes down
/// with the broken family named in the reason.
/// A probe that can be run once per address family: the kinds `dual_stack`
/// applies to.
#[derive(Clone, Copy)]
pub(super) enum DualStack<'a> {
    Http(&'a HttpSpec),
    Tcp(&'a TcpSpec),
    Icmp(&'a IcmpSpec),
}

impl<'a> DualStack<'a> {
    /// The probe, when it is set to run over both families.
    pub(super) fn of(probe: NetworkProbe<'a>) -> Option<Self> {
        match probe {
            NetworkProbe::Http(spec) if spec.dual_stack => Some(Self::Http(spec)),
            NetworkProbe::Tcp(spec) if spec.dual_stack => Some(Self::Tcp(spec)),
            NetworkProbe::Icmp(spec) if spec.dual_stack => Some(Self::Icmp(spec)),
            _ => None,
        }
    }
}

pub(super) async fn dual_stack(monitor: &Monitor, probe: DualStack<'_>) -> Outcome {
    let (v4, v6) = tokio::join!(
        probe_family(monitor, probe, Family::V4),
        probe_family(monitor, probe, Family::V6)
    );
    combine(&v4, &v6)
}

async fn probe_family(monitor: &Monitor, probe: DualStack<'_>, family: Family) -> Outcome {
    match probe {
        // The per-monitor client cannot be steered per family, so dual-stack
        // probes share one family-bound client per family.
        DualStack::Http(spec) => match family_client(family) {
            Ok(client) => http(&client, monitor, spec).await,
            Err(err) => Outcome::down(
                FailureKind::Other,
                format!("could not build probe client: {err}"),
            ),
        },
        DualStack::Tcp(spec) => tcp_family(monitor, spec, family).await,
        DualStack::Icmp(spec) => icmp_family(monitor, spec, Some(family)).await,
    }
}

/// The process-wide probe client bound to `family`, built on first use. Every
/// dual-stack monitor shares it: the client carries no per-monitor setting
/// (headers go per request, dual-stack excludes proxies), and sharing keeps
/// one TLS setup per family - like the shared client of single-stack probes -
/// instead of two fresh clients on every probe. Connections are not reused
/// (see [`probe_client`](crate::http::probe_client)).
fn family_client(family: Family) -> reqwest::Result<Client> {
    static V4: OnceLock<Client> = OnceLock::new();
    static V6: OnceLock<Client> = OnceLock::new();
    let cell = match family {
        Family::V4 => &V4,
        Family::V6 => &V6,
    };
    if let Some(client) = cell.get() {
        return Ok(client.clone());
    }
    let client = crate::http::probe_client_family(family.unspecified())?;
    Ok(cell.get_or_init(|| client).clone())
}

/// Merge the two per-family outcomes of a dual-stack probe. Both up → up, with
/// the *worst* latency (so `degraded_over_ms` judges the slower path). One
/// family down → down: the dual-stack contract is broken even though some
/// clients still reach the service; the reason names the failing family and
/// the latency reflects the surviving path. Both down → down with both reasons.
pub(super) fn combine(v4: &Outcome, v6: &Outcome) -> Outcome {
    match (v4.is_up(), v6.is_up()) {
        (true, true) => Outcome {
            status: CheckStatus::up_unless(v4.is_degraded() || v6.is_degraded()),
            latency_ms: match (v4.latency_ms, v6.latency_ms) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            },
            status_code: v4.status_code.or(v6.status_code),
            error: None,
            reason: None,
            snapshot: None,
        },
        (false, true) => one_family_down(Family::V4, v4, v6),
        (true, false) => one_family_down(Family::V6, v6, v4),
        (false, false) => Outcome {
            status: CheckStatus::Down,
            latency_ms: None,
            status_code: v4.status_code.or(v6.status_code),
            error: Some(format!(
                "IPv4 and IPv6 failing: {}; {}",
                v4.error.as_deref().unwrap_or("unknown error"),
                v6.error.as_deref().unwrap_or("unknown error")
            )),
            reason: Some(FailureKind::BothFamiliesFailing),
            snapshot: v4.snapshot.clone().or_else(|| v6.snapshot.clone()),
        },
    }
}

/// The down outcome for a single broken family: the failure's detail and
/// status code, the healthy family's latency.
fn one_family_down(failed: Family, failure: &Outcome, healthy: &Outcome) -> Outcome {
    Outcome {
        status: CheckStatus::Down,
        latency_ms: healthy.latency_ms,
        status_code: failure.status_code,
        error: Some(format!(
            "{} failing: {} ({} ok)",
            failed.label(),
            failure.error.as_deref().unwrap_or("unknown error"),
            failed.other().label()
        )),
        reason: Some(match failed {
            Family::V4 => FailureKind::Ipv4Failing,
            Family::V6 => FailureKind::Ipv6Failing,
        }),
        snapshot: failure.snapshot.clone(),
    }
}
