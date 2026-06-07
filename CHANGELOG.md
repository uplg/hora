# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-06-07

Initial release.

### Added

- **Monitors** — HTTP and TCP probes with per-monitor interval, timeout, expected
  status, a "degraded if slower than" threshold, and custom request headers.
- **Status page** — server-rendered (no JS framework) compact, responsive grid:
  daily uptime bars graded by severity, an inline SVG 24h latency chart,
  auto-refresh, Cal Sans branding and an SVG favicon.
- **JSON API** — `GET /api/summary` and `GET /api/monitors/{id}/latency`, plus a
  generated OpenAPI 3.1 document at `/api/openapi.json` (`utoipa`).
- **Badges** — embeddable flat SVG status and uptime badges per monitor at
  `/api/badge/{id}/status` and `/api/badge/{id}/uptime`.
- **TLS certificate expiry monitoring** with advance warnings.
- **Notifications** — a pluggable `Notifier` trait with a built-in Telegram
  channel; alerts fire only after _N_ consecutive failures (anti-flapping),
  include a snippet of the failing response body, and a recovery message.
- **Storage** — SQLite (sqlx) with per-monitor retention and automatic pruning.
- **Live configuration reload** — file-watch and `SIGHUP`: monitors, thresholds,
  retention and notification channels are reconciled in place, with no downtime
  for unchanged monitors.
- **API hardening** — per-IP rate limiting (`x-ratelimit-*` / `retry-after`
  headers), strict Content-Security-Policy, `X-Content-Type-Options` and
  `Referrer-Policy`.
- **Performance** — a lock-free, single-flight summary cache (busted on config
  reload) and concurrent per-monitor queries keep responses fast under load.
- **Packaging** — a static musl binary in a ~25 MB Alpine image (multi-arch
  amd64/arm64), with GitHub Actions for CI (fmt, clippy, tests, cargo-deny) and
  publishing to GHCR.

[Unreleased]: https://github.com/uplg/hora/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/uplg/hora/releases/tag/v0.1.0
