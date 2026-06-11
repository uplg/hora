# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.6.0] - 2026-06-12

### Added

- **Multi-vantage confirmation** (`confirm_with_peers`, the headline of
  0.6.0): when a monitor confirms down locally, the peers probe the same
  target from their side before the alert goes out, and the alert carries
  the verdict - *"confirmed down from 3/3 vantage points"* (a real outage)
  vs *"seen UP by hora-b - network issue near this node?"*. Two Raspberry Pi
  at two homes become a distributed Pingdom. Built to be boring under
  failure:
  - **Strictly fail-open**: peers being slow, broken, unreachable or
    misconfigured never block, delay past a hard 10s deadline (probes run
    concurrently), or suppress the alert - the worst outcome is an alert
    without the annotation, exactly what Hora sent before. The incident
    record is written *before* the peers are consulted.
  - **Never a proxy**: the new `POST /api/peer/probe` only probes targets
    present in the responder's *own* configuration (matched on kind +
    target, probed with its own settings), so a leaked token cannot turn a
    peer into an SSRF relay. It strictly requires the requesting peer's
    `listen_token` - the id alone never authorizes - and unknown peers are
    indistinguishable from wrong tokens.
  - **A disputed down still alerts**: a peer seeing the target up softens
    the message, never silences it - geo-partial outages are real outages.
  - Enabled globally with `[health] confirm_with_peers = true`, overridden
    per monitor; peer probe requests never ride a monitor's proxy; verified
    end-to-end by a two-real-nodes test over live HTTP sockets.

- **Per-group status pages** (`/status/{group}`): one display group's
  monitors, nothing else - lightweight multi-tenancy for an operator hosting
  several clients on one Hora. A per-group token (`server.group_tokens`)
  reveals that group's full view - private monitors included - and nothing
  else (it is never accepted anywhere as a global token); the maintenance
  banners are filtered to windows touching the group, and the peers section
  stays off client pages. An unknown group, or a fully private one viewed
  anonymously, answers 404.
- **Monthly SLA reports** (`/report/2026-05` and `hora report [YYYY-MM]`,
  default last month): a printable, print-first page - uptime per monitor
  and group, incidents, downtime (clipped to the month), MTTR (incidents
  resolved within the month), SLO verdict and error-budget consumption.
  `?group=` scopes the report to one group, with the group token accepted -
  the report an agency hands its client. Works as far back as the one-year
  aggregate retention; the running month is judged against elapsed time only.
- **`hora doctor`**: runtime environment diagnostics - the companion of
  `hora check`. Database writable, listen port free (busy is a warning: the
  daemon is probably just running), IPv4/IPv6 routes (no packets sent), the
  unprivileged ICMP datagram socket, and a real system-resolver lookup. Each
  finding is judged against what the *current config needs* - no IPv6 route
  only fails when a `dual_stack` monitor needs one - and the process exits
  non-zero on any missing needed capability.
- **Weekly digest** (`[digest]`): a recap of the last seven days through the
  notification channels - "99.97% overall, 2 incidents" plus one line per
  monitor with uptime, incidents and the error budget left when an SLO is
  set. Sent on a cron schedule (default Monday 08:00 UTC), optionally routed
  to specific channels with `notify`; the last-sent timestamp persists in the
  database, so a restart neither double-sends nor forgets, and a send missed
  while the daemon was down catches up once. Informational by construction -
  the one notification that never signals a problem. `hora digest` prints the
  exact text as a dry run.

## [0.5.1] - 2026-06-11

### Added

- **Domain expiry via RDAP** (`domain_expiry = "example.com"` per monitor):
  the registered domain is checked once a day against the registry (RDAP,
  JSON over HTTP via the rdap.org bootstrap - no whois parsing) and an alert
  fires `alerts.domain_expiry_days` (default 14) before it expires - the
  natural sibling of the TLS expiry warnings, with the same edge-triggered,
  maintenance-muted policy. The domain is explicit rather than derived from
  the target: registrable-domain extraction would need a public-suffix list,
  and the operator already knows the answer.
- **Latency heatmaps** on `/history`: a smokeping-style hours-by-days SVG per
  monitor (last 28 days, raw checks + hourly buckets), colour = how slow that
  hour was *relative to the monitor's own median* - "slow every Monday at
  9am" at a glance, with zero false-positive risk. Collapsed by default,
  loaded lazily from `GET /api/monitors/{id}/heatmap.svg` (same visibility
  rules as the latency endpoint).

### Changed

- **`hora test-alert` exits non-zero when a channel fails**, naming the
  failing channels - the notification chain is now CI-gateable. Under the
  hood `Notifier::notify()` returns a `Result` and the dispatcher reports
  which channels failed; the daemon's fire-and-forget behaviour (log a
  warning, never block alerting) is unchanged.

## [0.5.0] - 2026-06-11

### Added

- **Documentation site** at <https://uplg.github.io/hora/>: guides (monitors,
  alerting, SLOs, incidents, peers, Kuma import), CLI & HTTP API reference,
  and the roadmap. Built with Astro Starlight from `docs/`, deployed to
  GitHub Pages by the Docs workflow on every docs change.

- **Failure snapshots**: when an HTTP probe confirms a down *with a response*
  (bad status or failed assertion), the incident records what the service
  actually answered - status line, headers and the start of the body, bounded
  at capture time (24 headers, 160 chars/line, 2 KiB of body). Shown on
  `/history` in a collapsed "what the service answered" block, and as the
  status line in `hora incidents`. Same privacy rule as failure reasons:
  anonymous viewers never see it unless the monitor sets
  `public_error_detail`. DNS pin mismatches snapshot the full (bounded)
  answer too - TXT records rarely fit the inline reason - and a dual-stack
  down keeps the failing family's snapshot.

- **Ad-hoc silences** (`hora silence`, `POST /api/silence`): mute alerts for
  some monitors (comma-separated ids, or `all`) for a duration like `10m` or
  `1h30m` (max 7 days, with an optional reason) - the scriptable counterpart
  of a `[[maintenance]]` window, made for deploy hooks. Checks keep being
  recorded; only alert transitions are muted, and a database read error fails
  open (alerts still fire). The HTTP endpoint strictly requires
  `server.auth_token`; the CLI writes straight into the database and also
  offers `hora silence list` / `hora silence clear`. Expired silences are
  swept by the pruner.
- **Incident annotations** (`hora annotate <id|last> "<note>"`): attach a
  free-form operator note to an incident ("fiber cut, ETA 6pm"), displayed on
  `/history` and in the Atom feed. Notes are written *for* visitors, so they
  deliberately survive the anonymous-viewer sanitization that collapses
  captured failure detail. An empty note clears the annotation, `last` targets
  the most recent incident, and the new `hora incidents [limit]` lists recent
  incidents with their ids.
- **`hora backup <dest>`**: snapshot the database with SQLite's `VACUUM INTO` -
  consistent and compacted, safe while the daemon is writing. The source is
  opened read-only (a backup never creates or migrates a database), an
  existing destination is refused, and the snapshot is chmod'ed 0600 like the
  live database.

## [0.4.2] - 2026-06-10

### Added

- **Dual-stack verification** (`dual_stack = true`): http, tcp and icmp
  monitors can probe IPv4 and IPv6 separately (concurrently) and require both
  families to pass - catching the service whose IPv6 has been silently dead
  behind a healthy IPv4, or the reverse. One broken family confirms down with
  the culprit named ("IPv6 failing: connection timed out (IPv4 ok)") and the
  surviving family's latency recorded; when both families answer, the recorded
  latency is the slower path's, so `degraded_over_ms` judges the worst case.
  Anonymous viewers see the collapsed category ("IPv6 failing (IPv4 ok)").
  Requires a hostname target (an IP literal has a single family), cannot be
  combined with `proxy`, and the probing host itself needs working IPv4 and
  IPv6. HTTP probes are steered per family by binding the client's local end
  to the family's unspecified address.
- **`hora test-alert [monitor-id]`**: send a clearly-labelled test alert (down
  then recovered) through the real notification chain, so delivery is verified
  before the first real incident instead of during it. Without an id every
  configured channel is exercised; with one, the monitor's `notify` routing
  applies - testing exactly what would fire. A failing channel logs a warning
  with the rejection detail; an unknown id lists the configured ones.

## [0.4.1] - 2026-06-10

Security hardening release. See [UPGRADES.md](UPGRADES.md) for the six
behavioural changes.

### Security

- **Empty access tokens fail startup**: `server.auth_token`, `push_token`,
  `listen_token` and `ping_token` set to `""` - typically a `${VAR}`
  interpolating an unset variable - were silently treated as "no token",
  and an empty token would have authorized a blank `?token=`. Short-but-set
  tokens (under 16 chars) now warn.
- **Probe headers stop at the origin**: per-monitor `headers` (which often
  carry credentials, e.g. an API key) are re-attached across redirects only
  while the hop stays on the monitor's scheme/host/port - reqwest strips its
  own well-known sensitive headers across hosts, but not arbitrary custom
  ones. Probes follow at most 10 redirects, with the monitor's timeout
  covering the whole chain.
- **Anonymous viewers get categorized failure reasons**: the status page,
  `/api/summary`, `/history` and the Atom feed collapse a public monitor's
  stored failure detail (which can carry response-body snippets, DNS answers
  or asserted keywords) to a safe category ("HTTP 500", "content check
  failed"). The full reason still shows with the viewer token; a monitor can
  opt back in with `public_error_detail = true`. Topology annotations
  ("caused by", "impacts") never name a private monitor publicly.
- **`${VAR}` expands after parsing, in string values only**: a `${VAR}`
  inside a comment is no longer looked up, and a TOML syntax error can no
  longer echo an already-expanded secret back in its message.
- **`cert_pin` is validated and canonicalized**: it must be 64 hex chars
  (SHA-256 of the leaf public key) and is lowercased at load, so a malformed
  or mixed-case pin can't silently disable pinning.
- **Tokenless push targets warn at startup**: a push monitor without
  `push_token` (or a watched peer without `listen_token`) accepts heartbeats
  on the id alone, and ids are not secrets - the page, API and `/healthz`
  expose them - so anyone who can reach `/api/push` could forge heartbeats.
- **Rate limiting keys on the TCP peer** unless `server.client_ip_header`
  names the trusted proxy header - a direct client could mint fresh buckets
  by rotating `X-Forwarded-For`.
- Defence in depth: the database file is created with `0600` permissions,
  access logs record only the request path (query strings carried tokens),
  notifier log redaction strips every channel secret including its
  percent-encoded forms, witness `/healthz` bodies are capped at 64 KB, and
  a daily RustSec advisory scan (`audit.yml`) backs the `cargo-deny` gate.

### Changed

- The push examples and the dead-man heartbeat send the token in the
  `X-Push-Token` header instead of `?token=`, keeping it out of access logs
  (the query form still works).
- `/api/monitors/{id}/latency` aggregates in SQL (epoch-anchored buckets),
  so a wide window on a high-frequency monitor stays bounded and an
  auto-refreshing chart doesn't jitter.
- The `/history` page uses the same width as the status page (1500px,
  92vw beyond 1700px) instead of a narrow 900px column.

## [0.4.0] - 2026-06-09

### Added

- **Probe retries** (`probe_retries`, default 1, max 5): a failed probe is
  re-tried after one second before anything is recorded, so a single network
  blip between Hora and the target never lands in the history, the uptime
  numbers or the error budget - the burn-rate alerts and the page tell the
  same story. Retries are logged; set `probe_retries = 0` to record every
  raw result.
- **Failure reasons surfaced**: the most recent check's error (timeout, HTTP
  status + body snippet, connect error) is now a tooltip on the status dot,
  a `last_error` field in `/api/summary`, and a `check failed` warn log line
  for every failure that survived its retries (visible in `docker logs`) -
  no more opening the database to learn why a card went orange.
- **Header navigation**: the status page links to the incident history (and
  the history page back to the status page and its Atom feed) as pills in the
  header.

- **Availability SLOs, error budgets and burn-rate alerts**: `slo_uptime = 99.9`
  (+ optional `slo_window_days`, default 30) per monitor. The status page and
  `/api/summary` show the error budget left over the window (computed from the
  same merged daily history as the bars, so it survives raw retention); alerts
  are Google-SRE multi-window burn rates - fast (2% of budget in 1h, confirmed
  over 5m) and slow (5% in 6h, confirmed over 30m) - via a new `budget_burn`
  event on every notification channel, with an exhaustion ETA. Edge-triggered:
  one alert per episode, re-armed when the long window cools.
- **Cron-aware push monitors**: `schedule = "0 3 * * *"` (five-field cron, UTC)
  plus `grace_secs` (default 1800) on a push monitor alerts only when a
  scheduled run misses its grace window, instead of the fixed
  `interval_secs` gap - made for nightly jobs, à la Healthchecks.io.
- **Root-cause alert grouping** (`alerts.group_window_secs`, default 30):
  a monitor confirmed down whose `depends_on` upstream is also down waits out
  the window; if the upstream alerts (or already has), the dependent's alert -
  and its later recovery - fold into that single notification, transitively
  along dependency chains. A flap inside the window sends nothing. Incident
  records are unaffected (history stays complete). Set 0 to disable.
- **Uptime Kuma import** now also maps `json-query` monitors (JSONPath +
  expected value), request `headers`, `timeout`, single expected status codes,
  `expiryNotification = false` (→ `check_cert = false`) and Kuma groups:
  monitors under a Kuma folder get `group = "<folder name>"`. Both current and
  legacy Kuma field spellings (`maxretries`, `accepted_statuscodes`,
  `dns_resolve_type`) are accepted.
- **DNS monitors** (`kind = "dns"`): resolve a hostname (A, AAAA, CNAME, MX, NS,
  TXT, SRV, SOA or PTR via `dns_record`, system or custom `dns_resolver`) and
  optionally **pin the expected answer** with `dns_expected` (comma-separated,
  order-insensitive - hijack detection that does not flap on round-robin
  rotation). Without `dns_expected`, any non-empty answer counts as up.
- **TLS certificate pinning** (`cert_pin`, hex SHA-256 of the leaf public key):
  a fingerprint matching neither the pin nor the last seen value fires a
  `CertChanged` alert - once per change, surviving restarts, muted during
  maintenance windows like other alerts. The alert carries the old and new
  fingerprints, so a first mismatch also tells you the correct pin to configure.
- **Automatic incident history**: confirmed down/up transitions are recorded as
  incidents (start, end, duration, error, root cause and blast radius), served
  on `/history` (server-rendered, no JS) and as an **Atom feed** at
  `/history.atom`. Incidents survive restarts: a still-open incident is
  re-attached on startup and closed on the first healthy tick. Closed incidents
  are pruned after a year.
- **Prometheus `/metrics`** (text exposition format): `hora_monitor_up`,
  `hora_monitor_degraded`, `hora_monitor_uptime_ratio` (24h),
  `hora_monitor_last_latency_ms`, `hora_monitor_latency_ms{quantile=…}`
  (p50/p95/p99) and `hora_cert_expiry_days`, all labelled `{id, name}`.
- **Private monitors**: `public = false` hides a monitor from the
  unauthenticated status page, `/api/summary`, latency API, badges, `/metrics`
  and the incident history. A viewer token (`server.auth_token`, live-reloaded,
  sent as `Authorization: Bearer` or `?token=`) reveals the full view; both
  views are cached. Config validation rejects `public = false` without a token.
- **Plain-text status for terminals**: `curl status.example.com` (or an
  `Accept: text/plain` request) returns an aligned text rendering of the status
  page, groups, topology annotations and peers included.
- **Long-term downsampling**: raw checks roll up into hourly buckets after
  7 days and daily buckets after 90, kept for a year. Buckets are written once
  (never recomputed from partially-pruned raw data) and the daily uptime bars
  transparently read them beyond the raw retention window. Aggregates of
  removed monitors are swept like any other orphan.
- **ntfy, Gotify and Pushover** notification channels (`type = "ntfy" |
  "gotify" | "pushover"`), with the shared retry/redaction policy; the Gotify
  token travels as a header, never in the URL.
- **`hora check`**: validate the configuration and exit non-zero on error
  (CI-friendly), and **`hora import kuma <backup.json>`**: convert an Uptime
  Kuma backup to Hora TOML on stdout (http/keyword, port, ping, dns and push
  monitors; anything else becomes a commented stub). Plus `--version`/`-V`.

### Changed

- A failed check now needs to fail twice (probe + one retry, see
  `probe_retries`) before being recorded; histories get cleaner from this
  release on, existing rows are untouched.
- Down alerts of monitors whose `depends_on` upstream is also down now wait
  out the grouping window (up to `alerts.group_window_secs`, default 30s)
  before being sent - or folded. Root-cause alerts are unaffected. Set
  `group_window_secs = 0` for the previous one-alert-per-monitor behaviour.
- `/api/monitors/{id}/latency` answers 404 for private monitors without the
  viewer token, exactly as for unknown ids.

## [0.3.0] - 2026-06-08

### Added

- **ICMP (ping) monitors** (`kind = "icmp"`): `target` is a host or IP (no port),
  up = an echo reply within the timeout, latency = the round-trip time
  (`degraded_over_ms` applies). It uses an **unprivileged datagram socket**, so it
  works in rootless Docker without `CAP_NET_RAW` (the kernel's
  `net.ipv4.ping_group_range`, Docker's default, must cover the process); when no
  ICMP permission is available the monitor reports down with a clear reason rather
  than crashing. **IPv4 and IPv6** are both supported.
- **Dependency-aware alerting** (`depends_on`) and **display groups** (`group`)
  on monitors. When a monitor goes down, the alert is annotated with topology
  context: `"caused by X"` if an upstream it depends on is also down (symptom),
  or `"impacts: A, B, C"` if all its upstreams are up (root cause with blast
  radius). Every monitor still alerts independently — annotations are additive,
  nothing is suppressed. The dependency graph is validated as a DAG at load
  (Kahn's algorithm); cycles and unknown references are rejected. On the status
  page, monitors are grouped by their `group` field under section headers. The
  webhook payload carries structural `cause` and `impacted` fields.
- **Mutual surveillance / dead-man's switch** via a `[health]` section and
  `[[peers]]`. A node emits an outbound heartbeat to each peer's `ping_url` only
  while it is locally healthy (scheduler ticking *and* database writable), so a
  hung or dead node goes silent and its peers mark it down. Each peer has two
  independent halves - OUT (`ping_url`) and IN (`expect_every_secs`) - and either
  half can terminate at another Hora or at an external service (healthchecks.io,
  UptimeRobot, a cron job); the wire is plain HTTP. With `quorum = true` a node
  consults the other peers' `/healthz` before alerting a peer down: if a witness
  still sees it up, it reports a low-severity `PeerLinkDegraded` (a partition)
  instead of an outage, and stays silent if it cannot reach any witness (likely
  the local node is the isolated one). Watched peers appear in their own section
  on the status page. Peers and `[health]` hot-reload like monitors (on SIGHUP or
  a config-file edit) - adding, removing or changing a peer needs no restart.
- `/healthz` now returns a JSON report (`status`, `scheduler_ok`, `db_ok`,
  `last_tick_age`, `id`, and this node's `peers` view) instead of a bare `ok`.
  The top-level `status` is `"ok"` only when fully healthy, so a keyword monitor
  (e.g. UptimeRobot) can poll it; the rest powers peer quorum.
- `alerts.alert_on_degraded` (default off): also alert when a monitor is
  *degraded* - up, but slower than its `degraded_over_ms` - not only when it is
  down. Uses the same `fail_threshold`, sends a new `degraded` event to every
  channel, and recovers when the monitor is fully healthy again. (A monitor with
  no `degraded_over_ms` is never degraded, so this is a no-op for it.)

## [0.2.4] - 2026-06-08

### Added

- Two more notification channels: **Matrix** (posts to a room via the
  client-server API, authenticating with a bot access token) and **Free Mobile
  SMS** (texts your own number via the operator's API). Configure them like any
  other named channel - see `config.example.toml`.

### Changed

- Internal: the `hora-web` crate's single large `lib.rs` was split into focused
  modules (`routes`, `handlers`, `summary`, `render`) with tests colocated. No
  behaviour change.

## [0.2.3] - 2026-06-08

A hardening and scalability release: no config changes required.

### Security

- Secrets are now a dedicated `Secret` type that redacts itself in `Debug`, so a
  `{config:?}` in a log line or panic can never leak one. This covers channel
  credentials (Telegram token, Discord/Slack/webhook URLs, SMTP password), a
  monitor's request headers, and its push token.
- A monitor's `target` and `proxy` are masked too: any `user:pass@` credentials
  embedded in those URLs are redacted in `Debug`, keeping the host for debugging.
- Push tokens can be sent as an `X-Push-Token` header instead of `?token=`, so
  the secret stays out of proxy access logs, and the token is compared in
  constant time.
- Notification failures log a snippet of the provider's response with the secret
  stripped out first.

### Added

- An `x-request-id` correlation id on every response (an inbound one is honoured,
  otherwise a fresh opaque id is minted) and threaded through the request's trace
  span, so log lines can be tied back to a single request.
- Graceful shutdown: on `SIGTERM`/Ctrl-C the background tasks (supervisor,
  per-monitor probes, certificate watcher, pruner) finish their current iteration
  and exit cleanly instead of being aborted mid-write.
- Notifications retry transient failures (a network error, HTTP 5xx, or 429) with
  a short backoff, so one blip no longer silently drops an alert.

### Changed

- The status summary is built from batched queries (one per aggregate across all
  monitors) instead of a few per monitor, and latency percentiles plus the card
  sparklines are now computed in SQL. Memory use and page size are bounded by the
  monitor count rather than the check frequency.
- The `checks` table gained a primary key, a `UNIQUE (monitor_id, time)`
  constraint and a `CHECK` on `status`; inserts are `INSERT OR IGNORE`. Existing
  rows are de-duplicated by a migration. This prevents a retry or manual insert
  from double-counting in the aggregates.
- SMTP delivery has a 15s timeout; the certificate verifier advertises the
  provider's signature schemes; `/api/openapi.json` returns 500 (not an empty
  200) if generation ever fails; responses carry `X-Frame-Options: DENY`.

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

[Unreleased]: https://github.com/uplg/hora/compare/v0.4.1...HEAD
[0.4.1]: https://github.com/uplg/hora/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/uplg/hora/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/uplg/hora/compare/v0.2.4...v0.3.0
[0.2.4]: https://github.com/uplg/hora/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/uplg/hora/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/uplg/hora/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/uplg/hora/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/uplg/hora/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/uplg/hora/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/uplg/hora/releases/tag/v0.1.0
