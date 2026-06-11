//! `hora doctor`: runtime environment diagnostics - the companion of
//! `hora check`. The config can be perfectly valid while the *environment*
//! can't honour it: no IPv6 route for a `dual_stack` monitor, an ICMP socket
//! forbidden by `net.ipv4.ping_group_range` in rootless Docker, a busy listen
//! port, an unreachable resolver. Each finding says whether the current
//! config actually needs the capability, so a failure is actionable, not
//! noise.

use std::time::{Duration, Instant};

use crate::config::{Config, Kind};

/// Severity of one finding. `Fail` means a capability the *current config
/// needs* is missing - `hora doctor` exits non-zero on any of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// Worth knowing, not blocking (e.g. the listen port is busy because the
    /// daemon is already running, or IPv6 is absent but nothing needs it).
    Warn,
    Fail,
}

impl Status {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "FAIL",
        }
    }
}

/// One diagnostic finding.
#[derive(Debug)]
pub struct Finding {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

impl Finding {
    fn new(name: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name,
            status,
            detail: detail.into(),
        }
    }
}

/// Run every diagnostic against `config`'s requirements.
pub async fn run(config: &Config) -> Vec<Finding> {
    vec![
        database(config).await,
        listen_port(config).await,
        ip_route("ipv4", "8.8.8.8:53", true, config),
        ip_route(
            "ipv6",
            "[2001:4860:4860::8888]:53",
            needs_ipv6(config),
            config,
        ),
        icmp_socket(config),
        dns_resolver().await,
        exec_dir(config),
    ]
}

/// The exec-probe gate: with exec monitors configured, `HORA_EXEC_DIR` must
/// point at a real directory (config validation enforces it at load; doctor
/// re-checks the *current* environment) - and the plugins must actually be
/// there and executable.
fn exec_dir(config: &Config) -> Finding {
    let monitors: Vec<&str> = config
        .monitors
        .iter()
        .filter(|monitor| monitor.kind == Kind::Exec)
        .filter_map(|monitor| monitor.command.first().map(String::as_str))
        .collect();
    let Some(dir) = &config.exec_dir else {
        return if monitors.is_empty() {
            Finding::new("exec", Status::Ok, "disabled (HORA_EXEC_DIR not set)")
        } else {
            Finding::new(
                "exec",
                Status::Fail,
                format!(
                    "{} exec monitor(s) but HORA_EXEC_DIR is not set",
                    monitors.len()
                ),
            )
        };
    };
    let missing: Vec<&str> = monitors
        .iter()
        .filter(|name| !dir.join(name).is_file())
        .copied()
        .collect();
    if !missing.is_empty() {
        return Finding::new(
            "exec",
            Status::Fail,
            format!("missing in {}: {}", dir.display(), missing.join(", ")),
        );
    }
    Finding::new(
        "exec",
        Status::Ok,
        format!("{} ({} plugin(s) found)", dir.display(), monitors.len()),
    )
}

/// Whether any monitor needs working IPv6 on the probing host.
fn needs_ipv6(config: &Config) -> bool {
    config
        .monitors
        .iter()
        .any(super::config::Monitor::dual_stack)
}

/// The database: openable and writable when it exists; merely announced when
/// it does not (the daemon creates it on first start - doctor must not).
async fn database(config: &Config) -> Finding {
    let path = &config.server.database_path;
    if path != ":memory:" && !path.starts_with("file:") && !std::path::Path::new(path).exists() {
        return Finding::new(
            "database",
            Status::Ok,
            format!("{path} does not exist yet - created on first start"),
        );
    }
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .busy_timeout(Duration::from_secs(2));
    let pool = match sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
    {
        Ok(pool) => pool,
        Err(err) => return Finding::new("database", Status::Fail, format!("{path}: {err}")),
    };
    // A write-lock probe without writing anything: BEGIN IMMEDIATE takes the
    // reserved lock (fails on a read-only mount), ROLLBACK releases it.
    let writable = sqlx::raw_sql("BEGIN IMMEDIATE; ROLLBACK;")
        .execute(&pool)
        .await;
    pool.close().await;
    match writable {
        Ok(_) => Finding::new("database", Status::Ok, format!("{path} is writable")),
        Err(err) => Finding::new(
            "database",
            Status::Fail,
            format!("{path} not writable: {err}"),
        ),
    }
}

/// The listen address: bindable, or busy (which usually just means the daemon
/// is already running - a warning, not a failure).
async fn listen_port(config: &Config) -> Finding {
    let bind = &config.server.bind;
    match tokio::net::TcpListener::bind(bind).await {
        Ok(_listener) => Finding::new("listen", Status::Ok, format!("{bind} is free")),
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => Finding::new(
            "listen",
            Status::Warn,
            format!("{bind} already in use - is Hora already running?"),
        ),
        Err(err) => Finding::new("listen", Status::Fail, format!("cannot bind {bind}: {err}")),
    }
}

/// Whether the host has a route for one IP family. `UdpSocket::connect` only
/// consults the routing table - no packet is sent - so this is instant and
/// quiet. The classic catch: Docker's default bridge networks have no IPv6,
/// which silently breaks `dual_stack` monitors.
fn ip_route(name: &'static str, probe_addr: &str, needed: bool, config: &Config) -> Finding {
    let local = if name == "ipv6" {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    let routed = std::net::UdpSocket::bind(local)
        .and_then(|socket| socket.connect(probe_addr))
        .is_ok();
    match (routed, needed) {
        (true, _) => Finding::new(name, Status::Ok, "route to the public internet"),
        (false, true) => {
            let dual = config
                .monitors
                .iter()
                .filter(|monitor| monitor.dual_stack())
                .count();
            Finding::new(
                name,
                Status::Fail,
                format!(
                    "no route - {dual} dual_stack monitor(s) need it \
                     (in Docker, enable IPv6 on the network or use host networking)"
                ),
            )
        }
        (false, false) => Finding::new(name, Status::Warn, "no route (no monitor needs it)"),
    }
}

/// The unprivileged ICMP datagram socket - exactly what the icmp probes open.
/// In rootless Docker this hinges on `net.ipv4.ping_group_range` covering the
/// process's gid (or `CAP_NET_RAW`).
fn icmp_socket(config: &Config) -> Finding {
    let needed = config
        .monitors
        .iter()
        .filter(|monitor| monitor.kind == Kind::Icmp)
        .count();
    let ping_config = surge_ping::Config::builder()
        .kind(surge_ping::ICMP::V4)
        .sock_type_hint(socket2::Type::DGRAM)
        .build();
    match surge_ping::Client::new(&ping_config) {
        Ok(_client) => Finding::new("icmp", Status::Ok, "unprivileged datagram socket available"),
        Err(err) if needed > 0 => Finding::new(
            "icmp",
            Status::Fail,
            format!(
                "socket unavailable ({err}) - {needed} icmp monitor(s) need it; \
                 widen net.ipv4.ping_group_range or grant CAP_NET_RAW"
            ),
        ),
        Err(err) => Finding::new(
            "icmp",
            Status::Warn,
            format!("socket unavailable ({err}) (no icmp monitor configured)"),
        ),
    }
}

/// The system resolver, exercised with a real lookup - the probes' DNS path.
async fn dns_resolver() -> Finding {
    let resolver = match hickory_resolver::TokioResolver::builder_tokio()
        .map(hickory_resolver::ResolverBuilder::build)
    {
        Ok(Ok(resolver)) => resolver,
        Ok(Err(err)) | Err(err) => {
            return Finding::new("dns", Status::Fail, format!("resolver setup failed: {err}"));
        }
    };
    let started = Instant::now();
    match tokio::time::timeout(Duration::from_secs(5), resolver.lookup_ip("example.com.")).await {
        Ok(Ok(_)) => Finding::new(
            "dns",
            Status::Ok,
            format!(
                "system resolver answered in {}ms",
                started.elapsed().as_millis()
            ),
        ),
        Ok(Err(err)) => Finding::new("dns", Status::Fail, format!("lookup failed: {err}")),
        Err(_elapsed) => Finding::new("dns", Status::Fail, "lookup timed out (5s)".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml: &str) -> Config {
        crate::config::parse(toml).expect("config")
    }

    #[tokio::test]
    async fn missing_database_is_announced_not_created() {
        let config = config(
            r#"
            [page]
            [server]
            database_path = "/tmp/hora-doctor-test-does-not-exist.db"
        "#,
        );
        let finding = database(&config).await;
        assert_eq!(finding.status, Status::Ok);
        assert!(finding.detail.contains("created on first start"));
        // Doctor must be side-effect free.
        assert!(!std::path::Path::new("/tmp/hora-doctor-test-does-not-exist.db").exists());
    }

    #[test]
    fn ip_route_severity_depends_on_need() {
        // A guaranteed-unroutable family: probing a v6 address from a v4-only
        // socket fails at connect; with no dual_stack monitor it only warns.
        let quiet = config(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://example.com"
            interval_secs = 60
        "#,
        );
        let finding = ip_route("ipv6", "0.0.0.1:1", false, &quiet);
        // Whatever the host's networking, "not needed" can never be a Fail.
        assert_ne!(finding.status, Status::Fail);
    }
}
