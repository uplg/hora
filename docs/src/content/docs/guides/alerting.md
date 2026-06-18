---
title: Alerting & notifications
description: Channels, routing, thresholds, root-cause grouping, maintenance windows and ad-hoc silences.
---

Hora's alerting philosophy is **flapping never wakes you up**: probes are
retried before anything is recorded, alerts fire only after N consecutive
failures, a cascade folds into one notification, and recoveries of alerts
that were never sent stay silent too.

## Channels

Channels are **named**, so you can have several of the same type and route
each monitor to specific ones. Ten backends are built in:

| `type` | Notes |
| --- | --- |
| `telegram` | `token` + `chat_id` |
| `discord` | `webhook_url` |
| `slack` | `webhook_url` |
| `matrix` | `homeserver` + access `token` + `room_id` |
| `ntfy` | topic `url`, optional `token` for private servers |
| `gotify` | server `url` + application `token` |
| `pushover` | application `token` + `user` key |
| `email` | SMTP: `host`, `port` (587 STARTTLS default, `implicit_tls` for 465), `from`, `to` |
| `freemobile` | Free Mobile SMS: `user` + `pass` |
| `webhook` | POSTs a structured JSON event (`{ event, monitor, … }`) to `url` |

```toml
[[channels]]
name = "ops-telegram"
type = "telegram"
token = "${HORA_TELEGRAM_TOKEN}"
chat_id = "123456"

[[channels]]
name = "alerts-discord"
type = "discord"
webhook_url = "${DISCORD_WEBHOOK}"
```

An empty secret (an unset `${VAR}`) simply disables that channel. Delivery
retries transient failures, and down alerts include a snippet of the failing
response body.

**Routing**: a monitor (or a peer) selects channels with
`notify = ["ops-telegram"]`; without it, every configured channel is used.

**Test the chain before you need it**:

```sh
hora test-alert            # a labelled test down + recovered through every channel
hora test-alert website    # ... through exactly the channels routed for "website"
```

Any channel that fails logs a warning saying why ("chat not found", HTTP
403, ...).

### Channel watchdog

A channel that breaks — a revoked bot token, a dead SMTP relay, a deleted
Discord webhook — fails *silently* in the logs. You discover it during the
real incident, when the alert that should have paged you never arrived.
Hora counts consecutive delivery failures per channel and, at
`alerts.channel_fail_threshold` (default 3), alerts the **other** channels:

```
channel 'telegram' is failing — 3 consecutive delivery failures since 2h
```

The dead-man's switch applied to notifications themselves. Each delivery
already retries 3 times internally, so the default threshold represents 9
total failed attempts — enough to ride through a transient blip without
crying wolf. The alert fires once per failure streak (a flag prevents
re-spamming on every subsequent dispatch); a single successful delivery
resets the counter, so a channel that recovers and breaks again alerts
again. The failure counters survive a config reload, so touching an
unrelated setting does not silently forgive a channel that has been failing
for two days.

```toml
[alerts]
channel_fail_threshold = 3   # 1 = aggressive (alert on first failure)
```

Channel health is visible in three places:

- **`hora top`** — a yellow line per broken channel in the trouble panel.
- **`/api/summary`** (authenticated) — a `channels` array with `failing`,
  `consecutive_failures` and `failing_for_secs` per channel. The public
  status page never sees it: channel names and delivery health are the
  operator's business, not a client's.
- **`hora doctor`** — reports active vs. disabled-by-empty-secret channels,
  so a config with zero working channels is caught before the first
  incident.

## Confirmation threshold

```toml
[alerts]
fail_threshold = 3       # consecutive failures before a monitor is alerted down
alert_on_degraded = true # optional: also alert on degraded (same threshold)
```

A single failure shows the monitor as *degraded* on the page; only
`fail_threshold` consecutive failures confirm **down** and fire the alert.
Degraded alerts (up, but slower than the monitor's `degraded_over_ms`) are
opt-in and use the same anti-flap threshold.

Not sure what to set these to? [`hora tune`](../../reference/cli/#hora-tune)
replays your own history and recommends a `fail_threshold` and
`degraded_over_ms` per monitor - "with `fail_threshold = 5` you would have had
4 alerts instead of 11, same real outages, +40s to detect".

## Root-cause grouping

When a database takes ten services down with it, you get **one**
notification - the root cause, with its blast radius - not eleven. Dependent
monitors (via `depends_on`) confirmed down within the grouping window fold
into their upstream's alert, and their recoveries stay silent too. A monitor
that flaps entirely inside the window sends nothing at all.

```toml
[alerts]
group_window_secs = 30   # 0 restores one-alert-per-monitor
```

## Maintenance windows

Scheduled windows mute alerts for the affected monitors; checks keep being
recorded and the card shows a "maintenance" badge:

```toml
[[maintenance]]
title = "DB upgrade"
start = "2026-06-08T00:00:00Z"   # RFC 3339
end   = "2026-06-08T02:00:00Z"
monitors = ["database"]          # empty = all monitors
```

## Ad-hoc silences (deploy hooks)

The scriptable counterpart of a maintenance window - made for "mute while
deploying":

```sh
hora silence api,web 10m "deploying"   # CLI, straight into the database
hora silence list
hora silence clear
```

or from CI over HTTP:

```sh
curl -fsS -X POST -H "Authorization: Bearer $HORA_TOKEN" \
  "https://status.example.com/api/silence?monitors=api,web&duration=10m&reason=deploy"
```

Durations look like `10m`, `90s`, `1h30m` (max 7 days - anything longer
belongs in a visible maintenance window). Checks keep recording; only alert
transitions are muted, picked up on the next tick. The HTTP endpoint
**strictly requires** `server.auth_token`; unknown monitor ids are rejected
so a typo'd hook fails loudly instead of silencing nothing. Expired silences
are swept automatically.

## Pushed alerts (from your own jobs)

Probes tell you a service is *reachable*; they cannot tell you a batch job
wrote zero rows or a patch failed to apply. `POST /api/monitors/{id}/alert`
lets a producer push its **own** failure straight to a monitor's channels:

```sh
curl -fsS -X POST -H "X-Push-Token: $TOKEN" -H "Content-Type: application/json" \
  "https://status.example.com/api/monitors/ekb-api/alert" -d '{
    "severity":"error",
    "title":"Patch materialization failed",
    "message":"3/12 operations failed: SourceFile not found",
    "dedup_key":"ekb-api:materialization",
    "tags":{"task_id":"4711"}
  }'
```

The alert fans out to the monitor's `notify` channels immediately and adds a
line to that monitor's timeline (shown on `/history`) - but it **never marks
the monitor down**: status stays driven by probes and heartbeats alone, so a
producer's hiccup can't make your status page lie.

Two things Hora does for you here:

- **`dedup_key` + anti-flood.** A repeat of the same key within
  `alerts.push_alert_window_secs` (default 300) is coalesced - dropped and
  counted, not re-sent. The throttle lives in Hora, so a retrying job pages
  once and every producer benefits without writing its own rate-limiter.
- **`severity` → priority.** On ntfy, Pushover and Gotify the severity
  (`info`/`warning`/`error`/`critical`) maps onto the backend's native
  priority; elsewhere it is a text label, and the `webhook` channel gets it
  structured.

Authenticate with the monitor's `push_token` (`X-Push-Token`) or
`server.auth_token` (`Authorization: Bearer`). See the
[API reference](../../reference/api/#post-apimonitorsidalert) for the full
request/response shape.

## Weekly digest

The one notification that never signals a problem - a recap of the last
seven days, sent on a cron schedule through your channels:

```toml
[digest]
schedule = "0 8 * * 1"     # five-field cron, UTC (default: Monday 08:00)
notify = ["ops-telegram"]  # optional; default: every configured channel
```

```
99.97% overall, 2 incidents
- API: 99.99%, 1 incident, budget 41m of 43m left (30d)
- Web: 100.00%
```

One line per monitor: uptime, incidents in the window, and the error budget
left when an [SLO](../slo/) is configured. The last-sent timestamp persists
in the database, so a restart neither double-sends nor forgets - and a send
missed while the daemon was down catches up once. Preview the exact text
anytime with `hora digest` (a dry run; it notifies no one).

## Announcements

Banners pinned on the status page (and the per-group pages), independent of
any monitor - the mini-Statuspage half of self-hosted monitoring. Two ways
to pin one:

**Ad hoc**, from the CLI or a remote API call - made for "during the
incident":

```sh
hora announce "Fibre incident" "ETA 6pm" --severity warning --until 4h
hora announce list
hora announce clear
```

```sh
curl -X POST -H "Authorization: Bearer $TOK" \
  "https://status.example.com/api/announce?title=Fibre+incident&severity=warning&until=4h"
curl -X DELETE -H "Authorization: Bearer $TOK" "https://status.example.com/api/announce"
```

`--until` (a duration like `4h`, or `18:00` UTC) auto-expires the banner, so
the classic stale "incident ongoing" banner three days later cannot happen
by default. The API requires `server.auth_token` and the banner shows
immediately (the summary cache is busted on write).

**Declared in the config** - for planned, longer-lived notices, or a GitOps
workflow where announcements go through git:

```toml
[[incidents]]
title = "Investigating elevated latency"
body = "We are looking into reports of slow responses."
severity = "warning"             # info | warning | critical | resolved
at = "2026-06-07T12:00:00Z"
```

## TLS expiry warnings

`https://` monitors are warned `alerts.cert_expiry_days` before their
certificate expires (default 14), through the same channels and routing.
