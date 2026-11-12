---
title: Concepts
description: How a failed check becomes one alert, or none. Retries, fail_threshold, peers, topology, coalescing, maintenance and silences.
---

Most failed checks should never reach you. A packet lost on the way, a
service restarting for two seconds, your own network having a bad minute:
none of these is an outage. This page follows one check from the probe to
your phone, and shows where each kind of noise stops.

<figure class="hora-diagram">
<svg viewBox="0 0 520 544" role="img" aria-labelledby="flow-title flow-desc">
<title id="flow-title">From a failed check to one alert</title>
<desc id="flow-desc">A check runs and is retried once. A failure is recorded and shows as degraded. After fail_threshold failures in a row it is confirmed down. Peers are asked to confirm. Dependent monitors are grouped under their cause. One alert goes out. At each step, a branch shows what stops there: a retry that passes, fewer failures than the threshold, a maintenance window or silence, a place where the target is still up (not an outage: a quiet channel at most), and an upstream that is already down.</desc>
<defs><path id="ah" d="M-5 -7 0 0 5 -7Z"/></defs>
<g>
<rect class="box" x="10" y="10" width="250" height="56" rx="10"/>
<text class="t" x="26" y="34">A check runs</text><text class="s" x="26" y="53">every interval_secs</text>
<path class="ar" d="M135 66v20"/><use href="#ah" class="ar-head" x="135" y="88"/>
<rect class="box" x="10" y="88" width="250" height="56" rx="10"/>
<text class="t" x="26" y="112">It fails, and is retried</text><text class="s" x="26" y="131">probe_retries, 1 s apart</text>
<path class="exit" d="M260 116h30"/>
<rect class="box quiet" x="290" y="88" width="220" height="56" rx="10"/>
<text class="t" x="304" y="112">The retry passes</text><text class="s" x="304" y="131">nothing recorded, no trace</text>
<path class="ar" d="M135 144v20"/><use href="#ah" class="ar-head" x="135" y="166"/>
<rect class="box" x="10" y="166" width="250" height="56" rx="10"/>
<text class="t" x="26" y="190">A failure is recorded</text><text class="s" x="26" y="209">the page shows degraded</text>
<path class="exit" d="M260 194h30"/>
<rect class="box quiet" x="290" y="166" width="220" height="56" rx="10"/>
<text class="t" x="304" y="190">The next check passes</text><text class="s" x="304" y="209">below fail_threshold: no alert</text>
<path class="ar" d="M135 222v20"/><use href="#ah" class="ar-head" x="135" y="244"/>
<rect class="box" x="10" y="244" width="250" height="56" rx="10"/>
<text class="t" x="26" y="268">Confirmed down</text><text class="s" x="26" y="287">fail_threshold failures in a row</text>
<path class="exit" d="M260 272h30"/>
<rect class="box quiet" x="290" y="244" width="220" height="56" rx="10"/>
<text class="t" x="304" y="268">Maintenance or silence</text><text class="s" x="304" y="287">checks recorded, no alert</text>
<path class="ar" d="M135 300v20"/><use href="#ah" class="ar-head" x="135" y="322"/>
<rect class="box" x="10" y="322" width="250" height="56" rx="10"/>
<text class="t" x="26" y="346">Peers are asked</text><text class="s" x="26" y="365">confirm_with_peers, 10 s at most</text>
<path class="exit" d="M260 350h30"/>
<rect class="box quiet" x="290" y="322" width="220" height="56" rx="10"/>
<text class="t" x="304" y="346">Up from another place</text><text class="s" x="304" y="365">quiet channel, or recorded only</text>
<path class="ar" d="M135 378v20"/><use href="#ah" class="ar-head" x="135" y="400"/>
<rect class="box" x="10" y="400" width="250" height="56" rx="10"/>
<text class="t" x="26" y="424">Grouped by cause</text><text class="s" x="26" y="443">depends_on, group_window_secs</text>
<path class="exit" d="M260 428h30"/>
<rect class="box quiet" x="290" y="400" width="220" height="56" rx="10"/>
<text class="t" x="304" y="424">An upstream is down</text><text class="s" x="304" y="443">folded into its alert</text>
<path class="ar" d="M135 456v20"/><use href="#ah" class="ar-head" x="135" y="478"/>
<rect class="box end" x="10" y="478" width="250" height="56" rx="10"/>
<text class="end-t" x="26" y="502">One alert</text><text class="end-s" x="26" y="521">the cause and what it affects</text>
</g>
</svg>
</figure>

## 1. A check, and its retry

Each monitor runs on its own cadence (`interval_secs`, with `timeout_secs`
per attempt). When an attempt fails, Hora retries it before recording
anything: once by default, one second later (`probe_retries = 0..5`). A
packet lost once never reaches the history, the error budget or you.

[Push monitors](../guides/monitors/#push-heartbeat) work the other way
round: the job calls Hora. A heartbeat that does not arrive in time counts
as a failure, and so does a heartbeat that says so itself
(`status=down`, with the job's `msg` as the reason). A push monitor that
**never** sent a heartbeat fails too, once its first interval (or first
scheduled run plus grace) has passed since Hora started watching it. That
start time is kept across restarts.

## 2. Failures in a row: `fail_threshold`

A failure that survives its retry is recorded and logged, and the monitor
shows as **degraded** on the page. It is not called down yet:

```toml
[alerts]
fail_threshold = 3        # failures in a row before a monitor is down
alert_on_degraded = false # set true to be told about slowness too
```

Only `fail_threshold` failures **in a row** confirm the outage. With a
60-second interval and the default of 3, a service that restarts in under
two minutes never alerts. A single passing check resets the count.

The value in `[alerts]` is the default; a monitor may set its own
`fail_threshold` (`1` for a service whose first failure is already worth a
page, more for one known to flap). [`hora tune`](../reference/cli/#hora-tune) replays
your own history and tells you, monitor by monitor, how many alerts another
value would have sent, and how much later it would have caught each real
outage.

Slowness follows the same rule when you opt in: a monitor with
`degraded_over_ms` is degraded when it answers slower than that, and
`alert_on_degraded = true` alerts after `fail_threshold` slow checks in a
row.

## 3. Maintenance and silences

Two ways to say "I know, do not wake me":

- A **maintenance window** is planned and visible. It is declared in the
  config with a start and an end, and shown on the status page.
- A **silence** is ad hoc, for a deploy: `hora silence api,web 10m` or
  `POST /api/silence` from CI. At most 7 days; anything longer belongs in a
  maintenance window.

Both work the same way. Checks keep running and are recorded, so the
history and the error budget stay honest; only the alerts are held back.
The failure count does not advance while muted, so a monitor that is still
down when the window ends alerts after `fail_threshold` more failures.
Both apply to monitor ids and to watched peers (by their `listen_id`).

See [Maintenance windows](../guides/alerting/#maintenance-windows) and
[Ad-hoc silences](../guides/alerting/#ad-hoc-silences-deploy-hooks).

## 4. Confirmation from other places

A Hora node can be wrong about the internet: its own uplink fails, a route
breaks between one city and your server. With
[peers](../guides/peers/) and `confirm_with_peers = true`, a monitor that
has just been confirmed down asks the other nodes to probe the same target
from where they are, before the alert goes out.

- Down from every place: *"confirmed down from 3/3 vantage points"*.
- Up from somewhere else: *"seen UP by hora-b, down from 1/3 vantage points
  (network issue near this node?)"*.

In the second case, when every peer that answered sees the target up, the
down is **local-only**: the alert is flagged as seen from this node only and
the page says *Not an outage*. It goes to the monitor's channels, or to
`alerts.notify_unconfirmed` if you set one (a quiet channel; an empty list
only records it). The peers are asked again
every 5 minutes; if they come to see it down too, the real alert goes out
then. Anything short of that explicit contradiction - no peer answering, or
one peer seeing it down - alerts as usual: an outage seen from one region
is still an outage for someone. Peers only probe targets that are in their
own configuration, and the round never takes more than 10 seconds: a slow
or unreachable peer only means the alert goes out without the annotation.

## 5. Topology: one alert for a cascade

Declare what depends on what:

```toml
[[monitors]]
id = "api"
depends_on = ["db"]
```

When the database goes down, the API and everything behind it go down too.
Without topology, that is ten alerts at once. With it, Hora sends **one**:
the database is down, and it names what it affects.

This is the **coalescing** step. A monitor that has upstreams waits a short
window before alerting (`alerts.group_window_secs`, 30 seconds by default).
If one of its upstreams alerted before or during that wait, its alert folds
into the upstream's notification, and so does its recovery. A monitor that
recovers while it waits was a flap: nothing is sent at all. Set the window
to `0` for one alert per monitor.

Alerts and the page are annotated either way: *"caused by db"* on a
symptom, *"impacts: api, web"* on a root cause. The dependency graph is
checked for cycles when the config loads. Incident records are written for
every monitor, folded or not, so the [history](../guides/incidents/) stays
complete.

## 6. One alert, then quiet

The alert goes to the monitor's `notify` channels (or every channel), with
the reason, the root-cause annotation, the peers' verdict and, for HTTP, a
snippet of what the service answered. Then Hora stays quiet until the
monitor recovers, and sends the recovery only if it sent the down.

This state survives edits and restarts: changing a monitor's config during
an outage does not send the down again, and a restart picks up the open
incident instead of announcing it twice.

## Alongside the down alert

A few signals take their own path, because they answer different questions:

- **Burn-rate alerts** ([SLOs](../guides/slo/)) catch the service that flaps
  every half hour and never confirms down, yet spends its error budget.
- **Certificate, domain and release warnings** fire once per change, days
  ahead, and never mark a monitor down.
- **Pushed alerts** (`POST /api/monitors/{id}/alert`) let your own jobs
  report a failure the probe cannot see. They go straight to the channels,
  with a `dedup_key` so a retrying job pages once, and they never change
  the monitor's status.
- The **channel watchdog** tells the other channels when one of them keeps
  failing to deliver.
