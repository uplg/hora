//! DNS checks: resolve a record and optionally compare the answer.

use super::FailureKind;
use crate::status::CheckStatus;
use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::Instant;

use hickory_resolver::TokioResolver;
use hickory_resolver::config::{ConnectionConfig, NameServerConfig, ResolverConfig, ResolverOpts};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::net::{DnsError, NetError};
use hickory_resolver::proto::op::ResponseCode;
use hickory_resolver::proto::rr::RecordType;

use super::snapshot::{MAX_SNAPSHOT_BODY_CHARS, snippet};
use super::{Outcome, millis, over_threshold};
use crate::config::{DnsRecord, DnsSpec, Monitor};

/// DNS resolution: resolve a name and, when `dns_expected` is set, pin the
/// answer (hijack detection). Without it any non-empty answer counts as up -
/// answers rotate freely behind CDNs and round-robin records, so alerting on
/// mere change would flap.
pub(super) async fn dns(monitor: &Monitor, spec: &DnsSpec) -> Outcome {
    let record_type = record_type(spec.record);
    let custom = spec.resolver;
    let resolver = match resolver_for(custom) {
        Ok(resolver) => resolver,
        Err(err) => {
            return Outcome::down(
                FailureKind::DnsFailed,
                format!("resolver setup failed: {err}"),
            );
        }
    };

    let start = Instant::now();
    let result = tokio::time::timeout(
        monitor.timeout(),
        resolver.lookup(spec.target.as_str(), record_type),
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
                return Outcome::down(
                    FailureKind::NoRecords,
                    format!("no {record_type} records found"),
                );
            }
            answers.sort();
            let answer = answers.join(",");

            if let Some(expected) = &spec.expected {
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
                        status: CheckStatus::Down,
                        latency_ms: Some(latency),
                        status_code: None,
                        error: Some(format!("expected {wanted}, got {answer}")),
                        reason: Some(FailureKind::DnsMismatch),
                        snapshot: Some(snapshot),
                    };
                }
            }

            Outcome::up(
                over_threshold(latency, monitor.degraded_over_ms),
                Some(latency),
                None,
            )
        }
        Ok(Err(err)) => {
            let (kind, reason) = lookup_failure(&err, record_type);
            Outcome::down(kind, reason)
        }
        Err(_elapsed) => Outcome::down(FailureKind::DnsTimeout, "DNS lookup timed out".to_owned()),
    }
}

/// The reason for a failed lookup. hickory's own `Display` of an empty answer
/// embeds the query's `Debug` form (`no records found for Query { name: ...`),
/// and reads the same for a name that does not exist (NXDOMAIN) as for a name
/// that exists without the asked record type: say which one it was.
fn lookup_failure(err: &NetError, record_type: RecordType) -> (FailureKind, String) {
    match err {
        NetError::Dns(DnsError::NoRecordsFound(no_records)) => {
            if no_records.response_code == ResponseCode::NXDomain {
                (
                    FailureKind::Unresolvable,
                    "domain does not exist (NXDOMAIN)".to_owned(),
                )
            } else {
                (
                    FailureKind::NoRecords,
                    format!("no {record_type} records found"),
                )
            }
        }
        NetError::Dns(DnsError::ResponseCode(code)) => (
            FailureKind::DnsFailed,
            format!(
                "DNS lookup failed: server answered {} (rcode {})",
                code.to_str(),
                u16::from(*code)
            ),
        ),
        NetError::Timeout => (FailureKind::DnsTimeout, "DNS lookup timed out".to_owned()),
        other => (
            FailureKind::DnsFailed,
            format!("DNS lookup failed: {other}"),
        ),
    }
}

pub(super) fn record_type(record: DnsRecord) -> RecordType {
    match record {
        DnsRecord::A => RecordType::A,
        DnsRecord::Aaaa => RecordType::AAAA,
        DnsRecord::Cname => RecordType::CNAME,
        DnsRecord::Mx => RecordType::MX,
        DnsRecord::Ns => RecordType::NS,
        DnsRecord::Txt => RecordType::TXT,
        DnsRecord::Srv => RecordType::SRV,
        DnsRecord::Soa => RecordType::SOA,
        DnsRecord::Ptr => RecordType::PTR,
    }
}

/// The system resolver, or a custom resolver (UDP, then TCP) when configured (its address
/// parsed at config load). A fresh resolver per probe on purpose: a shared one
/// would answer from its cache, and a DNS monitor must ask the server every
/// time - so only the system configuration is shared (see [`system_conf`]).
pub(super) fn resolver_for(custom: Option<SocketAddr>) -> anyhow::Result<TokioResolver> {
    let provider = TokioRuntimeProvider::default();
    let Some(addr) = custom else {
        let (config, options) = system_conf()?;
        let mut builder = TokioResolver::builder_with_config(config, provider);
        *builder.options_mut() = options;
        return Ok(builder.build()?);
    };
    // UDP first, TCP for answers too large for a datagram (a truncated
    // reply - long TXT/DKIM/SPF sets - is retried over TCP, as RFC 7766 requires).
    let mut udp = ConnectionConfig::udp();
    udp.port = addr.port();
    let mut tcp = ConnectionConfig::tcp();
    tcp.port = addr.port();
    let nameserver = NameServerConfig::new(addr.ip(), true, vec![udp, tcp]);
    let config = ResolverConfig::from_parts(None, vec![], vec![nameserver]);
    Ok(TokioResolver::builder_with_config(config, provider).build()?)
}

/// The system resolver configuration (`/etc/resolv.conf` on Unix), read once:
/// reading it is blocking file I/O that has no business running on the async
/// runtime at every DNS probe. A failed read is not cached, so it is retried
/// at the next probe; a change to the file needs a restart to be seen.
fn system_conf() -> anyhow::Result<(ResolverConfig, ResolverOpts)> {
    static SYSTEM: OnceLock<(ResolverConfig, ResolverOpts)> = OnceLock::new();
    if let Some(conf) = SYSTEM.get() {
        return Ok(conf.clone());
    }
    let conf = hickory_resolver::system_conf::read_system_conf()?;
    Ok(SYSTEM.get_or_init(|| conf).clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_resolver::net::NoRecords;
    use hickory_resolver::proto::op::{Message, Query};
    use hickory_resolver::proto::rr::rdata::TXT;
    use hickory_resolver::proto::rr::{Name, RData, Record};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[test]
    fn lookup_failures_read_plainly() {
        let empty = |code| NetError::from(NoRecords::new(Query::default(), code));
        let (kind, reason) = lookup_failure(&empty(ResponseCode::NXDomain), RecordType::A);
        assert_eq!(kind, FailureKind::Unresolvable);
        assert_eq!(reason, "domain does not exist (NXDOMAIN)");
        let (kind, reason) = lookup_failure(&empty(ResponseCode::NoError), RecordType::AAAA);
        assert_eq!(kind, FailureKind::NoRecords);
        assert_eq!(reason, "no AAAA records found");
        let servfail = NetError::Dns(DnsError::ResponseCode(ResponseCode::ServFail));
        let (kind, reason) = lookup_failure(&servfail, RecordType::A);
        assert_eq!(kind, FailureKind::DnsFailed);
        assert_eq!(
            reason,
            "DNS lookup failed: server answered Server Failure (rcode 2)"
        );
    }

    /// A custom resolver whose UDP answer is truncated (a long TXT set) gets
    /// the full answer over TCP on the same port.
    #[tokio::test]
    async fn custom_resolver_retries_truncated_answers_over_tcp() {
        // The same port for both, as a resolver has it. The system hands out
        // a free UDP port; its TCP twin may be another test's listener, so
        // try again with the next one it offers.
        let (udp, tcp, addr) = loop {
            let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let addr = udp.local_addr().unwrap();
            match tokio::net::TcpListener::bind(addr).await {
                Ok(tcp) => break (udp, tcp, addr),
                Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {}
                Err(err) => panic!("binding {addr} over TCP: {err}"),
            }
        };
        let answer = |query: &Message, truncated: bool| {
            let mut reply = Message::response(query.metadata.id, query.metadata.op_code);
            reply.metadata.recursion_desired = true;
            reply.metadata.recursion_available = true;
            reply.metadata.truncation = truncated;
            reply.queries.clone_from(&query.queries);
            if !truncated {
                let name = query.queries[0].name().clone();
                reply.add_answer(Record::from_rdata(
                    name,
                    60,
                    RData::TXT(TXT::new(vec![
                        "v=spf1 include:example.com ~all".to_owned();
                        30
                    ])),
                ));
            }
            reply.to_vec().unwrap()
        };
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            loop {
                let (len, from) = udp.recv_from(&mut buf).await.unwrap();
                let query = Message::from_vec(&buf[..len]).unwrap();
                udp.send_to(&answer(&query, true), from).await.unwrap();
            }
        });
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = tcp.accept().await.unwrap();
                let len = usize::from(stream.read_u16().await.unwrap());
                let mut buf = vec![0u8; len];
                stream.read_exact(&mut buf).await.unwrap();
                let reply = answer(&Message::from_vec(&buf).unwrap(), false);
                stream
                    .write_u16(u16::try_from(reply.len()).unwrap())
                    .await
                    .unwrap();
                stream.write_all(&reply).await.unwrap();
            }
        });

        let resolver = resolver_for(Some(addr)).expect("resolver");
        let lookup = resolver
            .lookup(Name::from_ascii("big.example.").unwrap(), RecordType::TXT)
            .await
            .expect("answered over TCP");
        assert!(
            lookup
                .answers()
                .iter()
                .any(|record| record.record_type() == RecordType::TXT)
        );
    }
}
