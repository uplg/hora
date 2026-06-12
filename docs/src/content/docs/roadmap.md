---
title: Roadmap
description: Where Hora is heading - and what was deliberately left out.
---

Hora's compass: a single small binary, alerts you can trust at 3 a.m., and no
feature whose false positives outweigh its value. Plans below are directional,
not promises - the [changelog](https://github.com/uplg/hora/blob/main/CHANGELOG.md)
records what actually shipped.

## Next

- **`hora probe <target> [--confirm]`** - a one-shot ad-hoc check from the
  terminal with full monitor semantics (status, latency, cert days,
  assertions), where `--confirm` also asks your
  [peers](../guides/peers/#multi-vantage-confirmation) for their verdict: a
  private, distributed *"down for everyone or just me?"* in one SSH command.
- **Notification channel watchdog** - a broken Telegram channel currently
  fails silently in the logs until the next real incident. The dispatcher
  will track consecutive delivery failures per channel and alert *through
  the other channels* ("your telegram channel has been failing for 2 days"),
  surfacing it in `hora doctor` and `hora top` too - the dead-man philosophy
  applied to the notifications themselves.
- **Event markers** - `hora event "deploy api v2.3"` (or `POST /api/event`
  from a CI hook): a marker on the latency charts, a line in the history,
  and automatic correlation in incidents ("down 3 minutes after *deploy api
  v2.3*"). The first diagnostic question is never "is it slow?" - it is
  "what changed?".
- **`hora tune`** - replay your own history against other settings: "with
  `fail_threshold = 5` you would have had 4 alerts last month instead of 11,
  catching the same real outages 40 s later". Threshold recommendations
  computed from your own data.

## Exploring

- **Auto-generated post-mortems** - an incident already knows its first
  failure, the captured response, the multi-vantage verdict, the topology
  and the correlated event: assemble it into a Markdown report ready to
  paste in a ticket (`hora postmortem <id>`, `/incident/{id}`).
- **Per-vantage data on the status page** - the observability residue of a
  "multi-region mesh" now that [multi-vantage
  confirmation](../guides/peers/#multi-vantage-confirmation) covers the
  anti-false-positive side: a region badge and per-vantage latency ("80 ms
  from EU, 220 ms from US"), aggregated read-only from the peers'
  `/api/summary`. A display increment, not an architecture project.
- **Escalation & acknowledgement** - if an alert is not acknowledged within
  N minutes, notify the next channel; ack via a signed link in the
  notification. The biggest item on the list, and more natural once
  multi-vantage confirmation exists.
- **`conf.d/` config splitting** - forty monitors in one file doesn't scale
  to a team; splitting plays well with config-as-code in git.
- **Quiet hours** - `quiet = "22:00-07:00"` per channel: non-critical alerts
  held and delivered as a morning digest; critical downs still pass.
- **Cert expiry over STARTTLS** - extend the certificate machinery to SMTP
  587 / IMAP 143, the certificate every self-hosted mail operator forgets.
- **`hora import compose` / `caddy`** - generate monitors from a
  `docker-compose.yml` or a Caddyfile, like the Kuma importer.
- **Severity/group alert routing** - degraded to a quiet channel, down to a
  loud one, the database group to SMS: a simple matrix in the TOML, not a
  rule engine.
- **Response-time breakdown (DNS / TCP / TLS / TTFB)** - know whether the
  slowness is the resolution, the handshake or the backend.
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
