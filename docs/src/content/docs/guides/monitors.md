---
title: Monitors
description: HTTP, TCP, ICMP, DNS and push probes - assertions, dual-stack, topology, visibility.
---

A monitor is a `[[monitors]]` entry with an `id`, a `name`, a `kind` and a
probing cadence (`interval_secs`, `timeout_secs`). Failed probes are retried
once (after 1 s) before anything is recorded - override with
`probe_retries = 0..5` - so a one-off network blip never pollutes the history
or the error budget. Every failure that survives its retry is logged with its
reason and shown as a tooltip on the status dot (plus `last_error` in the
API).

## A monitor's own `fail_threshold`

`[alerts] fail_threshold` (3 by default) is how many failed checks in a row
confirm a monitor down. Any monitor, of any kind, may set its own:

```toml
[[monitors]]
id = "payments"
name = "Payments"
target = "https://pay.example.com/health"
interval_secs = 30
fail_threshold = 1        # the first failed check is already an outage

[[monitors]]
id = "legacy-erp"
name = "Legacy ERP"
target = "https://erp.example.com"
interval_secs = 60
fail_threshold = 6        # known to hiccup: 6 min of failures before a page
```

It decides the alert, the status on the page and the badge alike, and
`alert_on_degraded` counts slow checks against it too. `probe_retries` still
runs first: with `fail_threshold = 1`, a failure that survives its immediate
retry alerts. A monitor used as an upstream (`depends_on`) is the cause of
its dependents' alerts once it is down by its own threshold. A missed
heartbeat on a push monitor never waits for any threshold (see
[push](#push-heartbeat)). [`hora tune`](../../reference/cli/#hora-tune) recommends a
value per monitor from its recorded history.

## Kinds

### `http` (default)

`target` is a URL; up = 2xx, or what `expected_status` lists if set. TLS
certificate expiry is checked automatically for `https://` targets.

```toml
[[monitors]]
id = "api"
name = "API"
target = "https://api.example.com/health"
interval_secs = 30
timeout_secs = 10
# expected_status = 200     # or a list: [200, 401], ["2xx", 401]
# method = "HEAD"           # "GET" (default) or "HEAD"
# degraded_over_ms = 800    # up but slower than this = degraded (yellow)
# headers = { Authorization = "Bearer ${TOKEN}" }
# proxy = "socks5://127.0.0.1:9050"
```

**`expected_status`** is a status (`200`), a class (`"2xx"`, any of `"1xx"`
to `"5xx"`) or a list of either. An API behind authentication that answers
401 when it is healthy, without giving Hora a credential:

```toml
expected_status = [200, 401]      # or ["2xx", 401]: any 2xx, or a 401
```

Anything the list leaves out is down, a 2xx included once the list does not
name it. A monitor whose list holds a 3xx (`301`, `[200, 301]`, `"3xx"`)
judges the first answer and does not follow redirects: see below.

**`method = "HEAD"`** asks for the headers alone: for a large page or a
download you do not want fetched every minute, or an endpoint that counts
its `GET`s. The answer to a `HEAD` has no body, so a monitor with `keyword`,
`json_query` or `number_regex` and `method = "HEAD"` is refused at load.
`"GET"` is the default and the only other method: Hora sends no request
body, so there is no `POST` or `PUT`. Some servers answer a `HEAD` with a
405 or another status than their `GET`: check with
`hora probe https://example.com --method HEAD` first.

**Body assertions** turn a reachability check into a correctness check:

```toml
keyword = "operational"   # body must contain this (keyword_invert = true → must NOT)
json_query = "$.status"   # JSONPath (RFC 9535) against a JSON body
json_expected = "ok"      # the queried value must equal this (omit = must match a node)
number_regex = 'in stock: (\d+)'  # extract a number from the body (first capture
                          # group, or the whole match) - down when nothing matches
number_min = 1            # ... or when the number is below this (inclusive)
number_max = 500          # ... or above this (inclusive)
max_body_kb = 256         # cap on the body read for assertions (default 1 MiB)
```

`number_regex` is the assertion for pages that render a gauge inline with no
JSON endpoint behind them - a queue depth, a stock level, a sensor's "receivers
online" counter. When the page is HTML, anchor the pattern on the surrounding
markup: `number_regex = 'class="shipCount">(\d+)<'` with `number_min = 1` goes
down the moment the counter reads zero (or disappears entirely).

Redirects are followed (up to 10), but configured headers, which may carry
credentials, are only re-attached while the redirect stays on the original
origin, so a compromised target can't bounce your API key to another host.
A redirect from `https://` to plain `http://` is not followed: the monitor
is down, *redirected to plain http*. Nor is a redirect from a target on a
public address to a loopback, link-local, private (RFC 1918, `fc00::/7`) or
carrier-grade NAT address: the monitor is down, *redirected to a private
address*, so a compromised site cannot have Hora fetch something inside
your network and show you the answer. A target that is itself on such an
address may redirect anywhere. (Through a `proxy`, the check uses this
host's view of DNS: the proxy does the resolving.) A monitor that expects a 3xx
(`expected_status = 301`, or a list or class that holds one) does **not**
follow redirects: it checks the redirect itself, such as HTTP to HTTPS or
apex to `www`. Every hop of a followed chain is sent with the monitor's
method: a `HEAD` stays a `HEAD`.

Compressed bodies (`gzip`, `br`, `deflate`, `zstd`) are decoded before the
assertions run, and `max_body_kb` caps the decoded size; an assertion that
fails on a body cut at the cap says so (*keyword missing: operational
(body cut at 256 KiB)*). A failed connection names its cause:
*connection failed: connection refused*, *connection failed: TLS invalid
peer certificate: Expired*. Through a `proxy`, a proxy that refuses the
request (`407`, a refused tunnel, SOCKS credentials) is reported as
*proxy failed: ...*, not as the target failing; the TLS certificate is read
through the proxy too.

### `tcp`

`target` is `host:port`; up = the TCP connect succeeds. A name with several
addresses is tried happy-eyeballs style: when one address does not answer
within 250 ms the next one is tried alongside it, so a dead IPv6 address
does not turn a working service into a timeout. The certificate watch
connects the same way.

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
An ICMP error instead (*destination unreachable*, *time exceeded*, sent by
a router on the way) is a down that names it. Uses an **unprivileged
datagram socket** - no `CAP_NET_RAW`, rootless-Docker friendly, IPv4 and
IPv6. If the socket is unavailable the monitor reports down with a clear
reason naming the sysctl to fix (`net.ipv4.ping_group_range`); it works
with every capability dropped, see
[ICMP monitors in Docker and Kubernetes](../../getting-started/#icmp-monitors-in-docker-and-kubernetes).

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

`dns_resolver` is an IP and a port; an IPv6 resolver is bracketed
(`"[2620:fe::fe]:53"`). A hostname is refused when the config loads. A
custom resolver is asked over UDP, then over TCP when the answer is too
large for a datagram (long TXT, SPF or DKIM records).

A failed lookup says what happened: *domain does not exist (NXDOMAIN)*
(shown publicly as *could not resolve host*), *no AAAA records found* when
the name exists without that record type, or *server answered Server
Failure (rcode 2)*.

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
  pipe), and a stuck plugin is killed at the monitor's timeout, with
  everything it started. A plugin that exits while a background child
  still holds its output (`sleep 30 &`) is judged by its exit code, and the
  child is killed.

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

No target: the job calls Hora. Down when no ping arrives within
`interval_secs`:

```sh
curl -fsS -X POST -H "X-Push-Token: ${BACKUP_TOKEN}" \
  "https://status.example.com/api/push/nightly-backup"
```

A push monitor **must** set a `push_token` (at least 16 characters), sent as
the `X-Push-Token` header: its id is on the status page, so the id alone
must not be enough to fake a heartbeat and hide a job that stopped. A config
without one fails to load. On a network where everyone who can reach
`/api/push` is trusted, `allow_unauthenticated = true` on the monitor is the
explicit opt-out; `?token=` in the URL is refused either way.

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
grace_secs = 1800            # how late a ping may be (default 30 min)
```

:::caution[The schedule is UTC]
`schedule` is read in **UTC**, not in the server's local time. A job run by
a local-time crontab (`0 3 * * *` in Europe/Paris) moves by an hour against
UTC twice a year with daylight saving time: write the schedule in UTC and
give `grace_secs` an hour of slack, or run the job itself on UTC
(`CRON_TZ=UTC`, a `TZ=UTC` systemd timer) so both clocks agree.
:::

An unknown push id answers 401, exactly like a wrong token, so the ids
cannot be listed from outside; the daemon logs the unknown id at debug
level.

Optional query parameters: `?status=up|down|degraded`, `msg=...` (recorded
with the heartbeat), `ping=<ms>` (round-trip latency). An unknown `status`
or a negative `ping` is refused with a 400, so a typo never reads as up.

**A heartbeat that does not come is down at once.** Hora looks at a push
monitor at least every minute; once the last heartbeat is older than
`interval_secs` (or a scheduled run is later than `grace_secs`), the
monitor is down and alerted, without waiting for `fail_threshold` (the
global one or the monitor's own): the
interval or the grace is the tolerance. Size them for the job, not for the
threshold.

**The job's own verdict drives alerting.** A heartbeat that says
`status=down` counts as a failure on the next tick, with the job's `msg` as
the reason, and the monitor is confirmed down after `fail_threshold`
evaluations (a few minutes; at once with `fail_threshold = 1` on the monitor), like any other check that ran and failed. `status=degraded` counts as degraded, so
`alert_on_degraded` works for push monitors, and the alert carries the
`msg`:

```sh
# the backup ran but failed: say so, rather than staying silent
curl -fsS -X POST -H "X-Push-Token: ${BACKUP_TOKEN}" \
  "https://status.example.com/api/push/nightly-backup?status=down&msg=disk+full"
```

**A push monitor that never pinged alerts too.** Once its interval (or its
first scheduled run plus `grace_secs`) has passed since Hora started
watching it, it is down with *"no heartbeat received yet"*. That start time
is stored, so a restart does not push the deadline back. If you declare a
monitor for a job that does not run yet, comment it out until it does.

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

Mail servers get the same watch: set `starttls = "smtp"` (or `"imap"`) on a
tcp monitor and the watcher negotiates STARTTLS before reading the
certificate.

```toml
kind = "tcp"
target = "mail.example.com:25"
starttls = "smtp"
ehlo_name = "status.example.com"   # optional
```

SMTP needs a client identity in `EHLO`. RFC 5321 requires a fully qualified
domain or an address literal, and MX servers enforce it on port 25
(`550 Invalid EHLO domain`). By default Hora announces the address literal
of its end of the connection (`[192.0.2.10]`), which is valid even in a
container whose hostname is a bare id. Set `ehlo_name` to announce a real
name instead.

## Domain expiry (RDAP)

The natural sibling of the TLS warnings: *"your domain expires in 14 d"*.

```toml
domain_expiry = "example.com"
```

The **registered** domain (not the subdomain the target uses - that's why it
is explicit rather than derived) is checked once a day against the registry
over RDAP - JSON over HTTP, no whois parsing - and an alert fires
`alerts.domain_expiry_days` (default 14) before it expires, with the same
edge-triggered, maintenance-muted policy as certificates.

## Upstream releases

The update you hear about three weeks late is a security fix you ran without
for three weeks. A monitor can name the project it runs:

```toml
release = { github = "matrix-construct/tuwunel", current = "v1.9.1" }
```

Twice a day Hora asks GitHub for the project's **latest release** (drafts and
prereleases are not) and alerts, **once per release**, when it is newer than
the version that runs: *"matrix-construct/tuwunel v1.9.2 is out (running
v1.9.1)"*, with the link to the release notes. Nothing is down, so the
monitor's status does not change; the alert follows the monitor's `notify`
routing and is muted during its maintenance windows.

`current` is a line to edit on each upgrade. Better, let the service say what
it runs, and the watch can never drift from what is deployed:

```toml
# a bare version as text (Element Web serves one at /version)
release = { github = "element-hq/element-web", current_url = "https://chat.example.com/version" }

# a version inside JSON (a Matrix homeserver, MSC4383)
[monitors.release]
github = "matrix-construct/tuwunel"
current_url = "https://chat.example.com/_matrix/client/versions"
current_query = "$['net.zemos.msc4383.server'].version"
```

Versions are compared by their leading numbers: `v1.9.2`, `1.9.2` and
`1.9.2 (a1b2c3)` are one version, `1.9` is `1.9.0`, and a build that runs ahead
of the latest release says nothing. When a tag or a version carries no leading
number (`release-42`), any difference alerts. GitHub's anonymous API allows 60
requests an hour per address, far above what a few dozen monitors ask; a
project that only pushes tags, without publishing releases, cannot be watched.

## Visibility

```toml
public = false               # hide from unauthenticated viewers
public_error_detail = true   # publish raw failure reasons to anonymous viewers
```

A monitor with `public = false` disappears from the unauthenticated status
page, API, badges and history (its latency endpoint answers 404, exactly like
a missing monitor); the operator token (`server.auth_token`) or the
read-only one (`server.view_token`) reveals the full view. By default anonymous viewers see only a **safe category** of a failure
("HTTP 500", "content check failed") - the stored detail can carry response
snippets, DNS answers or asserted keywords; `public_error_detail = true` opts
a monitor into publishing the full reason (and the
[failure snapshot](../incidents/#failure-snapshots)).

## Latency and SLO fields

```toml
degraded_over_ms = 800   # up but slower = degraded
slo_latency_ms = 500     # the 24 h p95 is flagged met/breached against this
slo_uptime = 99.9        # availability SLO; shows error budget, arms burn-rate alerts
slo_window_days = 30     # SLO window (default 30)
retention_days = 30      # how long this monitor's raw history is kept
```

See [SLOs & error budgets](../slo/) for how the budget and burn-rate alerts
work.
