//! `hora probe`: a one-shot probe of a configured monitor or an ad-hoc target,
//! optionally confirmed from the peers.

use anyhow::Context as _;
use hora_core::config;
use hora_core::config::{ExpectedStatus, HttpMethod, Kind, ParseField as _, split_host_port};

use super::{CliError, usage};

/// `hora probe <monitor-id|target> [--confirm] [--kind http|tcp|icmp|dns]
/// [--method GET|HEAD] [--expect 200,401]`: a
/// one-shot ad-hoc probe from the terminal with full monitor semantics (status,
/// latency, HTTP/DNS assertions, TLS expiry). A bare argument matching a
/// configured monitor id is probed with that monitor's exact config; anything
/// else is an ad-hoc target whose kind is inferred (or forced with `--kind`),
/// and for an HTTP one `--method` and `--expect` stand for the monitor's
/// `method` and `expected_status`.
/// `--confirm` asks the configured peers to probe the same target - the
/// distributed "down for everyone or just me?" in one command. Exits non-zero
/// when the target is down, so it doubles as a scriptable health check.
pub(crate) async fn probe(args: &[String]) -> Result<(), CliError> {
    let parsed = parse_probe_args(args)?;
    let config = config::load_from(&config::path()).context("loading configuration")?;

    // A bare id (no --kind) reuses that monitor's full config - assertions,
    // cert pin, dns expectation and all. --kind always means "ad-hoc target".
    let configured = parsed
        .kind_override
        .is_none()
        .then(|| config.find_monitor(parsed.target))
        .flatten();
    let mut monitor = if let Some(found) = configured {
        found.clone()
    } else {
        let (kind, target) = match parsed.kind_override {
            Some(kind) => (kind, probe_target(kind, parsed.target)),
            None => infer_probe(parsed.target),
        };
        hora_core::config::Monitor::ad_hoc(kind, target).map_err(|err| usage(format!("{err:#}")))?
    };
    apply_http_options(&mut monitor, &parsed, configured.is_some())?;
    // Force confirmation on regardless of the monitor's own setting: the flag
    // is the explicit ask. confirm_with_peers is read by confirm::enabled only.
    if parsed.confirm {
        monitor.confirm_with_peers = Some(true);
    }
    // Push and exec monitors are evaluated from heartbeats / an external
    // command, not a live network probe - reject them whichever way the monitor
    // was resolved (a configured id, or an inferred kind).
    let Some(probe) = monitor.spec.network() else {
        return Err(CliError::Failed(format!(
            "hora probe runs a live network check, but {:?} is a {} monitor (no active probe)",
            monitor.id,
            monitor.kind().as_str()
        )));
    };

    let header = if configured.is_some() {
        format!(
            "{} ({}, {})",
            monitor.name,
            monitor.kind().as_str(),
            monitor.target()
        )
    } else {
        format!("{} {}", monitor.kind().as_str(), monitor.target())
    };
    println!("hora probe - {header}");
    println!();

    let client =
        hora_core::http::probe_client(monitor.proxy()).context("building the probe HTTP client")?;
    let outcome = hora_core::probe::run(&client, &monitor, probe).await;
    print_probe_report(&monitor, &outcome).await;

    if parsed.confirm {
        confirm_probe(&config, &monitor, &outcome).await?;
    }

    // Down exits non-zero so `hora probe url && deploy` works as a gate;
    // degraded is still up.
    if !outcome.is_up() {
        return Err(CliError::Silent);
    }
    Ok(())
}

/// A parsed `hora probe` invocation. Borrows the target straight from `args`.
#[derive(Debug)]
pub(super) struct ProbeArgs<'a> {
    pub(super) target: &'a str,
    pub(super) confirm: bool,
    pub(super) kind_override: Option<hora_core::config::Kind>,
    /// `--method`: the request an ad-hoc HTTP probe sends.
    pub(super) method: Option<HttpMethod>,
    /// `--expect`: the statuses an ad-hoc HTTP probe accepts as up.
    pub(super) expect: Option<ExpectedStatus>,
}

/// Give an ad-hoc HTTP probe its `--method` and `--expect`. They describe a
/// check that has no monitor: with a configured id the monitor's own settings
/// are the point of the command, and on another kind they mean nothing, so
/// both are refused rather than ignored.
pub(super) fn apply_http_options(
    monitor: &mut hora_core::config::Monitor,
    parsed: &ProbeArgs<'_>,
    configured: bool,
) -> Result<(), CliError> {
    if parsed.method.is_none() && parsed.expect.is_none() {
        return Ok(());
    }
    if configured {
        return Err(usage(format!(
            "--method and --expect are for an ad-hoc target: monitor {:?} is probed with its \
             own method and expected_status",
            monitor.id
        )));
    }
    let hora_core::config::MonitorKind::Http(spec) = &mut monitor.spec else {
        return Err(usage(format!(
            "--method and --expect need an http target, and this is a {} probe",
            monitor.kind().as_str()
        )));
    };
    if let Some(method) = parsed.method {
        spec.method = method;
    }
    if let Some(expect) = &parsed.expect {
        spec.expected_status = Some(expect.clone());
    }
    Ok(())
}

/// Parse `hora probe`'s arguments: a usage error on anything malformed (a
/// missing target, an unknown flag, a bad `--kind`, extra args).
pub(super) fn parse_probe_args(args: &[String]) -> Result<ProbeArgs<'_>, CliError> {
    let mut target: Option<&str> = None;
    let mut confirm = false;
    let mut kind_override: Option<hora_core::config::Kind> = None;
    let mut method = None;
    let mut expect = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--confirm" => confirm = true,
            "--kind" => {
                let raw = iter.next().map_or("", String::as_str);
                kind_override =
                    Some(parse_probe_kind(raw).ok_or_else(|| {
                        usage("--kind must be one of: http, tcp, icmp (ping), dns")
                    })?);
            }
            "--method" => {
                let raw = iter.next().map_or("", String::as_str);
                method = Some(
                    HttpMethod::parse_field(raw)
                        .map_err(|err| usage(format!("--method: {err}")))?,
                );
            }
            "--expect" => {
                let raw = iter.next().map_or("", String::as_str);
                expect = Some(ExpectedStatus::parse_list(raw).map_err(|err| {
                    usage(format!(
                        "--expect: {err} (statuses or classes separated by commas: 200,401 or \
                         2xx,401)"
                    ))
                })?);
            }
            flag if flag.starts_with('-') => {
                return Err(usage(format!(
                    "Unknown option {flag:?} (usage: hora probe <id|target> [--confirm] \
                     [--kind K] [--method GET|HEAD] [--expect 200,401])"
                )));
            }
            value if target.is_none() => target = Some(value),
            extra => {
                return Err(usage(format!(
                    "Unexpected argument {extra:?} - hora probe takes a single target"
                )));
            }
        }
    }
    let target = target.ok_or_else(|| {
        usage(
            "Usage: hora probe <monitor-id|target> [--confirm] [--kind http|tcp|icmp|dns] \
             [--method GET|HEAD] [--expect 200,401]",
        )
    })?;
    Ok(ProbeArgs {
        target,
        confirm,
        kind_override,
        method,
        expect,
    })
}

/// Print the probe outcome, then the TLS expiry for https targets - the latter
/// fail-open like the watcher: a handshake failure annotates, never changes the
/// verdict.
async fn print_probe_report(
    monitor: &hora_core::config::Monitor,
    outcome: &hora_core::probe::Outcome,
) {
    let status = if !outcome.is_up() {
        "DOWN"
    } else if outcome.is_degraded() {
        "DEGRADED (up but slow)"
    } else {
        "UP"
    };
    println!("  status    {status}");
    println!(
        "  latency   {}",
        outcome
            .latency_ms
            .map_or_else(|| "-".to_owned(), |ms| format!("{ms} ms"))
    );
    if let Some(code) = outcome.status_code {
        println!("  code      {code}");
    }
    if let Some(error) = &outcome.error {
        println!("  error     {}", hora_core::fmt::printable(error));
    }
    if let Some(first_line) = outcome
        .snapshot
        .as_deref()
        .and_then(|snapshot| snapshot.lines().next())
    {
        println!("  answered  {}", hora_core::fmt::printable(first_line));
    }

    // Only read the certificate when the probe actually reached the server (up,
    // or a response like a 5xx where TLS still answered). A transport failure
    // means the cert handshake would just hit the same timeout twice.
    let reached_server = outcome.is_up() || outcome.status_code.is_some();
    let https =
        monitor.kind() == hora_core::config::Kind::Http && monitor.target().starts_with("https://");
    let starttls = hora_core::cert::starttls_of(monitor);
    if reached_server && (https || starttls.is_some()) {
        // Read exactly like the watcher: STARTTLS monitors negotiate in
        // plaintext first, proxied ones go through their proxy.
        match hora_core::cert::inspect_monitor(monitor).await {
            Ok(cert) => println!(
                "  cert      {} (expires {})",
                cert_left_phrase(cert.not_after - chrono::Utc::now().timestamp()),
                format_date(cert.not_after)
            ),
            Err(err) => println!("  cert      could not read certificate: {err}"),
        }
    }
}

/// Ask the configured peers for their verdict on the target and print the
/// multi-vantage summary from this node's perspective: a local down asks "down
/// for everyone or just me?", a local up asks "is it up from elsewhere too?".
/// Fail-open: peers being absent or unreachable never errors.
async fn confirm_probe(
    config: &hora_core::config::Config,
    monitor: &hora_core::config::Monitor,
    outcome: &hora_core::probe::Outcome,
) -> anyhow::Result<()> {
    let plain = hora_core::http::client(None).context("building the confirm HTTP client")?;
    match hora_core::mesh::confirm::confirm_verdict(&plain, config, monitor, outcome.is_up()).await
    {
        Some(verdict) => println!("  vantage   {verdict}"),
        None => {
            println!(
                "  vantage   no peers to ask - needs [health].id and [[peers]] with a ping_url"
            );
        }
    }
    Ok(())
}

/// Parse a `--kind` value into a probeable monitor kind (push/exec excluded).
fn parse_probe_kind(raw: &str) -> Option<hora_core::config::Kind> {
    use hora_core::config::Kind;
    match raw.to_ascii_lowercase().as_str() {
        "http" => Some(Kind::Http),
        "tcp" => Some(Kind::Tcp),
        "icmp" | "ping" => Some(Kind::Icmp),
        "dns" => Some(Kind::Dns),
        _ => None,
    }
}

/// Human phrase for the time a certificate has left, handling the
/// already-expired case: `"30 d left"`, `"10 h left"`, `"EXPIRED 5 h ago"`.
/// Under two days it counts hours ("0 d left" with ten hours to go reads as
/// already gone), and minutes in the last hour.
fn cert_left_phrase(secs_left: i64) -> String {
    let span = hora_core::fmt::elapsed(secs_left.unsigned_abs());
    if secs_left < 0 {
        format!("EXPIRED {span} ago")
    } else {
        format!("{span} left")
    }
}

/// Format a unix timestamp as a `YYYY-MM-DD` date (UTC).
fn format_date(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0).map_or_else(
        || timestamp.to_string(),
        |dt| dt.format("%Y-%m-%d").to_string(),
    )
}

// --- Target inference (no configured monitor, no `--kind`) -----------------

/// Classify a bare `hora probe` target (no `--kind`) into a kind and a
/// normalized target:
///
/// - an `http(s)://` URL is an HTTP check, kept as-is;
/// - a bare IP is an ICMP ping (a host to reach), even IPv6 (all colons);
/// - an explicit `host:port` or `[ipv6]:port` is a TCP connect;
/// - a bare **hostname** is an HTTPS check (`https://` prepended): a name
///   denotes a web service far more often than a ping target, and ICMP is
///   widely filtered, so a ping default would cry wolf.
///
/// DNS is never inferred - it needs a record type - so use `--kind dns`.
#[must_use]
fn infer_probe(raw: &str) -> (Kind, String) {
    let raw = raw.trim();
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return (Kind::Http, raw.to_owned());
    }
    if raw.parse::<std::net::IpAddr>().is_ok() {
        return (Kind::Icmp, raw.to_owned());
    }
    if split_host_port(raw).is_some() {
        return (Kind::Tcp, raw.to_owned());
    }
    (Kind::Http, format!("https://{raw}"))
}

/// The target for an explicit `--kind`: an `http` kind without a scheme gets
/// `https://` prepended so it is a valid URL; every other kind takes the raw
/// target unchanged.
#[must_use]
fn probe_target(kind: Kind, raw: &str) -> String {
    let raw = raw.trim();
    if kind == Kind::Http && !raw.starts_with("http://") && !raw.starts_with("https://") {
        format!("https://{raw}")
    } else {
        raw.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_time_left_counts_hours_under_a_day() {
        assert_eq!(cert_left_phrase(10 * 3_600 + 5), "10 h left");
        assert_eq!(cert_left_phrase(90 * 60), "1 h left");
        assert_eq!(cert_left_phrase(30 * 86_400), "30 d left");
        assert_eq!(cert_left_phrase(-5 * 3_600), "EXPIRED 5 h ago");
        assert_eq!(cert_left_phrase(-3 * 86_400), "EXPIRED 3 d ago");
    }

    #[test]
    fn infer_probe_classifies_targets_and_normalizes() {
        // URLs are HTTP, scheme wins over anything that follows, kept as-is.
        assert_eq!(
            infer_probe("https://example.com"),
            (Kind::Http, "https://example.com".to_owned())
        );
        assert_eq!(
            infer_probe("http://example.com:8080/health"),
            (Kind::Http, "http://example.com:8080/health".to_owned())
        );
        // host:port (and bracketed ipv6:port) is a TCP connect.
        assert_eq!(
            infer_probe("db.example.com:5432"),
            (Kind::Tcp, "db.example.com:5432".to_owned())
        );
        assert_eq!(
            infer_probe("[2001:db8::1]:443"),
            (Kind::Tcp, "[2001:db8::1]:443".to_owned())
        );
        // A bare IP is a ping target - including unbracketed IPv6.
        assert_eq!(
            infer_probe("192.168.1.10"),
            (Kind::Icmp, "192.168.1.10".to_owned())
        );
        assert_eq!(infer_probe("::1"), (Kind::Icmp, "::1".to_owned()));
        // A bare hostname is an HTTPS check, scheme prepended.
        assert_eq!(
            infer_probe("example.com"),
            (Kind::Http, "https://example.com".to_owned())
        );
        // A non-numeric "port" is not host:port: it's a (weird) hostname -> https.
        assert_eq!(
            infer_probe("example.com:http"),
            (Kind::Http, "https://example.com:http".to_owned())
        );
    }

    #[test]
    fn probe_target_prepends_https_only_for_schemeless_http() {
        assert_eq!(
            probe_target(Kind::Http, "example.com"),
            "https://example.com"
        );
        assert_eq!(
            probe_target(Kind::Http, "http://example.com"),
            "http://example.com"
        );
        // Other kinds take the target verbatim.
        assert_eq!(probe_target(Kind::Tcp, "db:5432"), "db:5432");
        assert_eq!(probe_target(Kind::Icmp, "example.com"), "example.com");
    }
}
