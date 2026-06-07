# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.2] - 2026-06-07

### Fixed

- A config file event whose content is unchanged (a `touch`, or a spurious event
  that some filesystems and Docker bind mounts emit) no longer triggers a reload.
  The watcher could otherwise feed itself in a tight loop - reloading thousands of
  times a second and pinning a CPU core. Reloads are now debounced, ignore read
  (access) events, and only run when the file content actually changed.

## [0.2.1] - 2026-06-07

A status-page polish pass. No configuration changes.

### Changed

- The grid uses the full width on large displays instead of being capped at
  ~1100px, so it is not stranded in the middle of a wide monitor.
- The latency caption is less verbose and the SLO / TLS badges stack in a column.
- A day's uptime bar stays amber as long as it was mostly up (down to 90%); it
  only turns red for a real outage (majority down) instead of below 99%.
- Active maintenance windows now show in a top banner with their reason and the
  affected monitors; the card only gets a left-border accent, so a long reason
  never changes a card's height or disturbs the grid.

## [0.2.0] - 2026-06-07

A large feature release. **Breaking**: notification configuration moved to named
channels - see _Changed_ and the migration note in the README.

### Added

- **Monitor types & checks** - HTTP body assertions (`keyword` / `keyword_invert`
  and a `JSONPath` `json_query` / `json_expected`, with a configurable `max_body_kb`
  cap); per-monitor HTTP/SOCKS `proxy`; and **push / heartbeat** monitors
  (`kind = "push"`) that go down when a job stops calling `POST /api/push/{id}`.
- **Notification channels** - Discord, Slack, a generic JSON webhook and SMTP
  e-mail, in addition to Telegram. Channels are **named** (`[[channels]]` with a
  `type`), so several can share a type, and each monitor can route to specific ones
  with `notify = ["name", ...]`.
- **Latency percentiles** - p50/p95/p99 over 24h on the page and API, with an
  optional `slo_latency_ms` objective flagged met/breached.
- **Scheduled maintenance windows** (`[[maintenance]]`) that mute alerts for the
  affected monitors (checks are still recorded).
- **Incidents / announcements** (`[[incidents]]`) shown as a banner on the page.
- **`${VAR}` interpolation** in the config so secrets can come from the environment.
- **`server.client_ip_header`** to trust a proxy header (e.g. `cf-connecting-ip`
  behind Cloudflare) for rate-limit keying.
- A human-readable UTC "last updated" timestamp in the footer.

### Changed

- **Breaking:** notification config moved from `[telegram]` / `[discord]` singletons
  to named `[[channels]]`. Secrets now come through `${VAR}` interpolation; the fixed
  `HORA_TELEGRAM_TOKEN` / `HORA_DISCORD_WEBHOOK_URL` overrides were removed
  (`HORA_BIND` / `HORA_DATABASE_PATH` are unchanged).
- `monitor.target` is now optional (it is unused for push monitors).

### Removed

- The transitive `tonic` (gRPC) dependency pulled in by the rate limiter.

## [0.1.1] - 2026-06-07

A review, hardening and performance pass. No configuration changes required.

### Fixed

- A failing database query for one monitor no longer blanks out the whole status
  page or `/api/summary`; that monitor degrades to an `unknown` card instead.
- `docker stop` now triggers a graceful shutdown - the server listens for
  `SIGTERM` in addition to `Ctrl-C`/`SIGINT`.
- The shared HTTP client now has request and connect timeouts, so a hung
  Telegram API connection can no longer stall a monitor's probing loop.
- `timeout_secs = 0` is rejected at load instead of silently disabling probing.
- The config file-watch reload no longer risks a blocking send off the runtime.
- The failing-response body snippet is now strictly bounded in size.

### Performance

- A covering index on `checks (monitor_id, time, status, latency_ms)` makes the
  availability, daily-bar and latency-series queries index-only (no per-row
  table lookups), so the status page stays fast as history grows.
- `synchronous = NORMAL` under WAL: probe inserts no longer `fsync` on every
  write, only at checkpoints.
- A larger SQLite page cache (16 MiB) keeps the hot index resident across the
  cached summary rebuilds.

### Security

- The Telegram bot token is redacted in `Debug` output, so it can never leak
  through a log line or panic message that formats the configuration.

### Changed

- The Docker image runs as a non-root user (UID 10001) and ships a `HEALTHCHECK`.
- The database pool now has a 10 s acquire timeout, so contention degrades a
  single card instead of hanging the page.
- Reproducible builds: `--locked` is enforced in CI and the Docker build.

## [0.1.0] - 2026-06-07

Initial release.

### Added

- **Monitors** - HTTP and TCP probes with per-monitor interval, timeout, expected
  status, a "degraded if slower than" threshold, and custom request headers.
- **Status page** - server-rendered (no JS framework) compact, responsive grid:
  daily uptime bars graded by severity, an inline SVG 24h latency chart,
  auto-refresh, Cal Sans branding and an SVG favicon.
- **JSON API** - `GET /api/summary` and `GET /api/monitors/{id}/latency`, plus a
  generated OpenAPI 3.1 document at `/api/openapi.json` (`utoipa`).
- **Badges** - embeddable flat SVG status and uptime badges per monitor at
  `/api/badge/{id}/status` and `/api/badge/{id}/uptime`.
- **TLS certificate expiry monitoring** with advance warnings.
- **Notifications** - a pluggable `Notifier` trait with a built-in Telegram
  channel; alerts fire only after _N_ consecutive failures (anti-flapping),
  include a snippet of the failing response body, and a recovery message.
- **Storage** - SQLite (sqlx) with per-monitor retention and automatic pruning.
- **Live configuration reload** - file-watch and `SIGHUP`: monitors, thresholds,
  retention and notification channels are reconciled in place, with no downtime
  for unchanged monitors.
- **API hardening** - per-IP rate limiting (`x-ratelimit-*` / `retry-after`
  headers), strict Content-Security-Policy, `X-Content-Type-Options` and
  `Referrer-Policy`.
- **Performance** - a lock-free, single-flight summary cache (busted on config
  reload) and concurrent per-monitor queries keep responses fast under load.
- **Packaging** - a static musl binary in a ~25 MB Alpine image (multi-arch
  amd64/arm64), with GitHub Actions for CI (fmt, clippy, tests, cargo-deny) and
  publishing to GHCR.

[Unreleased]: https://github.com/uplg/hora/compare/v0.2.2...HEAD
[0.2.2]: https://github.com/uplg/hora/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/uplg/hora/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/uplg/hora/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/uplg/hora/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/uplg/hora/releases/tag/v0.1.0
