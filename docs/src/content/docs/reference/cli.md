---
title: CLI
description: Every hora subcommand - check, test-alert, silence, tune, incidents, annotate, backup, import.
---

Plain `hora` (no arguments) runs the monitor. Everything else is a
subcommand that does its job and exits. Subcommands that touch the database
(`silence`, `incidents`, `annotate`) open the daemon's SQLite file directly -
run them on the same host, from the daemon's working directory (or with
`HORA_CONFIG` pointing at its config). They refuse to *create* a database, so
a wrong path fails loudly instead of operating on an empty file.

```sh
hora                                   # run the monitor
hora check                             # validate the config; non-zero exit on error
hora test-alert [monitor-id]           # send a test alert through the real chain
hora silence <ids|all> <duration> [reason]
hora silence list
hora silence clear
hora announce <title> [body] [--severity s] [--until 4h|18:00]
hora announce list / clear             # pinned status-page banners
hora top [--url U] [--token T]         # live terminal dashboard
hora digest                            # print the weekly digest (dry run)
hora report [YYYY-MM]                  # print the monthly SLA report (default: last month)
hora tune [monitor-id] [--days N]      # recommend fail_threshold / degraded_over_ms per monitor
hora probe <id|target> [--confirm]     # one-shot ad-hoc probe; --confirm asks the peers
hora doctor                            # diagnose the runtime environment
hora incidents [limit]                 # list recent incidents with their ids
hora annotate <id|last> "<note>"       # attach a note to an incident
hora backup <dest.db>                  # consistent snapshot (VACUUM INTO)
hora import kuma backup.json           # convert an Uptime Kuma backup (stdout)
hora --version
```

## `hora check`

Validates the configuration and exits non-zero on error - made for CI and
pre-deploy hooks. Validation is strict; see
[Configuration](../../configuration/#validation).

## `hora test-alert`

Sends a clearly-labelled test alert (a down, then its recovery) through the
**real** dispatch path, so delivery is verified before the first real
incident instead of during it. Without an id every configured channel is
exercised; with one, exactly the channels that monitor's `notify` routing
would fire. A failing channel logs a warning with the rejection detail
("chat not found", HTTP 403, ...) and the command **exits non-zero**, naming
the failing channels - made for CI. An unknown id lists the configured ones.

## `hora silence`

Ad-hoc alert muting - the scriptable counterpart of a `[[maintenance]]`
window, made for deploy hooks:

```sh
hora silence api,web 10m "deploying"   # comma-separated ids, or 'all'
hora silence list                      # active silences, soonest-expiring first
hora silence clear                     # remove every silence
```

Durations look like `90s`, `10m`, `1h30m` (max 7 days). Checks keep being
recorded; only alert transitions are muted, picked up by the daemon on its
next tick. Unknown ids are rejected with the configured list. The same action
exists over HTTP as
[`POST /api/silence`](../api/#post-apisilence) for CI pipelines.

## `hora announce`

Pins a public banner on the status page - see
[Announcements](../../guides/alerting/#announcements). `--until` takes a
duration (`4h`) or a UTC clock time (`18:00`, the next occurrence); without
it the banner stays until `hora announce clear`.

## `hora top`

A live terminal dashboard over the JSON API: per-monitor statuses, 24h
uptime, p50/p95/p99, a latency sparkline for the selected monitor (arrow
keys), and the current trouble. `--url` and `--token` point it at any Hora -
local or remote (`HORA_TOKEN` works too, keeping the token out of `ps`);
without `--url` the local config's bind address is used, so `hora top` just
works on the daemon's host - including inside `docker exec -it hora hora
top`.

It also *acts*, through the same authenticated API (`--token` required):

| Key | Action |
| --- | --- |
| `a` | Pin an announcement: `title :: body`, optional `--severity` / `--until` |
| `s` | Silence the selected monitor (pre-filled `10m`, editable) |
| `C` | Clear every pinned announcement |
| `↑`/`↓` | Select (the sparkline follows, debounced) |
| `r` / `q` | Force a refresh / quit |

Pinned banners show in the trouble panel, so what you announce is what you
see. Selection scrolling is debounced and a server 429 backs the polling
off, so `hora top` stays comfortably within the default API rate limit.

## `hora digest`

Prints the weekly digest exactly as the `[digest]` task would send it - a
dry run to check the wording (and the data) without notifying anyone. See
[Weekly digest](../../guides/alerting/#weekly-digest).

## `hora report`

Prints the monthly SLA report as text - the terminal twin of the printable
[`/report/{month}` page](../../guides/multi-tenant/#monthly-sla-reports).
Defaults to last month.

## `hora tune`

Replays the stored check history against alternative anti-flap settings and
recommends, per monitor, what to change - the question no light monitor helps
with: *is this monitor set up right?* It is pure read-only analytics over data
that already exists (the raw `checks` table, which survives the full retention
window - 90 days by default), so it never probes and never writes.

```sh
hora tune                  # every monitor that has history
hora tune api              # just one monitor
hora tune api --days 30    # ... over the last 30 days (default: the retention window)
```

Two settings are replayable from the recorded check sequence:

- **`fail_threshold`** - because the per-check status sequence is exactly what
  the scheduler's down state machine sees, `tune` can count how many down
  alerts each candidate threshold would have fired and the detection delay it
  costs, then recommend the value that sits just above the *flap cluster* (the
  widest gap in the failure-run lengths): flaps are filtered, every real outage
  is still caught. It also flags the opposite mistake - a threshold so high a
  real multi-check outage went undetected.
- **`degraded_over_ms`** - every up check carries its latency sample, so `tune`
  reports the distribution (p50/p95/p99/max) and recommends a threshold near
  p99, warning when the current one flags normal traffic as "degraded".

```
Uplg  (http, every 60s)
  43182 checks, 2026-05-14 19:20:00 UTC -> 2026-06-13 19:20:00 UTC, 31 down  [fail_threshold=3]
  failure runs: 11  (23, 6, 2, 2, 1, 1, 1, 1, 1, 1, 1)
  fail_threshold   alerts   detect after
      1               11         1m 0s
      2                4         2m 0s
    * 3                2         3m 0s  (current)
      4                2         4m 0s
      5                2         5m 0s
  -> fail_threshold 3 looks right: flaps (runs <= 2) filtered, longest outage (23 checks) still caught
  latency, up checks: p50 118ms  p95 124ms  p99 254ms  max 8812ms  [degraded_over_ms=1500]
    1500ms flags 1 of 5668 up checks (0.0%); ~p99 is 300ms
```

`probe_retries` is deliberately **not** replayed: only a probe's final attempt
reaches the database, so retries are invisible here. The command says so and
surfaces the single-check-blip count instead - the cross-tick lever for those
is `fail_threshold`, which it *does* replay. See
[Configuration](../../configuration/) for the settings themselves.

## `hora probe`

A single ad-hoc check from the terminal, with the full monitor semantics:
status, latency, status code, HTTP/DNS assertions, and TLS expiry for
`https://` targets. It never touches the database - just a live probe and its
result.

```sh
hora probe api.example.com           # bare hostname -> an https check
hora probe db.example.com:5432       # host:port -> a tcp connect
hora probe 192.0.2.10                # a bare IP -> an icmp ping
hora probe api                       # a configured monitor id -> its exact config
hora probe api.example.com --kind tcp  # force a kind for an ad-hoc target
```

A bare argument matching a configured monitor **id** is probed with that
monitor's exact config (assertions, cert pin, proxy, dns expectation). Anything
else is an **ad-hoc target** whose kind is inferred - a URL is `http`, an
explicit `host:port` is `tcp`, a bare hostname is an `https` check (a name
usually denotes a web service, and ICMP is widely filtered), a bare IP is an
`icmp` ping - or set with `--kind http|tcp|icmp|dns`. The probe exits non-zero
when the target is down, so it doubles as a scriptable health check
(`hora probe url && deploy`).

With **`--confirm`** it also asks the configured
[peers](../../guides/peers/#multi-vantage-confirmation) to probe the same
target and prints the multi-vantage verdict from this node's perspective, both
ways:

```
hora probe - API (http, https://api.example.com)

  status    DOWN
  latency   -
  error     connection failed
  vantage   seen UP by hora-b - down from 1/2 vantage points (network issue near this node?)
```

A local **down** answers *"down for everyone or just me?"* (`confirmed down
from 3/3`, or `seen UP by hora-b …`); a local **up** answers *"is it up from
elsewhere too?"* (`up from 3/3 vantage points`, or `up here, but seen DOWN by
hora-b …`). Confirmation needs `[health].id` and `[[peers]]` with a `ping_url`;
the peer only answers for a target in *its own* config, so the two nodes must
share the monitor (which is what putting the config in git gives you).

## `hora doctor`

Diagnoses the runtime environment against what the configuration needs -
`hora check` says the config is sound, `hora doctor` says the *host* can
honour it. Checks: database writable, listen port free (busy is a warning -
the daemon is probably just running), IPv4/IPv6 routes (no packets sent),
the unprivileged ICMP datagram socket (the rootless-Docker
`net.ipv4.ping_group_range` catch), and a real system-resolver lookup.
Failures are judged against the config - no IPv6 route only fails when a
`dual_stack` monitor needs one - and the process exits non-zero on any
missing needed capability.

## `hora incidents`

Lists recent incidents (default 20) with their ids - the lookup companion of
`annotate`:

```
#42  API  2026-06-10 03:12:04 UTC -> 2026-06-10 03:15:10 UTC (3m 6s)
      error: HTTP 503: upstream database unreachable
      answered: HTTP/2.0 503 Service Unavailable
      note:  fiber cut
```

## `hora annotate`

Attaches a free-form operator note to an incident, shown on `/history` and in
the Atom feed:

```sh
hora annotate 42 "fiber cut, ETA 6pm"
hora annotate last "fiber cut"     # the most recent incident
hora annotate 42 ""                # an empty note clears it
```

## `hora backup`

Snapshots the database with SQLite's `VACUUM INTO`: consistent and compacted,
safe while the daemon is writing. The source is opened read-only (a backup
never creates or migrates a database), an existing destination is refused,
and the snapshot is created owner-only (0600) like the live database. A
one-liner in a cron job pointed at a NAS mount:

```sh
hora backup /mnt/nas/hora-$(date +%F).db
```

## `hora import kuma`

Converts an Uptime Kuma backup JSON to Hora monitors on stdout. See
[Importing from Uptime Kuma](../../guides/import/).
