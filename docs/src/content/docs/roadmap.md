---
title: Roadmap
description: Where Hora is heading - and what was deliberately left out.
---

Hora's compass: a single small binary, alerts you can trust at 3 a.m., and no
feature whose false positives outweigh its value. Plans below are directional,
not promises - the [changelog](https://github.com/uplg/hora/blob/main/CHANGELOG.md)
records what actually shipped.

## Next

- **Multi-vantage confirmation via peers** - when a monitor goes down
  locally, ask your [peers](../guides/peers/) to probe the same target from
  their side before alerting: *"down from 3/3 sites"* (real outage) vs
  *"down only from here, 2 peers see it up"* (local network blip - a
  different, quieter alert). Two Raspberry Pi at two homes become a
  distributed Pingdom. The peer auth and quorum infrastructure already
  exist; peers will only probe targets present in their own config, never
  arbitrary requested ones.

## Exploring

- **Escalation & acknowledgement** - if an alert is not acknowledged within
  N minutes, notify the next channel; ack via a signed link in the
  notification. The biggest item on the list, and more natural once
  multi-vantage confirmation exists.
- **`kind = "exec"` probes** - external command probes using the
  monitoring-plugins exit-code convention (0 = up, 1 = degraded, else down),
  unlocking the whole Nagios/Icinga plugin ecosystem.
- **`hora top`** - a terminal dashboard consuming the JSON API: live
  statuses, latency sparklines, ongoing incidents. Self-hosters live in SSH.
- **Public incident banner** - a pinned status-page message set via CLI/API
  (*"fiber incident, ETA 6 p.m."*), the communication extension of
  [incident annotations](../guides/incidents/#operator-annotations).
- **`conf.d/` config splitting** - forty monitors in one file doesn't scale
  to a team; splitting plays well with config-as-code in git.
- **Monthly SLA reports** - a printable per-month page (uptime per
  monitor/group, incidents, MTTR, budget consumed) for freelances and
  agencies hosting client sites.
- **Per-group status pages** - `/status/clients-acme` showing only one
  group, optionally with its own token: each client gets their page without
  seeing the rest.
- **Quiet hours** - `quiet = "22:00-07:00"` per channel: non-critical alerts
  held and delivered as a morning digest; critical downs still pass.
- **Cert expiry over STARTTLS** - extend the certificate machinery to SMTP
  587 / IMAP 143, the certificate every self-hosted mail operator forgets.
- **`hora doctor`** - runtime environment diagnostics: IPv6 available? ICMP
  socket allowed? DNS resolver reachable? The runtime companion of
  `hora check`, precious in rootless Docker.
- **`hora import compose` / `caddy`** - generate monitors from a
  `docker-compose.yml` or a Caddyfile, like the Kuma importer.

## Deliberately not planned

Declined with reasons, so they stay declined:

- **Alerting on latency anomalies by default** - an adaptive baseline
  (*"4x slower than a usual Monday 9 a.m."*) generates false positives until
  tuned, which contradicts *flapping never wakes you up*. If it comes, it
  will be info-only on the status page first.
- **Multi-step HTTP scenarios** - login → extract token → authenticated GET
  → assert. Separates page monitoring from journey monitoring, but pulls
  toward a DSL and away from a simple `config.toml`.
- **Content change detection** - same flap logic as alerting on DNS answer
  rotation: change is not failure.
- **gRPC health probes** - the dependency tree doesn't pass the project's
  supply-chain policy (`cargo-deny`).
- **Email subscriptions to the status page** - subscriber storage,
  unsubscribe flows and outbound SMTP are a whole product; subscribe to the
  [Atom feed](../guides/incidents/#the-history-page-and-atom-feed) instead.
