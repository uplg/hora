---
title: CLI
description: Every hora subcommand - check, test-alert, silence, incidents, annotate, backup, import.
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
top`. `q` quits, `r` forces a refresh.

## `hora digest`

Prints the weekly digest exactly as the `[digest]` task would send it - a
dry run to check the wording (and the data) without notifying anyone. See
[Weekly digest](../../guides/alerting/#weekly-digest).

## `hora report`

Prints the monthly SLA report as text - the terminal twin of the printable
[`/report/{month}` page](../../guides/multi-tenant/#monthly-sla-reports).
Defaults to last month.

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
