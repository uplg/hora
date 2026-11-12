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
listen_token = "${PEER_B_IN}"                    # required with expect_every_secs (16+ characters)
# listen_id = "hora-b"                           # the push id it pings: /api/push/{listen_id} (default: id)
# witness_url = "https://b.example/healthz"      # default: origin(ping_url)/healthz
# notify = ["ops-telegram"]                      # route this peer's alerts
```

A watched peer **must** set a `listen_token`: peer ids are served on the
unauthenticated `/healthz`, so the id alone must not be enough to forge its
heartbeats and hide that it is down. A config without one fails to load;
`allow_unauthenticated = true` on the peer is the explicit opt-out for a
network where everyone who can reach `/api/push` is trusted.

Watched peers appear in their own section on the status page (their state
does not roll into the overall badge: it tracks your services, not the
surveillance mesh). `[health]` and `[[peers]]` reload live like everything
else.

A watched peer follows the same rules as a monitor:

- It is down after `alerts.fail_threshold` missed heartbeats in a row (the
  global value: a monitor's own `fail_threshold` is for monitors).
- A peer that has **never** sent a heartbeat alerts too ("no heartbeat
  received yet") once `expect_every_secs` has passed since this node started
  watching it. That start is stored, so restarts do not reset it. For
  `grace_secs` after this node starts, peer alerts are held, so two nodes
  rebooting together do not page each other.
- It can be muted: a `[[maintenance]]` window or a silence that names its
  `listen_id` (`hora silence hora-b 30m "rebooting"`) - or the peer says so
  itself, ahead of time: see
  [planned maintenance of a host](#planned-maintenance-of-a-host).
- Its alert state survives a config edit or a restart: no repeated down,
  no lost recovery.

## Planned maintenance of a host

Two nodes, `frank` and `yuki`, watch each other. When frank installs a
kernel and reboots at 05:00, two things happen that are no surprise to
anyone: frank's Hora stops heartbeating, and the services hosted on frank
go down for three minutes. Without more, yuki alerts for both.

Frank knows about the reboot hours ahead. It can say so, once, to its own
Hora - and both nodes stay quiet for exactly that window, and alert as
usual if frank is not back when it ends.

### Label the hosts

Three optional `host` keys, all free text, say what runs where. On frank:

```toml
[health]
id = "hora-frank"
host = "frank"                  # the machine this node runs on

[[monitors]]
id = "mail"
name = "Mail"
kind = "tcp"
target = "mail.example.com:25"
interval_secs = 60
host = "frank"                  # the machine the service runs on
```

On yuki, which probes the same mail server and watches frank:

```toml
[[monitors]]
id = "mail"
# ... the same check ...
host = "frank"

[[peers]]
id = "hora-frank"
name = "Frank"
ping_url = "https://frank.example.com/api/push/hora-yuki"
ping_token = "${TO_FRANK}"
expect_every_secs = 180
listen_token = "${FROM_FRANK}"
host = "frank"                  # the machine that peer runs on
accept_maintenance_secs = 900   # trust its announcements, 15 min at most
```

### One call from the host

The script that learns about the reboot (on a systemd host, it reads
`/run/systemd/shutdown/scheduled`) makes one call to its **local** Hora,
with the operator token:

```sh
curl -fsS -X POST -H "Authorization: Bearer $HORA_TOKEN" \
  "http://127.0.0.1:8787/api/maintenance?host=frank&starts_at=2026-11-13T05:00:00Z&duration=10m&reason=kernel%20reboot"
```

or, on the same machine, `hora maintenance plan --host frank --at
2026-11-13T05:00:00Z --for 10m "kernel reboot"`. The script may run every
few minutes: the same host, start and duration is the same window, not a
second one. (Pass the start explicitly for that - the default, now, moves.)

From then on:

- **On frank**, the monitors labelled `host = "frank"` are in maintenance
  from 05:00 to 05:10: alerts muted, checks still recorded, the status page
  says *Planned maintenance*, and before 05:00 it announces the window as
  *Next*.
- **Frank tells its peers.** Each heartbeat carries the windows frank has
  planned, until they end. A window for frank's own host (`[health] host`)
  goes out with the host; a window for named monitors
  (`monitors=mail,web`) goes out without one; a window for another host
  than frank's own is not announced at all.
- **On yuki**, which accepts frank's announcements, the same ten minutes
  mute the watch of frank (no *peer silent* alert) and yuki's own monitors
  labelled `host = "frank"`. Yuki's page shows the window, and tells the
  operator it was announced by Frank.
- **At 05:10** everything alerts normally again. If frank is still silent,
  the peer alert goes out after the usual `fail_threshold` missed
  heartbeats counted from there; a service still down is confirmed and
  announced the same way. A service that came back during the window was
  never announced down, so nothing is said about it.

`hora maintenance list` (or `GET /api/maintenance`) shows the planned and
active windows of a node, the peers' included, and `hora maintenance cancel
<id>` (or `DELETE /api/maintenance/{id}`) withdraws one: frank's next
heartbeat no longer lists it, and yuki drops it on receipt.

### What a peer may mute

`accept_maintenance_secs` is a trust decision, and off unless you set it:
you let another machine decide that this one stays quiet. What it buys the
peer is deliberately small.

- Nothing at all unless the `[[peers]]` entry sets
  `accept_maintenance_secs`, and unless the heartbeat that carries the
  announcement is authenticated with that peer's `listen_token`. The key
  is refused together with `allow_unauthenticated`.
- A window lasts at most `accept_maintenance_secs`: a longer one is cut
  from its start, not refused. A window that starts more than 7 days ahead
  is refused.
- A peer has a budget of four times `accept_maintenance_secs` of muted
  time in any 24 hours (an hour a day at 900 seconds), overlapping windows
  counted once. Past it a window is cut, then refused, and the alerts come
  back: a script that plans a window every minute cannot keep a host silent.
- A peer has a budget of four times `accept_maintenance_secs` of muted
  time in any 24 hours (an hour a day at 900 seconds), overlapping windows
  counted once. Past it a window is cut, then refused, and the alerts come
  back: a script that plans a window every minute cannot keep a host silent.
- It always mutes the watch of **that peer**, and nothing else of the mesh.
- It mutes this node's monitors labelled with a host only when the
  announcement names a host **and** that host is the `host` of the peer's
  own `[[peers]]` entry. A peer cannot declare maintenance for another
  host, and cannot name monitors: a monitor without the peer's host label
  is never muted by it.
- Removing `accept_maintenance_secs` takes effect at once, for the windows
  already accepted too.

An accepted window is logged once (`maintenance window accepted from the
peer`), a refused announcement at most once an hour, with the reason.

A window mutes alerts, never observations: a node asked by a peer to
[confirm a down](#multi-vantage-confirmation) for a monitor it has in
maintenance still probes and answers what it sees.

### Mixed versions

The announcement rides the heartbeat as a JSON body, sent only when there
is something to announce. A 0.11 node never reads a heartbeat's body, so a
0.12 node's heartbeat counts for it as before, and it mutes nothing; a 0.11
node sends no body, which a 0.12 node reads as "nothing planned". Upgrade
both nodes before relying on it.

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
  this node?)"* - probably your fibre, not the service.

Two Raspberry Pi at two homes become a distributed Pingdom.

### Local-only downs

When **every peer that answered sees the target up** (and at least one
answered), the down is *local-only*: a problem on the road from this node,
not an outage. The alert says so (*seen from this node only*), the status
page shows *Not an outage*, and where it goes is yours to choose with
`alerts.notify_unconfirmed`:

```toml
[alerts]
# unset (default): the monitor's own channels, flagged as local-only
notify_unconfirmed = ["ntfy-low"]   # a quiet channel instead of the monitor's
# notify_unconfirmed = []           # nowhere: only recorded (incident, timeline)
```

The default errs on the side of telling someone: a peer answering "up" is
one HTTP response, and it should not be able to silence a down on its own.
Name a quiet channel once you trust the mesh, or set the empty list to only
record such downs.

Only an explicit contradiction quiets a down. No peer configured, no peer
reachable, peers that do not watch this target, or **any** peer seeing it
down too: the alert goes out normally - geo-partial outages are real
outages.

While a local-only down lasts, the node asks its peers again every 5
minutes. As soon as they no longer all see the target up (they see it down
too, or stop answering), the real down alert goes out, once, to the usual
channels. A recovery goes to whoever received a down: for a down that
stayed local-only, the channels that took it (the quiet ones, or the
monitor's own by default); both after a late confirmation. The
[webhook](../alerting/#channels) payload carries `local_only: true` on the
`down` and `recovered` events of a local-only down.

Confirmation only applies to network probes (http, tcp, icmp, dns). Exec
and push monitors have no remote point of view, so they are never sent to
peers, and a peer asked to probe one answers 400.

The same round is available on demand from the terminal with
[`hora probe <id> --confirm`](../../reference/cli/#hora-probe): it prints the
verdict both ways - *"down for everyone or just me?"* when the target is down
here, and *"up from 3/3 vantage points"* when it is up.

### How a probe request works

The requester `POST`s to the peer's `/api/peer/probe` (derived from the
origin of `ping_url`), authenticated with the same token pair as heartbeats:
it presents its `ping_token`, which the responder checks against its
`listen_token` for that peer - **required** here; the id alone never
authorizes a probe.

Because a peer's "up" changes what happens to an alert, a node with
`confirm_with_peers` on only loads peers whose `ping_url` is **https**: over
cleartext http anyone on the path could write that answer, or read the token
that asks for it. `http://localhost` and loopback addresses stay allowed
(two nodes on one host, tests).

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

## Seeing from elsewhere

Every peer with a `ping_url` is also asked, once a minute, how it sees the
targets both nodes monitor. The status page then shows each shared monitor
from the other places ("Hora B: 220 ms"). The exchange is
authenticated like a probe request, read-only and fail-open.

Confirmation only works for monitors both nodes have. To check that the
mesh agrees, run:

```sh
hora peers diff
```

It lists, per peer, the monitors (kind and target) that only one side has
and those both have but check differently, and exits non-zero on any
difference or an unreachable peer, so it can gate a config deploy in CI.

A peer confirms a down only with a monitor that checks the same way: same
kind and target, same `method`, same expected statuses, assertions and headers, same
`dual_stack`, DNS record, pinned answer and resolver, same STARTTLS
protocol. Timeouts, intervals, thresholds and the proxy are each node's own
business. A peer that watches the URL with another assertion answers as if
it did not watch it, and the down is alerted unconfirmed. The target is sent
to peers with the secrets of its URL masked.

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
