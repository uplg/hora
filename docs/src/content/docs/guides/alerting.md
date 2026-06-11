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
| `webhook` | POSTs `{ event, monitor, message?, days_left? }` as JSON to `url` |

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

Manual banners on the status page, independent of any monitor:

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
