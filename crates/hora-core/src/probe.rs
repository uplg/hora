//! Probing logic: turn a monitor into a single [`Outcome`].

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use hickory_resolver::TokioResolver;
use hickory_resolver::config::{ConnectionConfig, NameServerConfig, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::proto::rr::RecordType;
use reqwest::{Client, RequestBuilder};
use socket2::Type;
use surge_ping::{
    Client as PingClient, Config as PingConfig, ICMP, PingIdentifier, PingSequence, SurgeError,
};
use tokio::net::TcpStream;

use crate::config::{Kind, Monitor, Secret};

/// Maximum length (chars) of the response-body snippet kept on failure.
const MAX_BODY_SNIPPET: usize = 300;

/// Bounds of the failure snapshot ("what did the service actually answer?")
/// captured when an HTTP probe fails with a response: at most this many
/// headers, each line clipped, and this many chars of body. ~6 KiB worst case,
/// stored only on confirmed incidents.
const MAX_SNAPSHOT_HEADERS: usize = 24;
const MAX_SNAPSHOT_HEADER_CHARS: usize = 160;
const MAX_SNAPSHOT_BODY_CHARS: usize = 2048;

/// Result of a single probe.
#[derive(Debug)]
pub struct Outcome {
    pub up: bool,
    pub degraded: bool,
    pub latency_ms: Option<i64>,
    pub status_code: Option<i64>,
    pub error: Option<String>,
    /// The failing HTTP response (status line, headers, start of the body),
    /// bounded; only set when the probe got a response back. Stored on the
    /// incident when the down is confirmed.
    pub snapshot: Option<String>,
}

impl Outcome {
    /// Numeric status stored in the database: 0 = down, 1 = up, 2 = degraded.
    #[must_use]
    pub fn status_value(&self) -> i64 {
        if !self.up {
            0
        } else if self.degraded {
            2
        } else {
            1
        }
    }

    pub(crate) fn down(error: String) -> Self {
        Self {
            up: false,
            degraded: false,
            latency_ms: None,
            status_code: None,
            error: Some(error),
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
#[must_use]
pub async fn run(client: &Client, monitor: &Monitor) -> Outcome {
    let mut outcome = probe_once(client, monitor).await;
    for attempt in 1..=monitor.probe_retries() {
        if outcome.up {
            break;
        }
        tracing::info!(
            monitor = %monitor.id,
            attempt,
            error = outcome.error.as_deref().unwrap_or("unknown"),
            "probe failed, retrying"
        );
        tokio::time::sleep(RETRY_DELAY).await;
        outcome = probe_once(client, monitor).await;
    }
    outcome
}

async fn probe_once(client: &Client, monitor: &Monitor) -> Outcome {
    if monitor.dual_stack() {
        return dual_stack(monitor).await;
    }
    match monitor.kind {
        Kind::Http => http(client, monitor).await,
        Kind::Tcp => tcp(monitor).await,
        Kind::Icmp => icmp(monitor).await,
        Kind::Dns => dns(monitor).await,
        // Push monitors are evaluated from stored heartbeats by the scheduler,
        // never actively probed; this arm is unreachable in practice.
        Kind::Push => Outcome::down("push monitor has no active probe".to_owned()),
    }
}

/// One IP address family of a dual-stack probe.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    V4,
    V6,
}

impl Family {
    fn label(self) -> &'static str {
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

    fn matches(self, ip: IpAddr) -> bool {
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
async fn dual_stack(monitor: &Monitor) -> Outcome {
    let (v4, v6) = tokio::join!(
        probe_family(monitor, Family::V4),
        probe_family(monitor, Family::V6)
    );
    combine(&v4, &v6)
}

async fn probe_family(monitor: &Monitor, family: Family) -> Outcome {
    match monitor.kind {
        // The per-monitor client cannot be steered per family, so each probe
        // builds its own family-bound one; negligible at probing cadence.
        Kind::Http => match crate::http::probe_client_family(family.unspecified()) {
            Ok(client) => http(&client, monitor).await,
            Err(err) => Outcome::down(format!("could not build probe client: {err}")),
        },
        Kind::Tcp => tcp_family(monitor, family).await,
        Kind::Icmp => icmp_family(monitor, Some(family)).await,
        // Config validation restricts dual_stack to the three kinds above.
        Kind::Dns | Kind::Push => {
            Outcome::down("dual_stack unsupported for this monitor kind".to_owned())
        }
    }
}

/// Merge the two per-family outcomes of a dual-stack probe. Both up → up, with
/// the *worst* latency (so `degraded_over_ms` judges the slower path). One
/// family down → down: the dual-stack contract is broken even though some
/// clients still reach the service; the reason names the failing family and
/// the latency reflects the surviving path. Both down → down with both reasons.
fn combine(v4: &Outcome, v6: &Outcome) -> Outcome {
    match (v4.up, v6.up) {
        (true, true) => Outcome {
            up: true,
            degraded: v4.degraded || v6.degraded,
            latency_ms: match (v4.latency_ms, v6.latency_ms) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            },
            status_code: v4.status_code.or(v6.status_code),
            error: None,
            snapshot: None,
        },
        (false, true) => one_family_down(Family::V4, v4, v6),
        (true, false) => one_family_down(Family::V6, v6, v4),
        (false, false) => Outcome {
            up: false,
            degraded: false,
            latency_ms: None,
            status_code: v4.status_code.or(v6.status_code),
            error: Some(format!(
                "IPv4 and IPv6 failing: {}; {}",
                v4.error.as_deref().unwrap_or("unknown error"),
                v6.error.as_deref().unwrap_or("unknown error")
            )),
            snapshot: v4.snapshot.clone().or_else(|| v6.snapshot.clone()),
        },
    }
}

/// The down outcome for a single broken family: the failure's detail and
/// status code, the healthy family's latency.
fn one_family_down(failed: Family, failure: &Outcome, healthy: &Outcome) -> Outcome {
    Outcome {
        up: false,
        degraded: false,
        latency_ms: healthy.latency_ms,
        status_code: failure.status_code,
        error: Some(format!(
            "{} failing: {} ({} ok)",
            failed.label(),
            failure.error.as_deref().unwrap_or("unknown error"),
            failed.other().label()
        )),
        snapshot: failure.snapshot.clone(),
    }
}

async fn http(client: &Client, monitor: &Monitor) -> Outcome {
    let start = Instant::now();
    // One deadline for the whole request - every redirect hop and the body read -
    // so a chain of slow redirects can't outlive the monitor's timeout. Latency is
    // taken when the final response's headers arrive, before the body read, to
    // match the single-request timing this replaced.
    let attempt = async {
        let response = send_following_redirects(client, monitor).await?;
        let latency = millis(start.elapsed());
        let code = response.status().as_u16();
        let status_ok = match monitor.expected_status {
            Some(expected) => code == expected,
            None => response.status().is_success(),
        };
        // Read the body only when we need it: to detail a failure, or to run
        // a keyword/JSON assertion. Assertions get a larger budget. The head
        // (status line + headers) is captured first - reading the body
        // consumes the response - in case this turns into a failure snapshot.
        let assertions = monitor.keyword.is_some() || monitor.json_query.is_some();
        let (head, body) = if !status_ok || assertions {
            let head = snapshot_head(&response);
            let cap = if assertions {
                monitor.assertion_body_cap()
            } else {
                MAX_SNAPSHOT_BODY_CHARS
            };
            (head, read_body(response, cap).await)
        } else {
            (String::new(), Vec::new())
        };
        Ok::<_, HttpError>((code, status_ok, head, body, latency))
    };

    match tokio::time::timeout(monitor.timeout(), attempt).await {
        Ok(Ok((code, status_ok, head, body, latency))) => {
            let (up, error) = if !status_ok {
                let snippet = snippet(&body);
                let detail = if snippet.is_empty() {
                    format!("HTTP {code}")
                } else {
                    format!("HTTP {code}: {snippet}")
                };
                (false, Some(detail))
            } else if let Some(failure) = check_assertions(monitor, &body) {
                (false, Some(failure))
            } else {
                (true, None)
            };

            let degraded = up && over_threshold(latency, monitor.degraded_over_ms);
            Outcome {
                up,
                degraded,
                latency_ms: Some(latency),
                status_code: Some(i64::from(code)),
                error,
                // The service answered something and the check failed: keep
                // what it answered for the incident record.
                snapshot: (!up).then(|| render_snapshot(&head, &body)),
            }
        }
        Ok(Err(HttpError::TooManyRedirects)) => Outcome::down("too many redirects".to_owned()),
        Ok(Err(HttpError::Request(err))) => Outcome::down(describe(&err).to_owned()),
        Err(_elapsed) => Outcome::down("request timed out".to_owned()),
    }
}

/// reqwest's default redirect ceiling; we follow them ourselves (below) and keep
/// the same bound.
const MAX_REDIRECTS: usize = 10;

/// Either a transport error or our own redirect-budget exhaustion. The probe
/// client never auto-follows, so reqwest can't produce a "too many redirects"
/// error of its own.
enum HttpError {
    Request(reqwest::Error),
    TooManyRedirects,
}

/// Send the monitor's GET, following redirects manually so the configured
/// headers - which may carry credentials (an API key, a bearer token) - are
/// re-attached only while the redirect stays on the original origin. reqwest
/// strips its own well-known sensitive headers across hosts but not arbitrary
/// custom ones, so without this a malicious or compromised target could 30x us
/// to an attacker host and harvest the header. Cross-origin hops are still
/// followed, just without the headers.
async fn send_following_redirects(
    client: &Client,
    monitor: &Monitor,
) -> Result<reqwest::Response, HttpError> {
    let Ok(target) = reqwest::Url::parse(&monitor.target) else {
        // Config validation rejects non-URL http targets; degrade gracefully by
        // letting reqwest surface the error on send.
        return client
            .get(&monitor.target)
            .timeout(monitor.timeout())
            .send()
            .await
            .map_err(HttpError::Request);
    };
    let mut url = target.clone();
    for _ in 0..=MAX_REDIRECTS {
        // The per-request timeout overrides the client's 15s notifier backstop,
        // which may be *shorter* than the monitor's own; the caller's outer
        // deadline still bounds the whole chain.
        let mut request = client.get(url.clone()).timeout(monitor.timeout());
        if same_origin(&target, &url) {
            request = with_headers(request, &monitor.headers);
        }
        let response = request.send().await.map_err(HttpError::Request)?;
        if !response.status().is_redirection() {
            return Ok(response);
        }
        // A 3xx without a usable Location (or 304 Not Modified) is the final
        // response; hand it back rather than chase nothing.
        let next = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|location| url.join(location).ok());
        match next {
            Some(next) => url = next,
            None => return Ok(response),
        }
    }
    Err(HttpError::TooManyRedirects)
}

/// Whether `url` shares an origin (scheme + host + port) with the original
/// `target` - the rule that decides whether the monitor's (possibly
/// credential-bearing) headers are re-attached across a redirect.
fn same_origin(target: &reqwest::Url, url: &reqwest::Url) -> bool {
    target.origin() == url.origin()
}

/// Run the configured keyword/JSON assertions against the body; the first that
/// fails returns its reason. `None` means every assertion passed.
fn check_assertions(monitor: &Monitor, body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    if let Some(keyword) = &monitor.keyword {
        let found = text.contains(keyword.as_str());
        if found == monitor.keyword_invert {
            return Some(if monitor.keyword_invert {
                format!("keyword present: {keyword}")
            } else {
                format!("keyword missing: {keyword}")
            });
        }
    }
    if let Some(query) = &monitor.json_query {
        return check_json(query, monitor.json_expected.as_deref(), &text);
    }
    None
}

/// Evaluate a `JSONPath` against the body. Returns a failure reason or `None`.
fn check_json(query: &str, expected: Option<&str>, body: &str) -> Option<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Some("response is not valid JSON".to_owned());
    };
    // The query is validated at config load, so this should not fail.
    let Ok(path) = serde_json_path::JsonPath::parse(query) else {
        return Some(format!("invalid JSON query: {query}"));
    };
    let nodes = path.query(&value).all();
    match expected {
        None => nodes
            .is_empty()
            .then(|| format!("JSON query matched nothing: {query}")),
        Some(expected) => {
            let matched = nodes.iter().any(|node| json_value_eq(node, expected));
            (!matched).then(|| format!("JSON query {query} != {expected}"))
        }
    }
}

/// Compare a queried JSON node to an expected string: strings match their inner
/// value, everything else matches its compact JSON text (`true`, `42`, …).
fn json_value_eq(value: &serde_json::Value, expected: &str) -> bool {
    match value {
        serde_json::Value::String(text) => text == expected,
        other => {
            let rendered = other.to_string();
            rendered == expected
        }
    }
}

async fn tcp(monitor: &Monitor) -> Outcome {
    tcp_connect(monitor, monitor.target.as_str()).await
}

/// TCP for one address family: resolve the `host:port` target ourselves and
/// connect to the first address of that family.
async fn tcp_family(monitor: &Monitor, family: Family) -> Outcome {
    let Ok(addrs) = tokio::net::lookup_host(&monitor.target).await else {
        return Outcome::down("could not resolve host".to_owned());
    };
    match addrs.into_iter().find(|addr| family.matches(addr.ip())) {
        Some(addr) => tcp_connect(monitor, addr).await,
        None => Outcome::down(format!("no {} address for host", family.label())),
    }
}

async fn tcp_connect<A: tokio::net::ToSocketAddrs>(monitor: &Monitor, addr: A) -> Outcome {
    let start = Instant::now();
    match tokio::time::timeout(monitor.timeout(), TcpStream::connect(addr)).await {
        Ok(Ok(_stream)) => {
            let latency = millis(start.elapsed());
            Outcome {
                up: true,
                degraded: over_threshold(latency, monitor.degraded_over_ms),
                latency_ms: Some(latency),
                status_code: None,
                error: None,
                snapshot: None,
            }
        }
        Ok(Err(err)) => Outcome::down(err.to_string()),
        Err(_elapsed) => Outcome::down("connection timed out".to_owned()),
    }
}

/// ICMP echo (ping). Uses a per-probe unprivileged datagram socket (no
/// `CAP_NET_RAW`), so it works in rootless Docker; one socket per probe avoids
/// datagram identifier collisions between concurrent monitors. The address family
/// (IPv4/IPv6) follows the resolved address.
async fn icmp(monitor: &Monitor) -> Outcome {
    icmp_family(monitor, None).await
}

/// ICMP to the first resolved address - of one specific family when given.
async fn icmp_family(monitor: &Monitor, family: Option<Family>) -> Outcome {
    let Some(addr) = resolve(&monitor.target, family).await else {
        return Outcome::down(match family {
            Some(family) => format!("no {} address for host", family.label()),
            None => "could not resolve host".to_owned(),
        });
    };

    let kind = if addr.is_ipv4() { ICMP::V4 } else { ICMP::V6 };
    let config = PingConfig::builder()
        .kind(kind)
        .sock_type_hint(Type::DGRAM)
        .build();
    let client = match PingClient::new(&config) {
        Ok(client) => client,
        // Usually a missing privilege: no unprivileged-ping permission and no
        // CAP_NET_RAW. Surface it clearly rather than as a generic failure.
        Err(err) => {
            return Outcome::down(format!(
                "icmp socket unavailable ({err}); needs net.ipv4.ping_group_range or CAP_NET_RAW"
            ));
        }
    };

    let mut pinger = client.pinger(addr, PingIdentifier(0)).await;
    pinger.timeout(monitor.timeout());
    match pinger.ping(PingSequence(0), &[0u8; 16]).await {
        Ok((_packet, rtt)) => {
            let latency = millis(rtt);
            Outcome {
                up: true,
                degraded: over_threshold(latency, monitor.degraded_over_ms),
                latency_ms: Some(latency),
                status_code: None,
                error: None,
                snapshot: None,
            }
        }
        Err(SurgeError::Timeout { .. }) => Outcome::down("request timed out".to_owned()),
        Err(err) => Outcome::down(format!("icmp error: {err}")),
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

/// DNS resolution: resolve a name and, when `dns_expected` is set, pin the
/// answer (hijack detection). Without it any non-empty answer counts as up -
/// answers rotate freely behind CDNs and round-robin records, so alerting on
/// mere change would flap.
async fn dns(monitor: &Monitor) -> Outcome {
    let record_type = monitor
        .dns_record
        .as_deref()
        .map_or(RecordType::A, |record| {
            match record.to_uppercase().as_str() {
                "AAAA" => RecordType::AAAA,
                "CNAME" => RecordType::CNAME,
                "MX" => RecordType::MX,
                "NS" => RecordType::NS,
                "TXT" => RecordType::TXT,
                "SRV" => RecordType::SRV,
                "SOA" => RecordType::SOA,
                "PTR" => RecordType::PTR,
                // "A", plus anything else config validation already rejected.
                _ => RecordType::A,
            }
        });

    let resolver = match resolver_for(monitor.dns_resolver.as_deref()) {
        Ok(resolver) => resolver,
        Err(err) => return Outcome::down(format!("resolver setup failed: {err}")),
    };

    let start = Instant::now();
    let result = tokio::time::timeout(
        monitor.timeout(),
        resolver.lookup(monitor.target.as_str(), record_type),
    )
    .await;
    let latency = millis(start.elapsed());

    match result {
        Ok(Ok(lookup)) => {
            // The answer section may also carry the CNAME chain; keep only the
            // requested type so assertions compare like with like.
            let mut answers: Vec<String> = lookup
                .answers()
                .iter()
                .filter(|record| record.record_type() == record_type)
                .map(|record| record.data.to_string().trim_end_matches('.').to_owned())
                .collect();
            if answers.is_empty() {
                return Outcome::down(format!("no {record_type} records found"));
            }
            answers.sort();
            let answer = answers.join(",");

            if let Some(expected) = &monitor.dns_expected {
                // Both sides sorted: the assertion is order-insensitive, so
                // rotation within a pinned record set never flaps.
                let mut wanted: Vec<&str> = expected.split(',').map(str::trim).collect();
                wanted.sort_unstable();
                let wanted = wanted.join(",");
                if answer != wanted {
                    // The full (but still bounded) answer goes into the failure
                    // snapshot - "what did it actually answer?" applies to DNS
                    // pins too, and TXT answers rarely fit the inline reason.
                    let snapshot: String = format!("DNS answer: {answer}")
                        .chars()
                        .take(MAX_SNAPSHOT_BODY_CHARS)
                        .collect();
                    // The answer is remote-controlled (TXT records run to tens of
                    // KB and may span lines); snippet() bounds and single-lines it
                    // exactly like an HTTP failure body.
                    let answer = snippet(answer.as_bytes());
                    return Outcome {
                        up: false,
                        degraded: false,
                        latency_ms: Some(latency),
                        status_code: None,
                        error: Some(format!("expected {wanted}, got {answer}")),
                        snapshot: Some(snapshot),
                    };
                }
            }

            Outcome {
                up: true,
                degraded: over_threshold(latency, monitor.degraded_over_ms),
                latency_ms: Some(latency),
                status_code: None,
                error: None,
                snapshot: None,
            }
        }
        Ok(Err(err)) => Outcome::down(format!("DNS lookup failed: {err}")),
        Err(_elapsed) => Outcome::down("DNS lookup timed out".to_owned()),
    }
}

/// The system resolver, or a custom `host:port` UDP resolver when configured.
fn resolver_for(custom: Option<&str>) -> anyhow::Result<TokioResolver> {
    let Some(addr) = custom else {
        return Ok(TokioResolver::builder_tokio()?.build()?);
    };
    let (host, port) = addr
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("dns_resolver must be host:port"))?;
    let port: u16 = port.parse()?;
    let ip: IpAddr = host.parse()?;
    let mut connection = ConnectionConfig::udp();
    connection.port = port;
    let nameserver = NameServerConfig::new(ip, true, vec![connection]);
    let config = ResolverConfig::from_parts(None, vec![], vec![nameserver]);
    Ok(TokioResolver::builder_with_config(config, TokioRuntimeProvider::default()).build()?)
}

/// Apply every configured header to the request. reqwest *appends* headers, so
/// each distinct header is kept - none overwrites another.
fn with_headers(mut request: RequestBuilder, headers: &HashMap<String, Secret>) -> RequestBuilder {
    for (name, value) in headers {
        request = request.header(name, value.as_ref());
    }
    request
}

fn over_threshold(latency_ms: i64, threshold: Option<i64>) -> bool {
    threshold.is_some_and(|limit| latency_ms > limit)
}

fn millis(elapsed: Duration) -> i64 {
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

/// Read the response body up to `cap` bytes (so a huge body can't exhaust memory).
async fn read_body(mut response: reqwest::Response, cap: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    while buf.len() < cap {
        match response.chunk().await {
            // Copy at most the remaining budget so one huge chunk can't blow the bound.
            Ok(Some(chunk)) => {
                let take = (cap - buf.len()).min(chunk.len());
                buf.extend_from_slice(&chunk[..take]);
            }
            _ => break,
        }
    }
    buf
}

/// The status line and (bounded) headers of a response, captured before the
/// body read consumes it.
fn snapshot_head(response: &reqwest::Response) -> String {
    render_head(response.version(), response.status(), response.headers())
}

/// Format the status line and headers of the failure snapshot. Header values
/// are clipped per line and the count is capped, so a hostile response can't
/// bloat the incident record.
fn render_head(
    version: reqwest::Version,
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
) -> String {
    let mut head = format!("{version:?} {status}");
    for (name, value) in headers.iter().take(MAX_SNAPSHOT_HEADERS) {
        let value = value.to_str().unwrap_or("<binary>");
        head.push('\n');
        head.extend(
            format!("{name}: {value}")
                .chars()
                .take(MAX_SNAPSHOT_HEADER_CHARS),
        );
    }
    let dropped = headers.len().saturating_sub(MAX_SNAPSHOT_HEADERS);
    if dropped > 0 {
        let _ = std::fmt::Write::write_fmt(&mut head, format_args!("\n({dropped} more headers)"));
    }
    head
}

/// Assemble the stored failure snapshot: status line and headers, a blank
/// line, then the start of the body (lossy UTF-8, bounded in chars).
fn render_snapshot(head: &str, body: &[u8]) -> String {
    let text: String = String::from_utf8_lossy(body)
        .chars()
        .take(MAX_SNAPSHOT_BODY_CHARS)
        .collect();
    if text.trim().is_empty() {
        head.to_owned()
    } else {
        format!("{head}\n\n{text}")
    }
}

/// Collapse a byte body into a bounded, single-line snippet for failure detail.
fn snippet(body: &[u8]) -> String {
    // Fold the whitespace-separated words straight into one string, so there is
    // no intermediate `Vec<&str>` just to `join` it.
    String::from_utf8_lossy(body)
        .split_whitespace()
        .fold(String::new(), |mut acc, word| {
            if !acc.is_empty() {
                acc.push(' ');
            }
            acc.push_str(word);
            acc
        })
        .chars()
        .take(MAX_BODY_SNIPPET)
        .collect()
}

/// The public form of a stored failure reason. Stored reasons keep full
/// operator detail - response-body snippets, DNS answers, asserted keywords,
/// raw socket errors - which anonymous viewers of a public monitor must not
/// see: snippets and DNS answers are remote-controlled, keywords and JSON
/// queries reveal operator config. Known-safe reasons pass through verbatim;
/// detailed ones collapse to their category; anything unrecognized (raw TCP
/// errors, free-form push messages, legacy rows) falls back to "check failed".
///
/// Must track the reasons produced in this file and the scheduler's heartbeat
/// misses; an unmatched new reason degrades safely to the fallback.
#[must_use]
pub fn public_reason(reason: &str) -> &str {
    // Static reasons carry no detail; they are their own category.
    const VERBATIM: &[&str] = &[
        "request timed out",
        "connection failed",
        "connection timed out",
        "too many redirects",
        "invalid response body",
        "request error",
        "could not resolve host",
        "DNS lookup timed out",
        "missing heartbeat",
    ];
    // "HTTP 500: <body snippet>" keeps the status, drops the snippet. Anchored
    // to a numeric status: `checks.error` also stores free-form push messages,
    // so a bare "HTTP <anything>" prefix must not become a passthrough that
    // lets a push-token holder place arbitrary text on the public page.
    if let Some(rest) = reason.strip_prefix("HTTP ") {
        let code = rest.find(':').map_or(rest, |colon| &rest[..colon]);
        if code.parse::<u16>().is_ok() {
            return &reason[..5 + code.len()];
        }
    }
    if VERBATIM.contains(&reason) {
        return reason;
    }
    // Same anchoring: only the record types the DNS probe actually queries.
    if let Some(record) = reason
        .strip_prefix("no ")
        .and_then(|rest| rest.strip_suffix(" records found"))
        && ["A", "AAAA", "CNAME", "MX", "NS", "TXT", "SRV", "SOA", "PTR"].contains(&record)
    {
        return reason;
    }
    // "missed scheduled heartbeat (was due 03:00 UTC + 30m grace)" reveals the
    // job's schedule; keep the category only.
    if reason.starts_with("missed scheduled heartbeat") {
        return "missed scheduled heartbeat";
    }
    // Keyword and JSON assertions embed the configured keyword/query.
    if reason.starts_with("keyword ")
        || reason.starts_with("JSON query")
        || reason.starts_with("invalid JSON query")
        || reason == "response is not valid JSON"
    {
        return "content check failed";
    }
    // The DNS pin mismatch embeds the remote-controlled answer.
    if reason.starts_with("expected ") {
        return "unexpected DNS answer";
    }
    if reason.starts_with("DNS lookup failed") || reason.starts_with("resolver setup failed") {
        return "DNS lookup failed";
    }
    if reason.starts_with("icmp") {
        return "ping failed";
    }
    // Dual-stack failures embed the failing family's detail (which may be a raw
    // socket error); keep only which family broke. Checked longest-prefix first.
    if reason.starts_with("IPv4 and IPv6 failing") {
        return "IPv4 and IPv6 failing";
    }
    if reason.starts_with("IPv4 failing") {
        return "IPv4 failing (IPv6 ok)";
    }
    if reason.starts_with("IPv6 failing") {
        return "IPv6 failing (IPv4 ok)";
    }
    "check failed"
}

/// A concise, URL-free description of a request error. The raw error embeds the
/// target URL (which may carry credentials), so we categorize instead.
fn describe(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "request timed out"
    } else if err.is_connect() {
        "connection failed"
    } else if err.is_redirect() {
        "too many redirects"
    } else if err.is_body() || err.is_decode() {
        "invalid response body"
    } else {
        "request error"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_reason_collapses_detail() {
        // Remote-controlled or config-revealing detail is dropped.
        assert_eq!(public_reason("HTTP 500: secret stack trace"), "HTTP 500");
        assert_eq!(
            public_reason("expected 1.2.3.4, got 6.6.6.6"),
            "unexpected DNS answer"
        );
        assert_eq!(
            public_reason("keyword missing: internal-marker"),
            "content check failed"
        );
        assert_eq!(
            public_reason("JSON query $.status != ok"),
            "content check failed"
        );
        assert_eq!(
            public_reason("missed scheduled heartbeat (was due 03:00 UTC + 30m grace)"),
            "missed scheduled heartbeat"
        );
        assert_eq!(
            public_reason("DNS lookup failed: proto error: io error"),
            "DNS lookup failed"
        );
        // Unknown text (raw TCP errors, push messages) falls back to generic.
        assert_eq!(
            public_reason("Connection refused (os error 61)"),
            "check failed"
        );
        // Safe statics pass through verbatim.
        assert_eq!(public_reason("HTTP 503"), "HTTP 503");
        assert_eq!(public_reason("request timed out"), "request timed out");
        assert_eq!(public_reason("no A records found"), "no A records found");
        // A push msg crafted to look like a safe shape must not pass through:
        // the HTTP branch is anchored to a numeric status, the DNS one to the
        // record types the probe queries.
        assert_eq!(
            public_reason("HTTP looks legit but is a push msg"),
            "check failed"
        );
        assert_eq!(public_reason("HTTP 99999: nope"), "check failed");
        assert_eq!(
            public_reason("no big deal, just injected records found"),
            "check failed"
        );
    }

    #[test]
    fn same_origin_governs_header_forwarding() {
        let parse = |u: &str| reqwest::Url::parse(u).unwrap();
        let target = parse("https://api.example.com/v1");

        // Same host, scheme and port (path differs): headers stay attached.
        assert!(same_origin(
            &target,
            &parse("https://api.example.com/login")
        ));
        // Different host, scheme or port: headers are dropped.
        assert!(!same_origin(&target, &parse("https://evil.example.com/v1")));
        assert!(!same_origin(&target, &parse("http://api.example.com/v1")));
        assert!(!same_origin(
            &target,
            &parse("https://api.example.com:8443/v1")
        ));
    }

    #[test]
    fn threshold_detects_slow() {
        assert!(over_threshold(900, Some(800)));
        assert!(!over_threshold(700, Some(800)));
        assert!(!over_threshold(900, None));
    }

    #[test]
    fn millis_saturates() {
        assert_eq!(millis(Duration::from_millis(5)), 5);
    }

    #[test]
    fn status_value_mapping() {
        let down = Outcome::down("x".to_owned());
        assert_eq!(down.status_value(), 0);
    }

    fn http_monitor() -> Monitor {
        Monitor {
            id: "m".to_owned(),
            name: "M".to_owned(),
            kind: Kind::Http,
            target: "https://example.com".to_owned(),
            interval_secs: 60,
            timeout_secs: 10,
            expected_status: None,
            degraded_over_ms: None,
            slo_latency_ms: None,
            headers: HashMap::new(),
            keyword: None,
            keyword_invert: false,
            json_query: None,
            json_expected: None,
            max_body_kb: None,
            probe_retries: None,
            notify: None,
            proxy: None,
            push_token: None,
            check_cert: None,
            retention_days: None,
            group: None,
            depends_on: None,
            public: true,
            public_error_detail: false,
            dual_stack: None,
            dns_record: None,
            dns_expected: None,
            dns_resolver: None,
            cert_pin: None,
            domain_expiry: None,
            slo_uptime: None,
            slo_window_days: None,
            schedule: None,
            grace_secs: None,
        }
    }

    #[test]
    fn keyword_assertion() {
        let mut monitor = http_monitor();
        monitor.keyword = Some("OK".to_owned());
        assert!(check_assertions(&monitor, b"all OK here").is_none());
        assert!(check_assertions(&monitor, b"failure").is_some());

        monitor.keyword_invert = true;
        assert!(check_assertions(&monitor, b"failure").is_none());
        assert!(check_assertions(&monitor, b"all OK").is_some());
    }

    #[test]
    fn json_query_assertion() {
        // Expected value, string and non-string.
        assert!(check_json("$.status", Some("ok"), r#"{"status":"ok"}"#).is_none());
        assert!(check_json("$.status", Some("ok"), r#"{"status":"bad"}"#).is_some());
        assert!(check_json("$.healthy", Some("true"), r#"{"healthy":true}"#).is_none());
        // No expected value: the query just has to match something.
        assert!(check_json("$.data", None, r#"{"data":[1,2]}"#).is_none());
        assert!(check_json("$.missing", None, r#"{"data":1}"#).is_some());
        // Malformed JSON fails the assertion.
        assert!(check_json("$.x", Some("1"), "not json").is_some());
    }

    #[tokio::test]
    async fn applies_every_configured_header() {
        let client = Client::new();
        let mut headers = HashMap::new();
        headers.insert("Accept".to_owned(), Secret("text/html".to_owned()));
        headers.insert("X-Token".to_owned(), Secret("abc".to_owned()));

        let request = with_headers(client.get("https://example.com"), &headers);
        let built = request.build().expect("request builds");

        // Both headers survive: appending never overwrites the previous one.
        assert_eq!(built.headers().len(), 2);
        assert_eq!(
            built.headers().get("accept").unwrap().to_str().unwrap(),
            "text/html"
        );
        assert_eq!(
            built.headers().get("x-token").unwrap().to_str().unwrap(),
            "abc"
        );
    }

    #[tokio::test]
    async fn resolve_parses_ip_literals() {
        // IP literals short-circuit before any DNS lookup (no network in tests).
        assert_eq!(
            resolve("127.0.0.1", None).await,
            Some("127.0.0.1".parse().unwrap())
        );
        assert_eq!(
            resolve("2606:4700:4700::1111", None).await,
            Some("2606:4700:4700::1111".parse().unwrap())
        );
        // A family filter rejects a literal of the other family.
        assert_eq!(
            resolve("127.0.0.1", Some(Family::V4)).await,
            Some("127.0.0.1".parse().unwrap())
        );
        assert_eq!(resolve("127.0.0.1", Some(Family::V6)).await, None);
    }

    fn outcome(up: bool, latency_ms: Option<i64>, error: Option<&str>) -> Outcome {
        Outcome {
            up,
            degraded: false,
            latency_ms,
            status_code: None,
            error: error.map(str::to_owned),
            snapshot: None,
        }
    }

    #[test]
    fn combine_requires_both_families() {
        // Both up: up, with the worst latency.
        let both = combine(
            &outcome(true, Some(20), None),
            &outcome(true, Some(35), None),
        );
        assert!(both.up);
        assert_eq!(both.latency_ms, Some(35));
        assert_eq!(both.error, None);

        // One family broken: down, the reason names it, the latency is the
        // surviving path's.
        let v6_dead = combine(
            &outcome(true, Some(20), None),
            &outcome(false, None, Some("connection timed out")),
        );
        assert!(!v6_dead.up);
        assert_eq!(v6_dead.latency_ms, Some(20));
        assert_eq!(
            v6_dead.error.as_deref(),
            Some("IPv6 failing: connection timed out (IPv4 ok)")
        );

        let v4_dead = combine(
            &outcome(false, None, Some("connection refused")),
            &outcome(true, Some(12), None),
        );
        assert!(!v4_dead.up);
        assert_eq!(
            v4_dead.error.as_deref(),
            Some("IPv4 failing: connection refused (IPv6 ok)")
        );

        // Both down: both reasons, no latency.
        let dark = combine(
            &outcome(false, None, Some("timeout")),
            &outcome(false, None, Some("refused")),
        );
        assert!(!dark.up);
        assert_eq!(dark.latency_ms, None);
        assert_eq!(
            dark.error.as_deref(),
            Some("IPv4 and IPv6 failing: timeout; refused")
        );
    }

    #[test]
    fn combine_keeps_either_degraded() {
        let mut slow_v6 = outcome(true, Some(900), None);
        slow_v6.degraded = true;
        let both = combine(&outcome(true, Some(20), None), &slow_v6);
        assert!(both.up);
        assert!(both.degraded);
    }

    #[test]
    fn snapshot_renders_status_headers_and_body_bounded() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("content-type", "text/html".parse().unwrap());
        headers.insert("retry-after", "120".parse().unwrap());

        let head = render_head(
            reqwest::Version::HTTP_2,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            &headers,
        );
        assert_eq!(
            head,
            "HTTP/2.0 503 Service Unavailable\ncontent-type: text/html\nretry-after: 120"
        );

        // Body appended after a blank line; an empty body leaves the head alone.
        let full = render_snapshot(&head, b"<html>maintenance</html>");
        assert_eq!(full, format!("{head}\n\n<html>maintenance</html>"));
        assert_eq!(render_snapshot(&head, b"  \n "), head);

        // A hostile response can't bloat the record: the body is clipped to
        // the cap, oversized header values per line, excess headers counted.
        let huge = vec![b'x'; 100_000];
        let clipped = render_snapshot(&head, &huge);
        assert_eq!(
            clipped.len(),
            head.len() + 2 + MAX_SNAPSHOT_BODY_CHARS,
            "body bounded"
        );
        let mut many = reqwest::header::HeaderMap::new();
        for i in 0..30 {
            many.append(
                "x-filler",
                format!("{i}-{}", "v".repeat(500)).parse().unwrap(),
            );
        }
        let head = render_head(
            reqwest::Version::HTTP_11,
            reqwest::StatusCode::BAD_GATEWAY,
            &many,
        );
        let lines: Vec<&str> = head.lines().collect();
        assert_eq!(lines.len(), 1 + MAX_SNAPSHOT_HEADERS + 1);
        assert_eq!(lines[lines.len() - 1], "(6 more headers)");
        assert!(lines[1].len() <= MAX_SNAPSHOT_HEADER_CHARS);
    }

    #[test]
    fn public_reason_collapses_dual_stack_detail() {
        assert_eq!(
            public_reason("IPv6 failing: Connection refused (os error 61) (IPv4 ok)"),
            "IPv6 failing (IPv4 ok)"
        );
        assert_eq!(
            public_reason("IPv4 failing: no IPv4 address for host (IPv6 ok)"),
            "IPv4 failing (IPv6 ok)"
        );
        assert_eq!(
            public_reason("IPv4 and IPv6 failing: timeout; refused"),
            "IPv4 and IPv6 failing"
        );
    }
}
