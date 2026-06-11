---
title: Mutual surveillance (peers)
description: Who watches the watcher - dead-man heartbeats between Hora nodes, quorum, and external receivers.
---

A monitor is only as good as the machine it runs on. Two Hora nodes (two
Raspberry Pi at two friends' places, a VPS and a homelab...) can watch each
other with **dead-man heartbeats**: each node pings the other while it is
healthy; when the pings stop, the survivor alerts.

Nothing here is a bespoke protocol - every exchange is plain HTTP, so a peer
can be another Hora, a healthchecks.io / UptimeRobot endpoint, or a cron job.

## The outbound heartbeat: `[health]`

```toml
[health]
id = "hora-a"        # this node's identity (how peers refer to it)
interval_secs = 60   # heartbeat cadence while healthy
grace_secs = 180     # startup grace before a never-seen peer is alerted
# quorum = true      # see below
# heartbeat_url = "${HC_PING_URL}"   # optional extra dead-man target
```

The node POSTs to each peer's `ping_url` every `interval_secs` - but **only
while it is locally healthy** (its scheduler is ticking and its database is
writable). A hung process stops pinging, and the receiver notices.

## Peers: two independent halves

Each `[[peers]]` entry can declare either or both directions:

```toml
[[peers]]
id = "hora-b"                                    # the peer's [health].id
name = "Hora B (Paris)"
# OUT - I heartbeat the peer while I'm healthy:
ping_url = "https://b.example/api/push/hora-a"
ping_token = "${PEER_B_TOKEN}"                   # sent as X-Push-Token
# IN - I watch the peer and alert if it goes silent:
expect_every_secs = 90
listen_token = "${PEER_B_IN}"                    # required from the peer's pings
# witness_url = "https://b.example/healthz"      # default: origin(ping_url)/healthz
# notify = ["ops-telegram"]                      # route this peer's alerts
```

Watched peers appear in their own section on the status page (their state
does not roll into the overall badge - it tracks your services, not the
surveillance mesh). `[health]` and `[[peers]]` reload live like everything
else.

## Quorum: outage or partition?

With three or more nodes, set `quorum = true`: before alerting a peer down,
the node asks the *other* peers' `/healthz` whether they still see it. If any
does, it is a network partition between you two - reported as a degraded peer
link, not a false "node down". A no-op with fewer than three nodes.

## Multi-vantage confirmation

The peers don't just watch *each other* - they can confirm **your monitors'
outages** from their vantage point. With

```toml
[health]
id = "hora-a"
confirm_with_peers = true   # per-monitor override: confirm_with_peers = true/false
```

a monitor that confirms down locally triggers one concurrent round of probe
requests to the peers before the alert goes out, and the alert carries the
verdict:

- *"confirmed down from 3/3 vantage points"* - a real outage;
- *"seen UP by hora-b - down from 1/3 vantage points (network issue near
  this node?)"* - probably your fibre, not the service. The alert is
  **softened, never silenced**: geo-partial outages are real outages.

Two Raspberry Pi at two homes become a distributed Pingdom.

### How a probe request works

The requester `POST`s to the peer's `/api/peer/probe` (derived from the
origin of `ping_url`), authenticated with the same token pair as heartbeats:
it presents its `ping_token`, which the responder checks against its
`listen_token` for that peer - **required** here; the id alone never
authorizes a probe.

The responder only probes targets **present in its own configuration**
(matched on kind + target, probed with its own timeout, assertions and
proxy) - never an arbitrary target, so a leaked token cannot turn a peer
into an SSRF relay. Both nodes must therefore know the monitor, which pairs
naturally with sharing the config in git.

### Failure behaviour

Strictly fail-open, by construction: peers being slow, unreachable, behind a
wrong token or unaware of the target never block the alert, never delay it
past a hard 10-second deadline (probes run concurrently), and never suppress
it. The worst possible outcome is an alert *without* the vantage annotation -
exactly what Hora sent before the feature. The incident record is written
before the peers are even consulted.

## External receivers

- **OUT-only peer** - a plain dead-man to an external service:

  ```toml
  [[peers]]
  id = "healthchecks"
  name = "healthchecks.io"
  ping_url = "https://hc-ping.com/<uuid>"
  ```

- **Be watched by UptimeRobot** - point a keyword monitor at this node's
  `/healthz` and match the keyword `ok` (the top-level `status` field is
  `ok` only while the node is fully healthy). No peer entry needed.
