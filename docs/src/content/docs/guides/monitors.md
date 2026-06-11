---
title: Monitors
description: HTTP, TCP, ICMP, DNS and push probes - assertions, dual-stack, topology, visibility.
---

A monitor is a `[[monitors]]` entry with an `id`, a `name`, a `kind` and a
probing cadence (`interval_secs`, `timeout_secs`). Failed probes are retried
once (after 1s) before anything is recorded - override with
`probe_retries = 0..5` - so a one-off network blip never pollutes the history
or the error budget. Every failure that survives its retry is logged with its
reason and shown as a tooltip on the status dot (plus `last_error` in the
API).

## Kinds

### `http` (default)

`target` is a URL; up = 2xx, or exactly `expected_status` if set. TLS
certificate expiry is checked automatically for `https://` targets.

```toml
[[monitors]]
id = "api"
name = "API"
target = "https://api.example.com/health"
interval_secs = 30
timeout_secs = 10
# expected_status = 200
# degraded_over_ms = 800    # up but slower than this = degraded (yellow)
# headers = { Authorization = "Bearer ${TOKEN}" }
# proxy = "socks5://127.0.0.1:9050"
```

**Body assertions** turn a reachability check into a correctness check:

```toml
keyword = "operational"   # body must contain this (keyword_invert = true → must NOT)
json_query = "$.status"   # JSONPath (RFC 9535) against a JSON body
json_expected = "ok"      # the queried value must equal this (omit = must match a node)
max_body_kb = 256         # cap on the body read for assertions (default 1 MiB)
```

Redirects are followed (up to 10), but configured headers - which may carry
credentials - are only re-attached while the redirect stays on the original
origin, so a compromised target can't bounce your API key to another host.

### `tcp`

`target` is `host:port`; up = the TCP connect succeeds.

```toml
[[monitors]]
id = "database"
name = "Database"
kind = "tcp"
target = "db.example.com:5432"
interval_secs = 60
timeout_secs = 5
```

### `icmp`

`target` is a host or IP (no port); up = an echo reply within the timeout.
Uses an **unprivileged datagram socket** - no `CAP_NET_RAW`, rootless-Docker
friendly, IPv4 and IPv6. If the socket is unavailable the monitor reports
down with a clear reason naming the sysctl to fix
(`net.ipv4.ping_group_range`).

### `dns`

`target` is a hostname to resolve. Without `dns_expected` any non-empty
answer counts as up - CDN and round-robin answers rotate constantly, so
alerting on mere change would flap. With it, the answer is **pinned**
(hijack detection), compared order-insensitively:

```toml
[[monitors]]
id = "dns-check"
name = "DNS Check"
kind = "dns"
target = "example.com"
interval_secs = 300
dns_record = "A"             # A (default), AAAA, CNAME, MX, NS, TXT, SRV, SOA, PTR
dns_expected = "1.2.3.4"     # comma-separated, order-insensitive
dns_resolver = "8.8.8.8:53"  # optional; default: system resolver
```

### `exec`

Run any external check following the **monitoring-plugins convention**: exit
0 = up, 1 = degraded, anything else = down; the first stdout line is the
message (the `|perfdata` tail is stripped). The whole Nagios/Icinga plugin
ecosystem - thousands of checks: RAID, disks, exotic certificates, SNMP -
becomes usable from Hora.

```toml
[[monitors]]
id = "raid"
name = "RAID"
kind = "exec"
command = ["check_raid", "--no-sudo"]   # a raw argv - no shell, ever
interval_secs = 300
timeout_secs = 30                       # the plugin is SIGKILLed past this
```

**The security model is the `HORA_EXEC_DIR` environment variable** -
deliberately *not* a config key. The config is hot-reloaded, so it alone
must never be able to run code: enabling exec probes takes deployment-level
access on top of config access. With the variable set:

- `command[0]` is resolved **strictly inside that directory** - a bare file
  name, validated at load, canonicalized at run time (a symlink planted in
  the directory that points outside is refused);
- no shell is involved: `command` is an argv array, so nothing is ever
  interpolated or injected;
- the plugin gets a **scrubbed environment** (`PATH`, `HOME`, `LANG`, `TZ`):
  the daemon's own environment carries your notification tokens, and no
  plugin has any business reading them;
- output is bounded and drained (a chatty plugin never deadlocks on a full
  pipe), and a stuck plugin is killed at the monitor's timeout.

Without `HORA_EXEC_DIR`, a config declaring an exec monitor fails at load -
and `hora doctor` verifies the directory and that every configured plugin is
actually present.

:::tip[Recipe: watching other containers from a rootless Hora]
In **rootless Docker**, mounting the Docker socket only grants your user's
rights - not root on the host - which makes this pattern reasonable:

```sh
# checks/check_container - exit 0 if the container reports healthy
#!/bin/sh
state=$(curl -sf --unix-socket /var/run/docker.sock \
  "http://localhost/containers/$1/json" | jq -r '.State.Health.Status // .State.Status')
case "$state" in
  healthy|running) echo "container $1 $state"; exit 0 ;;
  *) echo "container $1 $state"; exit 2 ;;
esac
```

```sh
docker run -d --name hora \
  -v ./hora-config:/etc/hora \
  -v ./checks:/etc/hora/checks:ro \
  -e HORA_EXEC_DIR=/etc/hora/checks \
  -v $XDG_RUNTIME_DIR/docker.sock:/var/run/docker.sock:ro \
  my-hora-with-tools
```

with a one-line derived image for the tools your scripts need:

```dockerfile
FROM ghcr.io/uplg/hora:latest
RUN apk add --no-cache curl jq monitoring-plugins
```

`monitoring-plugins` alone brings `check_disk`, `check_load`, `check_smtp`,
`check_snmp` and friends - each one a `command = ["check_..."]` away from
uptime bars, SLOs and the weekly digest.
:::

Exec monitors take no `target`, are excluded from
[multi-vantage confirmation](../peers/#multi-vantage-confirmation) (a local
check has no remote vantage), and their failure detail collapses to
*"plugin check failed"* for anonymous viewers unless `public_error_detail`
opts in - plugin output names devices and container ids.

### `push` (heartbeat)

No target - the job calls Hora. Down when no ping arrives within
`interval_secs`:

```sh
curl -fsS -X POST -H "X-Push-Token: ${BACKUP_TOKEN}" \
  "https://status.example.com/api/push/nightly-backup"
```

The **cron-aware variant** declares *when* the job runs, and alerts only when
a scheduled run misses its grace window - a 03:00 backup pinging at 03:05 is
fine, one still silent at 03:30 + grace is down:

```toml
[[monitors]]
id = "nightly-backup"
name = "Nightly backup"
kind = "push"
interval_secs = 60           # with `schedule`, just the re-evaluation cadence
push_token = "${BACKUP_TOKEN}"
schedule = "0 3 * * *"       # five-field cron, UTC
grace_secs = 1800            # how late a ping may be (default 30m)
```

Optional query parameters: `?status=up|down|degraded`, `msg=...` (recorded
with the heartbeat), `ping=<ms>` (round-trip latency).

## Dual-stack verification

```toml
dual_stack = true
```

probes IPv4 *and* IPv6 separately (concurrently) and requires **both**: the
classic silent failure is a service whose IPv6 has been dead for weeks behind
a healthy IPv4 (or the reverse), invisible to every single-connection check.
One broken family confirms down with the culprit named - *"IPv6 failing:
connection timed out (IPv4 ok)"* - and the surviving family's latency
recorded; when both answer, the recorded latency is the slower path's, so
`degraded_over_ms` judges the worst case.

Works for `http`, `tcp` and `icmp` monitors with a hostname target (an IP
literal has a single family); cannot be combined with `proxy`.

:::caution[The probing host needs both families]
In Docker, default bridge networks have **no IPv6** - a dual-stack monitor
would blame your container's network, not your service. Enable IPv6 on the
daemon/compose network (or use host networking) first.
:::

## Topology: groups and dependencies

```toml
group = "infra"              # display group on the status page
depends_on = ["db", "cache"] # upstream monitors this one depends on
```

When a monitor goes down its alert is annotated with root cause vs. symptom:
*"caused by X"* when an upstream it depends on is also down, or
*"impacts: A, B, C"* (the blast radius) when its upstreams are all healthy
and it is the root cause. The dependency graph is validated acyclic at load.
Dependencies also drive [root-cause alert grouping](../alerting/#root-cause-grouping).

## TLS certificates

For `https://` monitors, expiry is checked automatically and warned
`alerts.cert_expiry_days` in advance (default 14). Optionally **pin the
public key**:

```toml
cert_pin = "abc123..."   # SHA-256 of the leaf public key
```

An unexpected key change - MITM, botched renewal - alerts once per change,
with the old and new fingerprints. Disable expiry checking per monitor with
`check_cert = false`.

## Domain expiry (RDAP)

The natural sibling of the TLS warnings: *"your domain expires in 14 days"*.

```toml
domain_expiry = "example.com"
```

The **registered** domain (not the subdomain the target uses - that's why it
is explicit rather than derived) is checked once a day against the registry
over RDAP - JSON over HTTP, no whois parsing - and an alert fires
`alerts.domain_expiry_days` (default 14) before it expires, with the same
edge-triggered, maintenance-muted policy as certificates.

## Visibility

```toml
public = false               # hide from unauthenticated viewers
public_error_detail = true   # publish raw failure reasons to anonymous viewers
```

A monitor with `public = false` disappears from the unauthenticated status
page, API, badges and history (its latency endpoint answers 404, exactly like
a missing monitor); the viewer token (`server.auth_token`) reveals the full
view. By default anonymous viewers see only a **safe category** of a failure
("HTTP 500", "content check failed") - the stored detail can carry response
snippets, DNS answers or asserted keywords; `public_error_detail = true` opts
a monitor into publishing the full reason (and the
[failure snapshot](../incidents/#failure-snapshots)).

## Latency and SLO fields

```toml
degraded_over_ms = 800   # up but slower = degraded
slo_latency_ms = 500     # the 24h p95 is flagged met/breached against this
slo_uptime = 99.9        # availability SLO; shows error budget, arms burn-rate alerts
slo_window_days = 30     # SLO window (default 30)
retention_days = 30      # how long this monitor's raw history is kept
```

See [SLOs & error budgets](../slo/) for how the budget and burn-rate alerts
work.
