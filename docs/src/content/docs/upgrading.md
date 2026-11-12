---
title: Upgrading
description: How to upgrade Hora, what to check before the next release, and where the version notes live.
---

Hora stores everything in one SQLite file; schema migrations are embedded in
the binary and applied automatically on startup. Upgrading is: replace the
binary (or image), restart.

## Knowing a new release is out

Once a day, Hora asks GitHub for its latest release (one anonymous request
to `api.github.com`; nothing about your instance is sent). When it is newer
than the one you run:

- the status page tells you, above everything, when you open it with the
  operator token (`server.auth_token`): the version, the opening of its
  changelog, and links to the release notes and to the upgrade notes.
  Visitors never see it, and it goes once you run that version;
- one message goes through your channels, the same day, once per release
  (a restart does not repeat it).

```toml
[updates]
check = true               # false: never ask, no banner, no message
notify = ["ops-telegram"]  # default: every channel; [] = the banner alone
```

## Docker

```sh
docker pull ghcr.io/uplg/hora:latest
docker stop hora && docker rm hora
docker run -d --name hora --restart unless-stopped \
  -p 8787:8787 \
  -v "$PWD/hora-config:/etc/hora" \
  -v hora-data:/data \
  ghcr.io/uplg/hora:latest
```

Your history lives on the `hora-data` volume and survives upgrades.

## Before you upgrade

A consistent snapshot of the database is one command, safe while the daemon
runs:

```sh
hora backup /backups/hora-pre-upgrade.db
```

Then validate your config with the **new** binary:

```sh
docker run --rm -v "$PWD/hora-config:/etc/hora" ghcr.io/uplg/hora:latest check
```

As a rule, deploy a new binary before a config that uses its new keys: an
older binary refuses keys it does not know.

## 0.11 to 0.12

0.12 closes doors that 0.11 only warned about. Three schema migrations run
on the first start; one rewrites the `checks` table to about half its size,
which takes a moment on a large history and needs free disk space of about
the database's size (back up `hora.db` first: 0.11 cannot open it
afterwards). There are new optional config keys (`server.view_token`,
`allow_unauthenticated` among them, the full list is in the
[upgrade notes](https://github.com/uplg/hora/blob/main/UPGRADES.md)), and a few
configs and scripts that stop working until they are fixed. Run `hora check`
with the new binary first, in the daemon's own environment: it now prints the
warnings too, and `hora check --strict` fails on them.

- **Unset variables are named.** A `${VAR}` missing from the environment
  still expands to nothing and a channel left without its secret is still
  disabled, but both are now warned about by name at load, shown by
  `hora check`, and failed on by `hora check --strict`. Write `${VAR:-}` for
  a value that is optional, `${VAR:-fallback}` for a default.
- **Push monitors and watched peers need a token.** A push monitor without
  `push_token`, or a peer with `expect_every_secs` and no `listen_token`, no
  longer loads, unless it sets `allow_unauthenticated = true`. See
  [push monitors](../guides/monitors/#push-heartbeat) and
  [peers](../guides/peers/).
- **Tokens under 16 characters are refused**: `auth_token`, `view_token`,
  group tokens, `push_token`, `listen_token`.
- **`?token=` is refused on write endpoints** (401), valid or not. Send
  `X-Push-Token` or `Authorization: Bearer`. Read-only pages keep it. See
  [Authentication](../reference/api/#authentication).
- **A read-only token, `server.view_token`.** Put it in kiosk URLs instead
  of the operator token: it shows private monitors and cannot write.
- **Local-only downs page again, flagged**, when `alerts.notify_unconfirmed`
  is not set. `notify_unconfirmed = []` keeps them silent. See
  [local-only downs](../guides/peers/#local-only-downs).
- **`confirm_with_peers` requires an https `ping_url`** on every peer
  (loopback excepted).
- **Planned maintenance, announced to peers**: new optional keys (`host` on
  a monitor, on `[health]` and on a peer; `accept_maintenance_secs` on a
  peer) and one more migration, an empty table. In a mesh with a 0.11 node
  nothing breaks, and nothing is muted on the 0.11 side: upgrade both
  before relying on it. See
  [planned maintenance of a host](../guides/peers/#planned-maintenance-of-a-host).

- **Connections are bounded and timed**: a new optional key,
  `server.max_connections` (default 1024, fewer in a small container), and
  a stalled connection is closed. JSON request bodies are limited to
  64 KiB. See [Connections and memory](../configuration/#connections-and-memory).
- **`/healthz` gains `probes_ok`** and answers 503 when the host is out of
  file descriptors or memory: checks are then skipped, not failed.

The exact error messages and what to change are in
[UPGRADES.md](https://github.com/uplg/hora/blob/main/UPGRADES.md).

## 0.10 to 0.11

0.11 brings the new status page and a hardening pass. Four
schema migrations apply by themselves, in seconds even on very large
databases, and one optional config key is new
(`alerts.notify_unconfirmed`); the first page request after the upgrade
waits a few seconds for the first summary build. A few behaviours change:

- **Local-only downs no longer page.** A down that every peer that answered
  sees up goes to `alerts.notify_unconfirmed` (a quiet channel), or is only
  recorded, instead of the monitor's channels. See
  [local-only downs](../guides/peers/#local-only-downs).
- **Some monitors may turn down, rightly**: an ICMP monitor whose router
  answered "unreachable" (it read as up), or an https monitor redirected to
  plain http. A `cert_unreadable` alert may arrive once for a certificate
  the watcher could not read (it only logged it).
- **The webhook** gains the `cert_unreadable` event and a `local_only`
  field on `down` and `recovered`.
- **A push to an unknown id answers 401**, like a wrong token (it was 404).

- **Some configs no longer load.** Intervals and timeouts above 30 days, an
  `expected_status` outside 100 to 599, an unbracketed IPv6 tcp target, or a
  `dns_resolver` given as a hostname. See
  [Bounds](../configuration/#bounds). A hot reload that fails keeps the
  previous config.
- **Push monitors and peers that never sent a heartbeat now alert** ("no
  heartbeat received yet"). Comment out monitors for jobs that do not run
  yet. See [push monitors](../guides/monitors/#push-heartbeat).
- **A push with `status=down` alerts** with the job's message, after
  `fail_threshold`, instead of one interval late as a missed heartbeat.
- **`?token=` on write endpoints is deprecated.** Move scripts to
  `Authorization: Bearer` (or `X-Push-Token` for push and alert). See
  [Authentication](../reference/api/#authentication).
- **Pages are rate-limited per client IP.** Behind a reverse proxy, set
  `server.client_ip_header`, or every visitor shares one bucket. See
  [Rate limiting](../reference/api/#rate-limiting--security-headers).
- **Notification wording changed** on several channels (`API is DOWN`
  instead of `DOWN: API`; `[DOWN] API is DOWN` email subjects). Update any
  filter that matched the old text.
- **Atom readers** show existing incidents once more as new: entry ids now
  point at `/incident/{id}`.
- **`/healthz` answers 503** on a degraded node, so Docker's `HEALTHCHECK`
  marks it unhealthy.
- **Removed monitors keep their history for 7 days** before it is deleted.
  See [Removed monitors](../configuration/#removed-monitors).
- **One daemon per database**: the daemon holds `<db>.lock` beside it, and
  a second one on the same file refuses to start.
- **The status page no longer reloads itself.** A wall screen adds
  `?refresh=30` to its URL. See [The status page](../guides/status-page/).
- **Latency percentiles come from histograms**: identical below 64 ms,
  within 1.6% above, Prometheus quantiles included.
- **The heatmaps moved** from `/history` to each monitor's page,
  `/monitor/{id}`. The JSON API is unchanged.

## Version notes

Every version's notes, with what to check before rolling out, are in
[`UPGRADES.md`](https://github.com/uplg/hora/blob/main/UPGRADES.md). The
full history is in the [changelog](/hora/changelog/).
