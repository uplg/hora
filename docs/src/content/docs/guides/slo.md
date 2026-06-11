---
title: SLOs & error budgets
description: Availability SLOs with visible error budgets and Google-SRE multi-window burn-rate alerts.
---

A binary down alert misses a whole class of problems: the service that flaps
for two minutes every half hour never confirms down, yet quietly burns
through its availability target. SLOs catch exactly that.

## Declare a target

```toml
[[monitors]]
id = "api"
name = "API"
target = "https://api.example.com/health"
interval_secs = 30
slo_uptime = 99.9        # availability SLO, percent
slo_window_days = 30     # rolling window (default 30)
slo_latency_ms = 500     # optional: flag the 24h p95 against this
```

The status page then shows the **error budget left** for the window - e.g.
*"budget 21m of 43m left, 30d"*. A 99.9% SLO over 30 days is a budget of
~43 minutes of downtime; every failed check consumes it.

## Burn-rate alerts

Alerting is Google-SRE style **multi-window burn rate**, evaluated from the
recorded checks:

- **Page** (fast burn): the last hour burns the budget at a high multiple,
  *confirmed by the last 5 minutes* - so a stale spike never pages.
- **Warn** (slow burn): the last six hours burn at a lower multiple,
  confirmed by the last 30 minutes.

An alert looks like *"burning error budget at 14.4x (1h) - exhausted in ~6h
at this rate"*. Each severity fires once per episode and re-arms when the
long window cools back down; a fast alert subsumes the slow one. When the
flapping stops, Hora goes quiet - there is nothing to acknowledge.

The thresholds scale with the window length, so a 7-day SLO and a 90-day SLO
both page when a comparable fraction of their budget is at risk.

## Latency SLO

`slo_latency_ms` is display-only judgement: the 24h p95 is flagged met or
breached against it on the status page (next to p50/p95/p99). It never
alerts - latency targets are noisy, and the philosophy is that only confirmed
unavailability and budget burn wake you up.
