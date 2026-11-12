---
title: Configuration
description: The config file, environment interpolation, live reload, and validation.
---

Everything lives in one TOML file, read from `$HORA_CONFIG` (default
`./config.toml`). The annotated
[`config.example.toml`](https://github.com/uplg/hora/blob/main/config.example.toml)
documents every option; this page covers the mechanics.

## Sections

| Section | What it holds |
| --- | --- |
| `[page]` | Status page title, days rendered in the uptime bars |
| `[server]` | Bind address, database path, CORS, rate limits, the operator token (`auth_token`), the read-only token (`view_token`), group tokens |
| `[[channels]]` | Named notification channels (Telegram, Discord, email, ...) |
| `[alerts]` | Fail threshold, cert expiry warning, retention, alert grouping, channel watchdog, where local-only downs go (`notify_unconfirmed`) |
| `[[maintenance]]` | Scheduled windows that mute alerts |
| `[[incidents]]` | Manual announcements shown as a status page banner |
| `[[monitors]]` | The monitors themselves |
| `[health]` / `[[peers]]` | Mutual surveillance ([dead-man's switch](../guides/peers/)) |

## Secrets stay in the environment

Any `${VAR}` in the file is replaced with the environment variable at load:

```toml
[[channels]]
name = "ops"
type = "telegram"
token = "${HORA_TELEGRAM_TOKEN}"
chat_id = "123456"
```

A variable that is not set expands to nothing and is **warned about**, with
the key that uses it (`channels[0].token: environment variable
HORA_TELEGRAM_TOKEN is not set ...`): the load does not fail, since the
config can be fixed and reloaded while Hora runs.

| Written | Means |
| --- | --- |
| `${VAR}` | the variable's value; nothing, with a warning, when it is not set |
| `${VAR:-fallback}` | the value, or `fallback` when the variable is unset or empty |
| `${VAR:-}` | the value, or nothing, without a warning: the way to say a value is optional |
| `$${` | a literal `${` (in a keyword or a body assertion) |

The fallback is plain text up to the first `}`: it is not expanded itself and
cannot contain `{` or `}`. Only string values are expanded, never keys or
comments. A `${...}` that is not a variable name, or whose fallback holds a
`{`, is left as written, with a warning; so is warned about a variable whose
value is not valid Unicode (it expands to nothing).

A channel whose token or URL is empty **is disabled** rather than
half-configured - the others keep working - and the load says so (`channel
ops: token is empty, the channel is disabled until it is set`);
[`hora doctor`](../reference/cli/#hora-doctor) lists it as disabled.
[`hora check`](../reference/cli/#hora-check) prints and counts all of these
warnings, and `hora check --strict` exits non-zero on them. An empty *access
token* (`auth_token`, `view_token`, a group token, `push_token`,
`listen_token`, `ping_token`) fails startup loudly instead of silently
meaning "no token required".

## Access tokens

| Key | What it opens |
| --- | --- |
| `server.auth_token` | The operator: private monitors, the operator's streams, and every write endpoint (silence, maintenance, announce, event, alert). |
| `server.view_token` | Read-only: every monitor, private ones and their failure detail included, on the read-only views. No write, no event markers, no silences, no channel health, no `/watchers`. |
| `[server.group_tokens]` | One group's full view on `/status/{group}` and its report. See [multi-tenant pages](../guides/multi-tenant/). |
| A monitor's `push_token` | Heartbeats and pushed alerts for that monitor. |
| A peer's `listen_token` | That peer's heartbeats and probe requests. |

```toml
[server]
auth_token = "${HORA_AUTH_TOKEN}"   # keep it for scripts and yourself
view_token = "${HORA_VIEW_TOKEN}"   # the one for a wall display's ?token= URL
```

Put `view_token`, not `auth_token`, in any URL that leaves your hands: a
kiosk, a bookmark, a colleague. If it leaks it shows the page; it cannot
mute an alert or pin a banner. The two must differ.

Three rules hold for all of them, checked at load:

- **At least 16 characters.** A shorter `auth_token`, `view_token`, group
  token, `push_token` or `listen_token` fails startup
  (`openssl rand -hex 16` makes one). A `ping_token` is the receiver's
  secret, so a short one only warns.
- **A push monitor sets a `push_token`, a watched peer a `listen_token`.**
  Their ids are public (the page, the API, `/healthz`), so the id alone
  must not be enough to fake a heartbeat and hide an outage. On a network
  where everyone who can reach `/api/push` is trusted, the explicit opt-out
  is `allow_unauthenticated = true` on that monitor or peer; Hora then
  warns at each load.
- **Write endpoints take the token in a header only.** `?token=` works on
  the read-only views; on a write it is refused. See
  [Authentication](../reference/api/#authentication).

## Live reload - no blind window

To add, remove or change a monitor **without downtime**, just edit the config.
Hora applies the change by itself, about a second after the file stops
changing, and three things tell it to look:

- **File notifications.** Hora watches the config's *directory* and reacts to
  whatever happens to the file's name there, so every way of saving works:
  written in place, or written to a temporary file and renamed over the old
  one (Ansible's `template` module, most editors, `sed -i`). A symlinked
  config is followed through its links, which covers a Kubernetes
  `ConfigMap` (updated by swapping the `..data` link). Other files in the
  directory - the database, when it sits next to the config - wake nothing.
- **A re-read every 60 s**, whatever the notifications said. This is what
  applies a change where there are none: a network filesystem (NFS, SMB, 9p,
  sshfs) edited from another machine, some FUSE and overlay setups. It costs
  one read of the file a minute; a text that has not changed is not parsed.
- **SIGHUP**, to apply it now: `kill -HUP <pid>` - in Docker,
  `docker kill -s HUP hora`.

### Mount the directory, not the file

In a container, mount the directory that holds the config
(`./config:/etc/hora`), never the file alone
(`./config/config.toml:/etc/hora/config.toml`). A file mounted on its own is
pinned to the file that existed when the container started: the first time
a tool replaces it on the host by renaming - which is how Ansible, most
editors and `git checkout` write - the container keeps reading the old one,
and neither the re-read nor SIGHUP can reach the new one before a restart.
Only an edit made in place still arrives.

Hora says so instead of silently not reloading: on Linux the daemon logs one
warning at startup when the config is a single-file mount or sits on a
network filesystem, `hora check` prints the same line among its warnings,
and [`hora doctor`](../reference/cli/#hora-doctor) reports it as `reload`.

On reload, unchanged monitors keep running untouched; only new, removed or
changed ones are started or stopped, and the notification channels are
rebuilt - adding a Telegram token takes effect live too. Existing checks never
pause, so there is no window where nothing is watching.

A few `[server]` settings are read once at startup and need a restart:
`bind`, `allowed_origins`, the rate limit (`rate_limit_burst`,
`rate_limit_refill_secs`), `client_ip_header`, `max_connections` and
`max_connections_per_ip`.

### Connections and memory

Hora serves a bounded number of HTTP connections at once, so that visitors
can never take the file descriptors its checks need:

```toml
[server]
max_connections = 1024         # the default
max_connections_per_ip = 256   # the default: a quarter of the above
```

One client address holds a quarter of the connections at most, so that a
single machine cannot keep the heartbeats of your jobs and peers from
getting in; its further connections are closed. An IPv6 client is counted
by its /64. Loopback is never limited, and neither is anyone when
`client_ip_header` is set: every connection then comes from your proxy.
`max_connections_per_ip = 0` turns the limit off.

Unset, the limit is 1024, fewer when the memory limit is small (a quarter
of it at 128 KiB a connection: 128 connections in a 64 MiB container), and
in every case at most half of the open-files limit. The startup log line
says the number in force. Connections beyond it wait in the kernel's queue;
a connection that stalls (no request, a request that never ends, a client
that stops reading) is closed after 30 to 60 seconds, an idle keep-alive
one after three minutes.

If the host itself runs out of descriptors or memory, checks are skipped
rather than recorded as failures, and the node reports itself degraded on
`/healthz` until they run again.

As a guide, the image with 300 monitors and a year of history uses about
18 MiB at rest and 45 MiB at the worst of a connection flood: a 64 MiB
limit is comfortable. A small fleet (ten monitors) stays under 20 MiB.

A reload that fails validation is refused: Hora logs why and keeps running
with the previous config.

## Validation

```sh
hora check
hora check --strict     # a warning fails the check too
```

validates the configuration and exits non-zero on error - made for CI and
pre-deploy hooks. What the loader only warns about is printed and counted
("config.toml is valid, 2 warnings."): a `${VAR}` that is not set and a
channel disabled by an empty secret are among them. Validation is strict: inverted maintenance windows, cyclic
`depends_on` graphs, private monitors without a viewer token, short or empty
tokens and malformed cron schedules are all rejected at load, not discovered
at 3 a.m.

### Bounds

Values that cannot work are refused too:

| Setting | Accepted |
| --- | --- |
| `interval_secs`, `timeout_secs`, `expect_every_secs`, `health.interval_secs` | at most 30 days (2592000) |
| `expected_status` | statuses from 100 to 599 and classes `"1xx"` to `"5xx"`; a list is not empty |
| `method` | `"GET"` or `"HEAD"`, on an http monitor; no `"HEAD"` with `keyword`, `json_query` or `number_regex` |
| `fail_threshold` on a monitor | at least 1 |
| An IPv6 `tcp` target | bracketed: `[::1]:80`, not `::1:80` |
| `dns_resolver` | an IP and a port (`8.8.8.8:53`, `[2620:fe::fe]:53`), not a hostname |

A `timeout_secs` longer than `interval_secs` is accepted with a warning
(which `hora check` prints, and `hora check --strict` refuses).

Before upgrading, run `hora check` with the **new** binary against your
config: a key or a value an older release accepted may be refused now (see
[Upgrading](../upgrading/)).

## Retention and downsampling

Raw checks are kept per monitor (`retention_days`, default
`alerts.default_retention_days = 90`). Alongside them, every ended hour is
rolled up into an hourly bucket; hourly buckets become daily ones after 90
days, kept for a year. The daily
uptime bars keep working beyond the raw retention window, and the database
never grows forever. Closed incidents age out after a year; expired silences
are swept too. The prune runs every 6 hours (the first one 5 minutes after
start) and deletes five minutes of checks per statement, pausing in
between, so the checks being recorded never wait on it. A tick spends at
most two minutes deleting: after lowering a retention a lot, the backlog is
worked off over the following ticks, five minutes apart, with the hourly
roll-ups in between. The WAL file
(`<db>-wal`) is truncated back to 64 MiB after each checkpoint.

### Removed monitors

Taking a monitor out of the config does not delete its history at once.
Hora notes when the id disappeared, logs a warning that names it and the
date its data will go, and deletes its checks, aggregates and incidents
**7 days** later. Put the id back within the week (after a rename, or a
monitor commented out for an afternoon) and everything is kept. Watched
peers are treated the same way, by their `listen_id`.

For backups, `hora backup <dest>` snapshots the live database with SQLite's
`VACUUM INTO` - see the [CLI reference](../reference/cli/#hora-backup).
