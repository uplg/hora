---
title: Roadmap
description: Where Hora is heading - and what was deliberately left out.
---

Hora's compass: a single small binary, alerts you can trust at 3 a.m., and no
feature whose false positives outweigh its value. Plans below are directional,
not promises - the [changelog](https://github.com/uplg/hora/blob/main/CHANGELOG.md)
records what actually shipped.

## Next

- **Quiet hours & severity routing** - `quiet = "22:00-07:00"` per channel
  (non-critical alerts held and delivered as a morning digest; critical
  downs still pass), and a simple severity/group-to-channel matrix in the
  TOML - degraded to a quiet channel, down to a loud one. *Flapping never
  wakes you up*, taken literally - and the prerequisite for escalation.

## Exploring

- **Escalation & acknowledgement** - if an alert is not acknowledged within
  N minutes, notify the next channel; ack via a signed link in the
  notification. The biggest item on the list; more natural once severity
  routing exists.
- **`conf.d/` config splitting** - forty monitors in one file doesn't scale
  to a team; splitting plays well with config-as-code in git, and
  `hora peers diff` already verifies the mesh stays aligned.
- **Latency anomaly hints, info-only** - "4x slower than a usual Monday
  9 a.m." as a card hint computed from the hourly aggregates the heatmap
  already stores. Never an alert by default (see below).
- **Response-time breakdown (DNS / TCP / TLS / TTFB)** - likely as a
  `hora probe --breakdown` diagnostic first (hand-timed resolve, connect,
  handshake, first byte), keeping the monitoring loop untouched.
- **`hora import compose` / `caddy`** - generate monitors from a
  `docker-compose.yml` or a Caddyfile, like the Kuma importer.
- **Towards 1.0** - a config-format freeze, a SemVer commitment, and
  database migrations exercised against real long-lived databases.

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
- **An "agent mode" multi-region mesh** - lightweight satellites phoning home
  to a central brain. It reintroduces exactly the single point of failure the
  peer mesh exists to avoid (the brain dies, monitoring goes blind), for the
  price of one duplicated TOML file. Symmetric full nodes with
  [multi-vantage confirmation](../guides/peers/#multi-vantage-confirmation)
  embody the one-small-binary thesis better.
- **gRPC health probes** - the dependency tree doesn't pass the project's
  supply-chain policy (`cargo-deny`).
- **Email subscriptions to the status page** - subscriber storage,
  unsubscribe flows and outbound SMTP are a whole product; subscribe to the
  [Atom feed](../guides/incidents/#the-history-page-and-atom-feed) instead.
