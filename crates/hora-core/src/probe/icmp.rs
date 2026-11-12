//! ICMP echo (ping) checks.

use super::FailureKind;
use socket2::Type;
use surge_ping::{
    Client as PingClient, Config as PingConfig, ICMP, IcmpPacket, PingIdentifier, PingSequence,
    SurgeError,
};

use super::family::Family;
use super::{Outcome, millis, over_threshold, resolve};
use crate::config::{IcmpSpec, Monitor};

/// ICMP echo (ping). Uses a per-probe unprivileged datagram socket (no
/// `CAP_NET_RAW`), so it works in rootless Docker; one socket per probe avoids
/// datagram identifier collisions between concurrent monitors. The address family
/// (IPv4/IPv6) follows the resolved address.
pub(super) async fn icmp(monitor: &Monitor, spec: &IcmpSpec) -> Outcome {
    icmp_family(monitor, spec, None).await
}

/// ICMP to the first resolved address - of one specific family when given.
pub(super) async fn icmp_family(
    monitor: &Monitor,
    spec: &IcmpSpec,
    family: Option<Family>,
) -> Outcome {
    // Resolution counts against the monitor's timeout, like the probe: a
    // resolver that never answers must not park this monitor's loop.
    let resolved = tokio::time::timeout(monitor.timeout(), resolve(&spec.target, family));
    let Some(addr) = resolved.await.ok().flatten() else {
        return match family {
            Some(family) => Outcome::down(
                FailureKind::Other,
                format!("no {} address for host", family.label()),
            ),
            None => Outcome::down(
                FailureKind::Unresolvable,
                "could not resolve host".to_owned(),
            ),
        };
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
            return Outcome::down(
                FailureKind::Ping,
                format!(
                    "icmp socket unavailable ({err}); needs net.ipv4.ping_group_range or CAP_NET_RAW"
                ),
            );
        }
    };

    let mut pinger = client.pinger(addr, PingIdentifier(0)).await;
    pinger.timeout(monitor.timeout());
    match pinger.ping(PingSequence(0), &[0u8; 16]).await {
        // surge-ping also hands back the ICMP *errors* that quote our request
        // (destination unreachable, time exceeded...) as a "reply": only an
        // echo reply means the host answered.
        Ok((packet, _rtt)) if !is_echo_reply(&packet) => {
            Outcome::down(FailureKind::Ping, icmp_error(&packet))
        }
        Ok((_packet, rtt)) => {
            let latency = millis(rtt);
            Outcome::up(
                over_threshold(latency, monitor.degraded_over_ms),
                Some(latency),
                None,
            )
        }
        Err(SurgeError::Timeout { .. }) => {
            Outcome::down(FailureKind::Timeout, "request timed out".to_owned())
        }
        Err(err) => Outcome::down(FailureKind::Ping, format!("icmp error: {err}")),
    }
}

/// Whether a packet matched to our request is an echo reply (type 0 over
/// IPv4, 129 over IPv6) rather than an error quoting the request.
fn is_echo_reply(packet: &IcmpPacket) -> bool {
    match packet {
        IcmpPacket::V4(packet) => packet.get_icmp_type().0 == 0,
        IcmpPacket::V6(packet) => packet.get_icmpv6_type().0 == 129,
    }
}

/// The reason for an ICMP error answered instead of an echo reply. Over IPv4
/// the error names the router that sent it; surge-ping reports an IPv6 error
/// as coming from the target itself, so no sender is named there.
fn icmp_error(packet: &IcmpPacket) -> String {
    let (kind, code, from) = match packet {
        IcmpPacket::V4(packet) => (
            match packet.get_icmp_type().0 {
                3 => "destination unreachable",
                11 => "time exceeded",
                _ => "unexpected reply",
            },
            packet.get_icmp_code().0,
            Some(packet.get_source().to_string()),
        ),
        IcmpPacket::V6(packet) => (
            match packet.get_icmpv6_type().0 {
                1 => "destination unreachable",
                3 => "time exceeded",
                _ => "unexpected reply",
            },
            packet.get_icmpv6_code().0,
            None,
        ),
    };
    match from {
        Some(from) => format!("icmp {kind} (code {code}) from {from}"),
        None => format!("icmp {kind} (code {code})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;
    use surge_ping::Icmpv6Packet;

    const TARGET: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);

    /// The (IPv6) echo header of request 7/0: type, code, checksum, id, seq.
    const ECHO_REQUEST: [u8; 8] = [128, 0, 0, 0, 0, 7, 0, 0];

    /// An IPv6 ICMP error of `kind`/`code` quoting our echo request to TARGET.
    fn error_quoting_request(kind: u8, code: u8) -> IcmpPacket {
        let mut buf = vec![kind, code, 0, 0, 0, 0, 0, 0];
        // The quoted IPv6 base header: version 6, payload length 8, next
        // header ICMPv6 (58), hop limit 64, source ::, destination TARGET.
        buf.extend_from_slice(&[0x60, 0, 0, 0, 0, 8, 58, 64]);
        buf.extend_from_slice(&[0; 16]);
        buf.extend_from_slice(&TARGET.octets());
        buf.extend_from_slice(&ECHO_REQUEST);
        IcmpPacket::V6(Icmpv6Packet::decode(&buf, TARGET).expect("decodes"))
    }

    #[test]
    fn only_an_echo_reply_counts_as_an_answer() {
        let reply = Icmpv6Packet::decode(&[129, 0, 0, 0, 0, 7, 0, 0], TARGET).expect("decodes");
        assert!(is_echo_reply(&IcmpPacket::V6(reply)));

        // A router saying the host is unreachable "answers" our request too:
        // that is a down, with the reason.
        let unreachable = error_quoting_request(1, 3);
        assert!(!is_echo_reply(&unreachable));
        assert_eq!(
            icmp_error(&unreachable),
            "icmp destination unreachable (code 3)"
        );
        let expired = error_quoting_request(3, 0);
        assert!(!is_echo_reply(&expired));
        assert_eq!(icmp_error(&expired), "icmp time exceeded (code 0)");
    }
}
