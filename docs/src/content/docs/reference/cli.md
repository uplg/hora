---
title: CLI
description: Every hora subcommand - check, test-alert, silence, tune, incidents, annotate, backup, import.
---

Plain `hora` (no arguments) runs the monitor. Everything else is a
subcommand that does its job and exits. Subcommands that touch the database
(`silence`, `incidents`, `annotate`, `event`, `postmortem`, `timeline`,
`tune`, `digest`, `report`) open the daemon's SQLite file directly -
run them on the same host, from the daemon's working directory (or with
`HORA_CONFIG` pointing at its config). They refuse to *create* a database, so
a wrong path fails loudly instead of operating on an empty file.

```sh
hora                                   # run the monitor
hora check [--strict]                  # validate the config; non-zero exit on error (--strict: or on a warning)
hora test-alert [monitor-id]           # send a test alert through the real chain
hora silence <ids|all> <duration> [reason]
hora silence list
hora silence clear
hora maintenance plan (--host <name> | --monitors <ids|all>) [--at <when>] --for <duration> <reason...>
hora maintenance list / cancel <id>    # planned maintenance windows
hora announce <title> [body] [--severity s] [--until 4h|18:00]
hora announce list / clear             # pinned status-page banners
hora top [--url U] [--token T] [--interval S]   # live terminal dashboard
hora digest                            # print the weekly digest (dry run)
hora report [YYYY-MM]                  # print the monthly SLA report (default: last month)
hora tune [monitor-id] [--days N]      # recommend fail_threshold / degraded_over_ms per monitor
hora probe <id|target> [--confirm]     # one-shot ad-hoc probe; --confirm asks the peers
hora doctor                            # diagnose the runtime environment
hora incidents [limit]                 # list recent incidents with their ids
hora annotate <id|last> "<note>"       # attach a note to an incident
hora event <title...>                  # record an event marker ("what changed?")
hora event list [limit]                # the recent event markers
hora postmortem <id|last>              # an incident's markdown post-mortem
hora timeline [--days N]               # downs, events, alerts, banners, silences, maintenance
hora peers diff                        # compare this node's monitors with each peer's
hora backup <dest.db>                  # consistent snapshot (VACUUM INTO)
hora compact [--dry-run]               # give free pages back to the disk (daemon stopped)
hora import kuma backup.json           # convert an Uptime Kuma backup (stdout)
hora --version
```

## `hora check`

Validates the configuration and exits non-zero on error - made for CI and
pre-deploy hooks. Validation is strict; see
[Configuration](../../configuration/#validation).

What the loader only warns about - the lines the daemon would log at startup
(a `${VAR}` that is not set, a channel disabled by an empty secret, a
`timeout_secs` longer than its interval, a channel over cleartext http) - is
printed on stderr,
one `warning: ...` line each, and counted in the verdict:

```text
$ hora check
warning: monitor api: timeout_secs (60) exceeds interval_secs (30) - a hung probe delays the next check by up to the timeout
config.toml is valid, 1 warning.
```

The exit code is still 0. `hora check --strict` exits non-zero when there is
at least one warning.

Run it in the environment the daemon runs in: a `${VAR}` that is not set
there is one of those warnings, and so is the channel it leaves disabled.
In the container it also warns, as the daemon does at startup, when the
config is mounted in a way that defeats
[live reload](../../configuration/#mount-the-directory-not-the-file): as a
single file, or from a network filesystem.

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
next tick. Ids are monitor ids or a watched peer's `listen_id`; unknown ids
are rejected with the configured list. The reason is capped at 500
characters. The same action
exists over HTTP as
[`POST /api/silence`](../api/#post-apisilence) for CI pipelines.

## `hora maintenance`

Plan a maintenance window while the daemon runs - the scheduled, visible
counterpart of a silence, and the runtime twin of a `[[maintenance]]`
window:

```sh
hora maintenance plan --host frank --at 2026-11-13T05:00:00Z --for 10m "kernel reboot"
hora maintenance plan --monitors api,web --for 30m "storage move"   # starts now
hora maintenance list                  # planned and active windows, the peers' included
hora maintenance cancel 3              # by the id `plan` and `list` print
```

`--host` covers every monitor labelled `host = "<name>"`; `--monitors`
takes ids (or a watched peer's `listen_id`), or `all`. Exactly one of the
two. `--for` is required (`10m`, `1h30m`, at most 7 days); the remaining
words are the reason, shown on the status page. `--at` takes an RFC 3339
time, unix seconds, or a UTC clock time such as `05:00` - its next
occurrence, meant for a person at a shell: a script should pass the full
timestamp, so that running it again plans the same window instead of
tomorrow's.

From its start to its end the window mutes alerts (checks keep being
recorded) and shows as *Planned maintenance*; before, the page announces
it as *Next*. A window for this node's own `[health] host`, or for named
monitors, is also announced to the peers on the next heartbeats: see
[planned maintenance of a host](../../guides/peers/#planned-maintenance-of-a-host).
A window announced by a peer cannot be cancelled from here. The same
actions exist over HTTP as
[`POST` / `GET` / `DELETE /api/maintenance`](../api/#post-apimaintenance).

## `hora announce`

Pins a public banner on the status page - see
[Announcements](../../guides/alerting/#announcements). `--until` takes a
duration (`4h`) or a UTC clock time (`18:00`, the next occurrence); without
it the banner stays until `hora announce clear`. A title that starts with
`list` or `clear` goes after `--`: `hora announce -- "clear skies tonight"`.

## `hora top`

A live terminal dashboard over the JSON API: per-monitor statuses, 24 h
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
  real multi-check outage went undetected. The replay starts from the
  monitor's own `fail_threshold` when it has one, and a recommendation that
  differs from it ends with where to write it: on that monitor (`to apply:
  fail_threshold = 2 in the [[monitors]] entry with id = "api"`), since
  `[alerts] fail_threshold` would change every other monitor too.
- **`degraded_over_ms`** - every up check carries its latency sample, so `tune`
  reports the distribution (p50/p95/p99/max) and recommends a threshold near
  p99, warning when the current one flags normal traffic as "degraded".

```
Uplg  (http, every 1 min)
  43182 checks, 2026-05-14 19:20:00 UTC -> 2026-06-13 19:20:00 UTC, 31 down  [fail_threshold=3]
  failure runs: 11  (23, 6, 2, 2, 1, 1, 1, 1, 1, 1, 1)
  fail_threshold   alerts   detect after
      1               11       1 min
      2                4       2 min
    * 3                2       3 min  (current)
      4                2       4 min
      5                2       5 min
  -> fail_threshold 3 looks right: flaps (runs <= 2) filtered, longest outage (23 checks) still caught
  latency, up checks: p50 118 ms  p95 124 ms  p99 254 ms  max 8812 ms  [degraded_over_ms=1500]
    1500 ms flags 1 of 5668 up checks (0.0 %); ~p99 is 300 ms
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
hora probe api.example.com --method HEAD --expect 200,401  # an ad-hoc http check
```

A bare argument matching a configured monitor **id** is probed with that
monitor's exact config (assertions, cert pin, proxy, dns expectation). Anything
else is an **ad-hoc target** whose kind is inferred - a URL is `http`, an
explicit `host:port` is `tcp`, a bare hostname is an `https` check (a name
usually denotes a web service, and ICMP is widely filtered), a bare IP is an
`icmp` ping - or set with `--kind http|tcp|icmp|dns`. The probe exits non-zero
when the target is down, so it doubles as a scriptable health check
(`hora probe url && deploy`).

An ad-hoc `http` target takes `--method GET|HEAD` and `--expect`, the
monitor's `method` and `expected_status` (statuses or classes separated by
commas: `200,401`, `2xx,401`): try a `HEAD` or a list of statuses against a
service before writing the monitor. With a configured id both are refused:
the monitor is probed as it is configured.

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
`net.ipv4.ping_group_range` catch), a real system-resolver lookup, and the
configured notification channels (active vs. disabled by an empty secret —
a config with zero working channels is the one class of problem the
watchdog itself can't warn about, since it needs at least one working
channel to reach you), and, on Linux, live reload (`reload`: a warning when
the config is bind-mounted as a single file, which stops following once the
file is replaced on the host, or sits on a network filesystem, where a
change waits for the re-read every 60 s).
Failures are judged against the config - no IPv6 route only fails when a
`dual_stack` monitor needs one - and the process exits non-zero on any
missing needed capability.

## `hora incidents`

Lists recent incidents (default 20) with their ids - the lookup companion of
`annotate`:

```
#42  API  2026-06-10 03:12:04 UTC -> 2026-06-10 03:15:10 UTC (3 min 6 s)
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

## `hora event`

Records an event marker, the answer to *"what changed?"*: shown on the
latency charts and `/history`, and named in the alert of any monitor that
confirms down within the hour after it.

```sh
hora event "deploy api v2.3"
hora event list               # the 20 most recent (or: hora event list 50)
hora event -- "list redesign" # a title that starts with 'list'
```

The HTTP twin is `POST /api/event`. See
[Event markers](../../guides/incidents/#event-markers-what-changed).

## `hora postmortem`

Prints an incident's auto-generated post-mortem as markdown: first failure,
what the service answered, the multi-vantage verdict, cause and impact, the
correlated change, your note, and the timeline. `last` targets the most
recent incident. The web twin is `/incident/{id}`.

```sh
hora postmortem 42 > incident-42.md
```

## `hora timeline`

One chronology, newest first: downs and recoveries, event markers, pushed
alerts, announcements and silences. Seven days by default:

```sh
hora timeline --days 30
```

## `hora peers diff`

Compares this node's probeable monitors (kind and target; push and exec
monitors have none) with each peer's, over the authenticated mesh exchange.
[Multi-vantage confirmation](../../guides/peers/#multi-vantage-confirmation)
only works for monitors both nodes know, and this is what checks it. Exits
non-zero on any difference or an unreachable peer, so it can gate a config
deploy. Needs `[health].id` and peers with a `ping_url`. Targets are compared
with the secrets of their URLs masked (`user:pass@`, query values), as peers
list them: two monitors that differ only by a credential are one target.

A target both nodes watch with different assertions (another expected
status or list of them, another `method`, a keyword on one side only,
another `Host` header) is reported as
`same target, checked differently`: the peer would refuse to confirm a down
of a check it does not run. A 0.11 peer does not say what it checks, and is
compared on targets alone.

## `hora backup`

Snapshots the database with SQLite's `VACUUM INTO`: consistent and compacted,
safe while the daemon is writing. The source is opened read-only (a backup
never creates or migrates a database), an existing destination is refused,
and the snapshot is created owner-only (0600) like the live database. A
one-liner in a cron job pointed at a NAS mount:

```sh
hora backup /mnt/nas/hora-$(date +%F).db
```

## `hora compact`

SQLite never shrinks its file by itself: rows deleted by retention leave
free pages that later inserts reuse, but the file keeps its high-water
mark. `hora compact` gives that space back.

```sh
hora compact --dry-run        # measure: every table and index, free pages, the gain, free disk
hora compact                  # apply the retention now, rewrite the file, swap it in
hora compact --purge-removed  # also delete monitors removed from the config, without the 7-day grace
hora compact --keep           # keep the original next to it as <db>.bak
```

A real run needs the database to itself: **stop the daemon first**. The
daemon holds `<db>.lock` while it runs, so `hora compact` refuses rather
than rewriting a file in use, and other commands (`announce`, `event`,
`silence`...) refuse while a compaction runs. `--dry-run` only reads, so it
works while the daemon runs, but it reads the whole file: allow minutes on a
database of tens of gigabytes.

The rewrite needs free disk for a copy of the pages in use (it checks
first), then swaps the copy in atomically. On a large, long-lived database,
expect it to reclaim about a fifth of the file. `hora top` shows the
database size, and suggests a compaction once more than a fifth of it is
free pages.

## `hora import kuma`

Converts an Uptime Kuma backup JSON to Hora monitors on stdout. See
[Importing from Uptime Kuma](../../guides/import/).
