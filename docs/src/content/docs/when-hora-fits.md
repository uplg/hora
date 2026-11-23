---
title: When Hora fits
description: What Hora is built for, what it deliberately leaves out, and the good neighbours to reach for when you need something else.
---

Hora is built around one promise: **it wakes you only for a real outage**.
Everything else follows from that: a failed check is retried, an outage
needs several failures in a row, peers in other places can confirm it, and a
cascade sends one alert about its cause. It ships as one small binary with a
TOML file, so the whole setup fits in git.

That focus makes it a good fit for some teams and not for others. This page
tries to help you tell which one you are.

## Hora fits when

- **Your nights matter more than your dashboards.** You want few alerts, and
  every one of them worth acting on: retries, `fail_threshold`, maintenance
  windows and silences are there to keep the pager quiet until it should
  not be.
- **You run services that depend on each other.** With `depends_on`, a down
  database is one alert about the database, not one per service behind it
  (see [Concepts](../concepts/)).
- **You can watch from more than one place.** Two or three small Hora nodes
  [watch each other](../guides/peers/) and confirm an outage before anyone
  is paged; a hiccup seen from one place only is reported as such.
- **You keep your configuration in git.** One TOML file, reloaded live, the
  same on every node, reviewed in pull requests like the rest of your
  infrastructure. No web editor to drift from it.
- **You make promises about uptime.** Error budgets, burn-rate alerts and a
  printable monthly report come with every monitor that sets a target
  ([SLOs](../guides/slo/)).
- **You want it small.** One static binary, SQLite, a calm public page
  that stays fast with hundreds of services and months of history.

## Another tool may fit better when

Self-hosted monitoring has good neighbours, and they are worth knowing.

- **You would rather click than write a file**, or you need a check type
  Hora does not have (databases, a real browser...).
  [Uptime Kuma](https://github.com/louislam/uptime-kuma) has a friendly web
  UI and a very long list of monitor types and notification services.
  Coming from it? [`hora import kuma`](../guides/import/) converts a backup.
- **You need UDP, SSH or gRPC checks, or multi-step check suites**, in a
  single binary and a YAML file. [Gatus](https://github.com/TwiN/gatus)
  is close to Hora in spirit and covers those.
- **What you watch is mostly scheduled jobs.**
  [Healthchecks](https://github.com/healthchecks/healthchecks) does that one
  thing very well, with time zones and systemd `OnCalendar` schedules.
  Hora's push monitors cover the common case next to active checks; the two
  also combine nicely: a Hora node can send its own
  [dead-man heartbeat](../guides/peers/#external-receivers) to a
  Healthchecks check, so something outside watches the watcher.

## What Hora deliberately leaves out

- **Some check types.** No built-in UDP, SSH, gRPC, database or browser
  checks (see the [roadmap](../roadmap/#deliberately-not-planned)). An
  [exec probe](../guides/monitors/#exec) runs any monitoring-plugins check,
  which covers many of them.
- **A long list of notification services.** Ten channels are built in; the
  generic webhook reaches the rest.
- **A web editor.** Everything lives in the TOML file, by design.
- **Multi-step scenarios** (log in, then check), to keep the configuration
  simple.
- **A database server.** SQLite only: one file, easy to back up.
