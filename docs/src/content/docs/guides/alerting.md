---
title: Alerting & notifications
description: Channels, routing, thresholds, root-cause grouping, maintenance windows (configured or planned at runtime) and ad-hoc silences.
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
| `webhook` | POSTs a structured JSON event (`{ event, monitor, … }`) to `url` (see below) |

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

An empty secret (an unset `${VAR}`) disables that channel; the others keep
working. It is never silent: the load warns about the variable and about the
channel by name, `hora check` prints both (and `hora check --strict` fails
on them), and `hora doctor` lists the channel as disabled. Delivery retries
transient failures, and down alerts include a snippet of the failing
response body.

On Gotify, ntfy and Pushover the notification's title is the event's headline
("API is DOWN") and its message the detail, so a phone's notification list
names the monitor on every line.

Every channel renders the same neutral message (`API is DOWN`,
`API is slow (812 ms)`, `API recovered`) and only adds its own markup and
priority, so the wording never drifts between channels. Email subjects read
`[DOWN] API is DOWN`. A message longer than a service accepts (Discord,
Telegram, Pushover, ntfy...) is cut to its limit with an ellipsis rather
than refused. A degraded alert says why when it knows: the `msg` of a push
with `status=degraded`, or the WARNING line of an exec plugin.

Requests to notification services, webhooks and peers follow a redirect
only within the same origin (or from http to https on the same host), so a
token is never carried to another host. A webhook URL that redirects
elsewhere fails delivery.

**Routing**: a monitor (or a peer) selects channels with
`notify = ["ops-telegram"]`; without it, every configured channel is used.
A down seen from this node only, while the peers see the target up, is
flagged as local-only; it goes to `alerts.notify_unconfirmed` instead when
that key is set (an empty list: nowhere). See
[local-only downs](../peers/#local-only-downs).

**The webhook payload** is a JSON object with `event` (`down`, `degraded`,
`recovered`, `cert_expiring`, `cert_changed`, `cert_unreadable`,
`domain_expiring`, `release_available`, `peer_link_degraded`,
`budget_burn`, `digest`, `alert`) and `monitor`, plus the fields that event
carries: `message`, `cause`, `impacted`, `vantage` and `change` on a down;
`local_only` (bool) on `down` and `recovered`; `days_left` and `secs_left`
on expiry events (`days_left` is rounded down: 0 during the last day, -1
once the expiry has passed; `secs_left` is the exact figure, negative once
expired); the fingerprints on `cert_changed`; the read error as `message` on
`cert_unreadable`. Fields are only ever added: ignore the ones you do not
know.

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
channel 'telegram' is failing — 3 consecutive delivery failures since 2 h
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

The value in `[alerts]` is the default. A monitor that needs another sets
its own:

```toml
[[monitors]]
id = "payments"
# ...
fail_threshold = 1       # alert on the first failed check
```

The page and the badge follow the same value, so a monitor shows **down**
exactly when its alert goes out. Watched [peers](../peers/) keep the global
one. See [a monitor's own `fail_threshold`](../monitors/#a-monitors-own-fail_threshold).

Not sure what to set these to? [`hora tune`](../../reference/cli/#hora-tune)
replays your own history and recommends a `fail_threshold` and
`degraded_over_ms` per monitor - "with `fail_threshold = 5` you would have had
4 alerts instead of 11, same real outages, +40 s to detect" - and says where
to set it: on that monitor.

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
recorded, and the status page shows them in maintenance (and the day in
their daily bar):

```toml
[[maintenance]]
title = "DB upgrade"
start = "2026-06-08T00:00:00Z"   # RFC 3339
end   = "2026-06-08T02:00:00Z"
monitors = ["database"]          # empty = all monitors
```

`monitors` takes monitor ids and the `listen_id` of watched
[peers](../peers/), so a planned reboot of another node stays quiet too.

### Planned at runtime

The same window can be planned while the daemon runs, with no config edit
and no reload - for a start time only a script knows (a reboot scheduled by
unattended upgrades):

```sh
hora maintenance plan --monitors api,web --at 2026-11-13T05:00:00Z --for 10m "storage move"
hora maintenance plan --host frank --for 10m "kernel reboot"   # every monitor with host = "frank"
hora maintenance list
hora maintenance cancel 3
```

or over HTTP, with the operator token:

```sh
curl -fsS -X POST -H "Authorization: Bearer $HORA_TOKEN" \
  "https://status.example.com/api/maintenance?monitors=api,web&starts_at=2026-11-13T05:00:00Z&duration=10m&reason=storage%20move"
```

It is a maintenance window like the config's, from its start to its end:
alerts muted, checks recorded, *Planned maintenance* on the status page
(and *Next* before it starts), the day marked in the daily bars and in the
monthly report, a line in the timeline. Unlike a silence it is planned
ahead, has a reason the visitors read, and is announced.

A window lasts at most 7 days and starts at most a year ahead. Planning
the same one again (same selector, start and end) returns the one already
planned. With `host = "<name>"` on your monitors, `--host` covers all the
services of a machine at once; and a node tells its
[peers](../peers/#planned-maintenance-of-a-host) about the windows of its
own host, so the node that watches it stays quiet too. When a window ends
and a monitor is still down, the down is confirmed and announced from
there.

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

Durations look like `10m`, `90s`, `1h30m` (max 7 days: anything longer
belongs in a visible maintenance window). Checks keep recording; only alert
transitions are muted, picked up on the next tick. The ids are monitor ids
or the `listen_id` of a watched [peer](../peers/) (by default its `id`), so
`hora silence hora-b 30m "rebooting hora-b"` mutes that peer's dead-man
alert. The HTTP endpoint **strictly requires** `server.auth_token`, sent as
`Authorization: Bearer`; unknown ids are rejected so a typo'd hook fails
loudly instead of silencing nothing. Expired silences are swept
automatically.

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
`server.auth_token` (`Authorization: Bearer`) - in a header, never as
`?token=`. See the
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
99.97 % overall, 2 incidents
- API: 99.99 %, 1 incident, budget 41 min of 43 min left (30 d)
- Web: 100.00 %
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

`--until` (a duration like `4h`, or a time of day like `18:00`, UTC, the
next occurrence) auto-expires the banner, so the classic stale "incident
ongoing" banner three days later cannot happen by default. The API's
`until` takes the same two forms (`until=4h` or `until=18:00`). The API
requires `server.auth_token` and the banner shows immediately (the summary
cache is busted on write).

A title that starts with `list` or `clear` needs `--` before it:
`hora announce -- "clear skies tonight"`. Without it, `hora announce clear`
removes every banner.

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
Under two days the warning counts hours (*expires in 10 h*), and minutes
in the last hour. A proxied
monitor's certificate is read through its proxy (HTTP `CONNECT`, `socks5`,
`socks5h`), like its probe. A certificate that cannot be read at all (the
STARTTLS dialogue changed, a server down to TLS 1.0, a firewall) alerts
once, *TLS certificate could not be read*, with the error - not while the
monitor is down, and not again until it has been read successfully.
