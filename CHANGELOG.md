# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.11.4] - 2026-10-27

A security release for two advisories published on 2026-10-21 and
2026-10-23. Hora's scheduler, supervisor, maintenance and certificate tasks
all tick on `tokio::time::Interval`, so the Tokio one applies directly. The
minimum Rust version is now declared. No other change.

### Security

- **tokio 1.54.1.** Fixes RUSTSEC-2026-0384: on the multi-threaded runtime,
  a race between `Interval::poll_tick` and the time driver could drop a timer
  entry the driver was firing, a use-after-free that showed up as assertion
  panics, corrupted task reference counts or heap corruption.
- **ctutils 0.4.6.** Fixes RUSTSEC-2026-0385: its mask generation and `u128`
  comparisons, meant to be constant-time, could compile to branches. The other
  dependencies take their compatible patch releases (webpki-roots 1.0.11,
  among others).

### Changed

- **Minimum supported Rust version: 1.99.1**, declared as `rust-version` in
  the workspace manifest. Building from source with an older toolchain now
  fails up front instead of somewhere in the dependency tree. The Docker image
  is unaffected.

## [0.11.3] - 2026-10-21

A security release for three certificate advisories published on
2026-10-19 and 2026-10-20, plus an unmaintained crate gone. Hora checks
certificates with rustls-webpki on its outbound TLS (HTTPS and TLS probes,
notification channels, the release check). No other change.

### Security

- **rustls-webpki 0.103.16, webpki-roots and webpki-root-certs 1.0.10.**
  Fixes RUSTSEC-2026-0382 (certificate paths through the distrusted emSign
  intermediates, whose private keys are now well known, could be accepted),
  RUSTSEC-2026-0381 and RUSTSEC-2026-0383 (the bundled roots still trusted
  `emSign ECC Root CA - G3`, the anchor of those intermediates). The other
  dependencies take their compatible patch releases (tokio 1.54.0, among
  others).
- **basic-toml is gone** (RUSTSEC-2026-0374, unmaintained). Askama pulled it
  in only to read an `askama.toml`, which Hora does not have: its `config`
  feature is off, the templates render the same.

## [0.11.2] - 2026-10-15

A security release: rustls moves to 0.23.46, which fixes four advisories
published on 2026-10-14. Hora uses rustls for its outbound TLS (HTTPS and
TLS probes, notification channels, the release check). No other change.

### Security

- **rustls 0.23.46.** Fixes RUSTSEC-2026-0346 (`Debug` output of client
  session values could expose TLS secrets), RUSTSEC-2026-0347 (buffered
  data sent to the outer server after an ECH rejection), RUSTSEC-2026-0348
  (Brotli certificate decompression could allocate up to 1 GiB before
  authentication) and RUSTSEC-2026-0349 (an unauthenticated ServerHello
  could downgrade later connections to classical key exchange). The other
  dependencies take their compatible patch releases (tokio 1.53.3, hyper
  1.12, aws-lc-rs 1.18.2, among others).

## [0.11.1] - 2026-10-04

Your logo on your status page, and word of new releases. The header can
show your own mark beside the title, one per theme, and Hora now tells
you when a newer version is out: on your status page, for your eyes only,
and once through your channels, with what it brings.

### Added

- **Your logo in the header.** `[page] logo = "logo.svg"` shows your own
  mark beside the title, in place of its initial tile, on every page and
  on the monthly report. SVG, PNG, WebP or JPEG, read with the config and
  served at `/logo`; `logo_dark` adds the dark theme's version, which shows
  whenever the page is dark, chosen by the system or by `?theme=dark`.
- **Hear of new Hora releases.** Once a day Hora asks GitHub for its latest
  release; a newer one shows on the status page to the operator (version,
  the opening of its changelog, links to the release and upgrade notes) and
  is sent once through the channels. `[updates] check = false` turns it
  off, `notify` picks the channels (`[]`: the banner alone). Release-watch
  messages carry the notes' opening too (`message` in the webhook payload).

## [0.11.0] - 2026-10-03

A new face and a hardening release. Hora gets its brand (the night watch,
a little owl that dozes while all is well) and a status page redesigned
around it; the findings of a full audit are fixed; and the page stays
instant on huge databases: measured on 575 million checks across 700
monitors, the status page answers in 1.5 ms at p99 (it took 13 s in
0.10.0), weighs 71 KB instead of 6.5 MB, and the daemon peaks at 353 MB of
RAM instead of 887 MB. See [UPGRADES.md](UPGRADES.md#0100--0110) for what to
check before upgrading.

### Added

- **A redesigned status page.** The hour and one sentence ("Everything is
  running."), count chips, then what needs attention first; groups that are
  all well fold away, large groups show compact rows, and each monitor's
  daily bar is one image instead of ninety elements. Every state reads as a
  colour, a shape and a word, never colour alone. Light and dark follow the
  system, `?theme=light|dark` forces one (handy for embeds and kiosks).
  Down monitors say since when ("Down · 38 min"), rows show uptime over the
  bar's days, maintenance days are marked, and recent incidents and a
  "No incident in N days" chip sit under the groups.
- **`/monitor/{id}`**: a page per monitor with its figures, the 24-hour
  chart, a day-by-day bar whose details open by keyboard or tap, the
  hour-by-day latency heatmap (moved here from `/history`), what each
  vantage point sees, and its incidents.
- **`/watchers`** (operator only): the mesh from this node, its peers'
  heartbeats, how often outages were confirmed, and each shared service as
  seen from each place.
- **"Not an outage."** A down seen from this node only, while every peer
  reaches the target, shows as up with a notice explaining why nobody was
  paged. The JSON API is unchanged.
- **`alerts.notify_unconfirmed`**: the channels that receive a local-only
  down (see Changed), validated like any `notify` route. The webhook payload
  gains `local_only` (bool) on `down` and `recovered` events.
- **A certificate that cannot be read alerts** (`cert_unreadable`: *TLS
  certificate could not be read*, with the error), once per streak and not
  while the monitor is down. It used to be a log line, and the expiry of a
  server whose STARTTLS dialogue or TLS versions changed silently stopped
  being watched.
- **The HTTP probe decodes `gzip`, `br`, `deflate` and `zstd` bodies**
  before the assertions run (a CDN that always compresses failed every
  keyword check). `max_body_kb` caps the decoded size.
- **Kiosk refresh is opt-in**: `?refresh=<secs>` (10 to 3600) reloads the
  page; without it the page no longer reloads itself every 30 seconds,
  which closed open groups and disturbed screen readers.
- **`hora compact`** gives the file's free pages back to the disk: applies
  the retention now (`--purge-removed` skips the 7-day grace), rewrites the
  file with `VACUUM INTO` and swaps it in atomically. `--dry-run` measures
  every table and index and the gain. It needs the daemon stopped: the
  daemon now holds `<db>.lock`, and CLI commands hold `<db>.writers.lock`
  so none of them writes into a file being replaced. `hora top` shows the
  database size and suggests a compaction past a fifth of free pages; the
  operator `/api/summary` reports `storage { db_bytes, reclaimable_bytes }`.
- **Add to home screen**: a web app manifest and icons, so a phone can keep
  the status page as an app (no service worker: always the live page).
- The monthly report gets a day strip per service and shorter headings, and
  is cached per month and audience.
- History, timeline, post-mortem, the empty and private-page states and the
  plain-text (curl) output share the new design and wording.
- The documentation site wears the brand, with new pages: When Hora fits,
  Concepts (how a failed check becomes one alert, or none), The status page,
  Brand, Development, and the changelog.

- **`/api/announce` takes `until` as a time of day** (`18:00`, UTC, the next
  occurrence) as well as a duration (`4h`), like `hora announce`; the hint
  in `hora top` is now true.
- **Silence a watched peer**: `POST /api/silence` and `hora silence` accept a
  peer's `listen_id`, and ad-hoc silences now mute peer-watch alerts too.
- **Degraded alerts say why**: a push with `status=degraded&msg=...` or an
  exec plugin's WARNING output is carried into the alert (and into the
  webhook's `message` field, now set on `degraded` events).
- **`hora_monitor_status{id,name,status}`** Prometheus gauge, set for every
  monitor including the ones whose status is still unknown.
- `Permissions-Policy` and `Cross-Origin-Opener-Policy` response headers.
- `hora event -- <title>` and `hora announce -- <title>` record a title that
  starts with a subcommand word (`hora announce clear skies` used to clear
  every banner).
- `just deps`: a report-only freshness check across crates, docs packages,
  pinned Actions and base images.

### Changed

- **Local-only downs no longer page.** With `confirm_with_peers`, a down
  that every peer that answered sees up used to be sent to the monitor's
  channels anyway, softened. It now goes to `alerts.notify_unconfirmed` (or
  is only recorded: incident, timeline, *Not an outage*). Only that explicit
  contradiction quiets a down: no peer, no answer, or any peer seeing it
  down alert as before. The peers are asked again every 5 minutes while it
  lasts; once they no longer all see it up, the real down goes out, once.
  A recovery goes to whoever received a down, across a daemon restart too
  (the routing is kept on the incident, migration `0022`, instant). The
  operator panel *Why nobody was woken* says what the config does with such
  a down.
- **Probe failures name their cause.** *connection failed: connection
  refused*, *connection failed: TLS invalid peer certificate: Expired*
  instead of a bare *connection failed*; *proxy failed: tunnel error: proxy
  authorization required (HTTP 407)* or *proxy failed: SOCKS error:
  credentials not accepted* instead of blaming the target; *domain does not
  exist (NXDOMAIN)* apart from *no A records found*, and *server answered
  Server Failure (rcode 2)* instead of hickory's debug text; an assertion
  judged on a body cut at `max_body_kb` says *(body cut at 256 KiB)*. A
  push to an unknown id, a cron grace of 20 seconds (*+ 20s grace*, was
  *+ 0m grace*) and an ICMP error (*icmp destination unreachable (code 1)
  from 192.0.2.1*) read as what they are.
- **An https monitor redirected to plain http is down** (*redirected to
  plain http*, public reason of the same words): following it carried the
  session in clear text without a word.
- **TCP and certificate connects try the next address after 250 ms**
  (happy-eyeballs): a dual-stack name whose IPv6 address blackholes no
  longer times out while IPv4 answers.
- **A custom `dns_resolver` falls back to TCP** for answers too large for a
  datagram (long TXT, SPF, DKIM records), which used to fail as truncated.
- **A proxied monitor's certificate is read through its proxy** (HTTP
  `CONNECT`, `socks5`, `socks5h`), like its probe; `hora probe` does the
  same. It was read by a direct dial around the proxy.
- **Certificate warnings count hours under a day** (*expires in 10 hours*;
  `hora probe` prints *10 hours left*): with less than a day left they said
  *has expired*.
- **`POST /api/push/{id}` answers 401 for an unknown id**, like a wrong
  token (it answered 404, which listed the push ids).
- **An announcement's `until` is at most a year** (`365d`); a longer one is
  refused (400, CLI error).
- A confirmation peer that does not watch the target (it answers 404) is no
  longer counted as unreachable: the verdict reads *no peer watches this
  target, unconfirmed*, and `/watchers` no longer counts it under *No
  answer*.
- A peer that misses a vantage poll keeps its last view for three rounds:
  one lost poll no longer makes its column (and the *Not an outage* notice)
  vanish for a minute.
- **The status summary is never built on a request.** A background task
  rebuilds it continuously (about 150 ms on 575M checks, was 13-20 s) and
  every page, `/api/summary`, `/metrics`, group page and peer exchange is
  served from the latest snapshot, at most a few seconds old. It is built
  once for the operator and cut in memory for the public and each group;
  pages are rendered once per snapshot and audience. Only the first request
  after start waits for the first build (logged as "status page ready").
  Announcing, clearing banners or recording an event returns once the page
  shows it.
- **24-hour figures read the hourly roll-ups** plus the current hour, not
  millions of raw checks, and hourly roll-ups now run every 5 minutes
  instead of every 6 hours. p50/p95/p99 come from per-hour latency
  histograms: exact below 64 ms, within 1.6% above.
- **Push heartbeats drive alerting with their own verdict.** A push with
  `status=down` makes the monitor down on the next tick (confirmed after
  `fail_threshold`), with the job's `msg` as the reason; `status=degraded`
  counts as degraded, so `alert_on_degraded` works for push monitors. It
  used to be read as a heartbeat like any other, so the alert came one
  interval late, as a missed heartbeat, without the message.
- **A push monitor or watched peer that never sent a heartbeat alerts**
  ("no heartbeat received yet") once its interval (or its first scheduled
  run plus grace) has passed since Hora first watched it - across restarts.
  It used to stay grey forever.
- **Alert state survives edits and restarts.** Editing a monitor or peer no
  longer re-sends a down for an ongoing outage, nor loses its recovery; a
  daemon restart mid-outage does not announce the down again (it is seeded
  from the open incident) and still sends the recovery. Monitor tasks that
  die are restarted and logged.
- **Notifications are worded once for every channel.** The ten channels
  render from one neutral message and only add their own markup and
  priority, so wording no longer drifts between them. Visible changes: ntfy,
  Gotify and Pushover bodies read `API is DOWN` / `API is slow (812ms)` /
  `API recovered` instead of `DOWN: API` / `DEGRADED:` / `RECOVERED:`; Free
  Mobile SMS use the same text (no emoji), now with fingerprints; email
  subjects are `[TAG] <headline>` (`[DOWN] API is DOWN`); Slack uses Unicode
  emoji; a certificate change carries the MITM / renewal hint everywhere.
- **Oversized notifications are cut to each API's limits** (Discord 256/4096,
  Telegram 4096, Pushover 1024, ntfy 4 KB...) with an ellipsis, instead of
  being refused with a 400 that then counted against the channel. A 413 no
  longer counts toward the failing-channel watchdog.
- **Removed monitors keep their history for 7 days.** Taking an id out of
  the config (a rename, a monitor commented out for an afternoon) used to
  delete its checks, aggregates and incidents at the next 6-hourly prune. A
  warning now names the ids and the deletion date; restoring the id within
  the week keeps everything.
- **One duration format everywhere** (`2h 5m`, `3m 10s`): the monthly report
  used to print `2h 05m` and incident phrases `1h13m`.
- **Page rate limit.** Every non-static route (pages, history, timeline,
  feed, reports, badges, heatmaps, `/metrics`, `/healthz`) is now limited per
  client IP, at four times the API burst and refill. `/report/{month}` and
  the heatmap SVG are cached for 60 seconds, and reports older than the
  12-month aggregate retention answer 400.
- **`/healthz` answers 503 when the node is degraded** (stalled scheduler,
  broken database), body unchanged - so the image's `HEALTHCHECK`, and any
  orchestrator probing it, now sees an unhealthy node. While the node is
  still starting (before the scheduler's first tick) that 503 is logged at
  debug, not as an ERROR line per health check.
- **Less memory per monitor.** Monitors share their probe client (one per
  proxy, instead of one per monitor at about 45 KB each), and the daily
  bars keep numbers instead of a formatted title per day: a node with
  10,000 monitors idles at 433 MB instead of 679 MB.
- The WAL file (`<db>-wal`) is truncated back to 64 MiB after a
  checkpoint; a burst of writes used to leave it at its high-water mark.
- **Prometheus**: `hora_monitor_up` / `hora_monitor_degraded` are omitted
  while a monitor's status is unknown (they read as down after every
  deploy); `hora_monitor_latency_ms` is typed `gauge`.
- **Atom feed** content is escaped HTML as RFC 4287 requires, entry ids point
  at `/incident/{id}` (they said `/incidents/`), each entry links to its
  post-mortem, and the feed is titled after `page.title`.
- **`POST /api/push/{id}`** answers 400 on an unknown `status` (`dwon` was
  recorded as up) or a negative `ping`. Two pushes in the same second: the
  last one wins, and a push replaces a recorded miss of the same second.
- **A second daemon on the same database refuses to start** ("locking the
  database: ... held by another Hora process") instead of racing the first.
- **Failure reasons are stored as a code** next to their text (migration
  `0021`, instant), so the public page no longer guesses them from the
  wording. An exec plugin's failure now always reads "plugin check failed"
  publicly, and a push monitor that never pinged reads "no heartbeat
  received yet".
- Settings that belong to another kind of monitor (`expected_status`,
  `headers` or `max_body_kb` on a non-http monitor, `keyword_invert`
  without `keyword`, `json_expected` without `json_query`) were silently
  ignored; they still load, now with a warning. `hora probe --kind` checks
  its target against the kind up front.
- Hora logs one warning when requests arrive through a proxy (forwarding
  headers) while `server.client_ip_header` is unset, since every visitor
  then shares the proxy's rate-limit bucket.
- OpenAPI: status fields are string enums; the JSON is unchanged.
- **Config bounds**: `interval_secs`, `timeout_secs`, `expect_every_secs`
  and `health.interval_secs` are capped at 30 days, `expected_status` must be
  100-599, an IPv6 tcp target must be bracketed (`[::1]:80`) and
  `dns_resolver` must be an IP and port. A warning flags `timeout_secs`
  above `interval_secs`.
- `hora top` never blocks on the network: requests run in the background,
  `q` and Ctrl-C always answer, and the header colour follows the API's
  `overall` state.
- Multi-vantage confirmation is never attempted for exec monitors; a peer
  asked to probe an exec or push monitor answers 400.
- Cert checks run eight at a time under a 5-minute deadline and stop on
  shutdown; monitor stagger phases shift once (a stable hash replaces
  `DefaultHasher`).
- Image: the build context is an allowlist, the cargo registry and target
  directory are BuildKit cache mounts, and the published image carries
  build provenance and an SBOM. CI Actions are pinned to commit SHAs.
- Dependencies: utoipa 6, nix 0.31 (process-group kill), docs on Astro 7.3
  and Starlight 0.42.
- Fonts: Cal Sans, Fraunces, Borel and Mona Sans Mono, served from
  `/assets/` with a content hash; the old `/assets/CalSans-SemiBold.woff2`
  route is gone. Badges and the heatmap use the brand palette.

### Deprecated

- **`?token=` on write endpoints** (push, alert, announce, silence, event).
  It still works, but the response carries `Deprecation: true` and a `Link`
  to the [authentication docs](https://uplg.github.io/hora/reference/api/#authentication),
  and Hora logs one warning per endpoint. Send `Authorization: Bearer` (or
  `X-Push-Token` for push and alert) instead. Read-only views keep
  `?token=`.

### Fixed

- **Checks were lost at every 6-hourly prune on large databases.** Once
  the history reached its retention, the prune deleted six hours of checks
  in one statement that held the write lock for 5 to 10 seconds, so the
  scheduler's inserts failed with "database is locked". Retention (and the
  sweep of a removed monitor's history) now deletes five minutes of checks
  at a time, oldest first, pausing in between; a tick spends at most two
  minutes deleting and comes back five minutes later for any backlog, so a
  much lowered retention never blocks the roll-ups or a shutdown.
- **Group pages leaked operator data**: a group token saw every group's
  deploy markers in the sparklines and the names of other groups' private
  monitors in "caused by" / "impacts". A group audience now names only
  public monitors and its own.
- **Monthly report and digest under-counted older months**: they looked at
  the 1000 (500) most recent incidents only; they now query the incidents
  that overlap the period, and the report stops at the month's end.
- **Exec monitors were "confirmed down from N/N vantage points"** when
  `[health].confirm_with_peers` was on: peers answered "not a network probe"
  as down.
- **Certificate pin mismatch accepted for good** when first seen during
  maintenance, or when the alert failed to deliver. It now alerts after the
  window and retries every check, and changing `cert_pin` to another value
  that still does not match alerts again.
- **`expected_status = 301`** (or any 3xx) could never pass: redirects were
  always followed.
- **`dns_resolver = "[2620:fe::fe]:53"`** passed validation but failed every
  probe; a hostname resolver now fails validation instead of every probe.
- **Exec timeouts left grandchildren running**: the plugin now runs in its
  own process group, killed as a whole; a plugin that exits while a
  background child holds stdout reports its real result, not "timed out",
  and that child is killed instead of piling up, one per interval.
- **ICMP monitors were up when a router answered "destination
  unreachable"**: surge-ping hands back the ICMP errors that quote the
  request as a reply, so the router's round-trip passed for the host's.
  Only an echo reply counts now.
- A cron push heartbeat that arrived slightly before its run counted as
  missed.
- A body cut mid-read was reported as a failed assertion ("keyword missing")
  instead of an invalid response body.
- Sparkline buckets jittered on every refresh.
- Cert checks did not work for HTTP monitors on an IPv6 literal.
- RDAP and release lookups: relative redirects work, an https-to-http hop is
  refused, and answers are capped at 1 MiB.
- The alert endpoint told unknown ids (404) from private ones (401).
- History and timeline pages showed fewer entries than they should when
  private monitors filled the limit.
- The plain-text page's columns were not aligned, and `/status/{group}` now
  serves it to curl too. The empty sparkline text is no longer stretched.
- `hora import kuma`: valid TOML for any name, unique ids, bracketed IPv6.
- The CLI no longer exits from deep inside a command, so the database pool
  closes cleanly; the silence reason is capped at 500 characters like the API.

### Security

- **Remote text cannot drive the operator's terminal.** A probed body, a
  push `msg`, a pushed alert or a plugin's output could carry escape
  sequences (retitle or clear the terminal, write the clipboard with OSC 52)
  into `hora incidents`, `hora postmortem` and `hora probe`. Control
  characters other than newline and tab are replaced with U+FFFD where the
  text is stored and again where the CLI prints it.
- **`/monitor/{id}` showed the raw target** (`https://user:pass@...`,
  `?api_key=...`) to group viewers; credentials are now redacted on the
  page.
- **IPv6 clients are rate-limited by their `/64`** (one subscriber holds a
  whole prefix, so a bucket per address was no limit), IPv4-mapped
  addresses as IPv4.
- **Responses to credentialed requests** (`Authorization`, `X-Push-Token`,
  `?token=`) carry `Cache-Control: no-store`; the CSP adds
  `form-action 'self'`.
- `/report/{month}` accepts digits only (`2026-+5` was a second spelling,
  and cache entry, of `2026-05`) and rendered report pages are evicted with
  their report.
- Post-mortems put the remote text of the *First failure* and timeline
  lines in a code span, so markdown in a response body (a tracking image, a
  link) stays text in the ticket it is pasted into.
- **Secrets stay on their origin.** Notifier, webhook and mesh requests
  follow a redirect only within the same origin (or an http-to-https upgrade
  on the same host): reqwest strips `Authorization` across hosts but not
  `X-Push-Token`, the Gotify key, or a Pushover body re-sent on 307/308.
  A webhook URL that redirects to another host now fails delivery.
- **Failure snapshots redact credential headers** (`Set-Cookie`,
  `Authorization`, `WWW-Authenticate`, API-key headers...) before they reach
  incidents, `/history` and post-mortems.
- Monitor and release-watch `Debug` output masks URL query values;
  `peers.witness_url` is a secret.
- `hora backup` creates the file `0600` atomically instead of tightening it
  after the copy.
- A malformed inbound `x-request-id` is replaced instead of echoed; CSP
  `img-src` no longer allows `data:`, and `style-src` no longer needs
  `'unsafe-inline'`: pages carry no inline style, and still run no script.
- The token in a query string is spliced into each response, never into
  the pages cached and shared between viewers.
- CI checkouts no longer persist the token, and the Pages write permission
  is scoped to the deploy job.

## [0.10.0] - 2026-09-21

### Added

- **Upstream release watch** (`release = { github = "owner/repo", ... }` per
  monitor): twice a day Hora asks GitHub for the project's latest release
  (no draft, no prerelease) and alerts, once per release and across restarts,
  when it is newer than the version that runs - *"matrix-construct/tuwunel
  v1.9.2 is out (running v1.9.1)"*, with the link to the notes, through the
  monitor's `notify` routing and muted during its maintenance. The running
  version is a literal (`current`) or, so it cannot drift from what is
  deployed, asked of the service itself (`current_url`, plus `current_query`,
  a JSONPath, when the answer is JSON). New `release_available` webhook event
  (`project`, `current`, `latest`, `url`); schema migration `0018`
  (`release_watch`) applies automatically. As with every new key, a config
  that uses `release` is rejected by an older binary (`deny_unknown_fields`).

## [0.9.6] - 2026-09-18

### Fixed

- **Peer polls no longer rebuild the status page.** `GET /api/peer/monitors`,
  which every mesh peer's vantage poller calls once a minute, built the whole
  authenticated summary (90 days of daily bars, percentiles, sparklines) to
  read two fields per monitor. The summary cache lives 5 seconds, so every
  poll paid for a full rebuild: on a node with months of history on 2 vCPU,
  about one second of CPU a minute, and a steady stream of sqlx
  `slow statement` warnings once the host was busy. The endpoint now reads
  only what it answers with (the recent checks behind `status`, the 24h
  percentiles behind `p50_ms`), with the same values the summary reports.

### Changed

- **Hourly roll-up follows the clock.** Raw checks used to be rolled up
  into `checks_hourly` only once 7 days old, so the status page's daily bars
  re-aggregated the last 9 days of raw checks on every rebuild (0.72 s over
  270k checks). Every ended hour is now rolled up (5-minute margin, on the
  existing maintenance tick), and `daily_all` reads the hourly buckets plus
  only the raw checks above the newest bucket, a few hours at most: about
  0.08 s on the same data. Both reads share one frontier, so they add up
  without overlap even when a roll-up commits between them. Days are also
  grouped as integer UTC day numbers instead of a per-row `strftime`
  string. Monitors whose `retention_days` is under 7 now keep hourly
  history too, since their raw checks roll up before retention prunes them.

## [0.9.5] - 2026-09-18

### Fixed

- **STARTTLS certificate checks on port 25**: the SMTP negotiation announced
  `EHLO hora`, a bare label that RFC 5321 does not allow. MX servers enforce
  the rule on port 25 (Stalwart answers `550 5.5.0 Invalid EHLO domain`), so
  the certificate of a `starttls = "smtp"` monitor on an MX was never read,
  and no expiry or pin alert could fire for it. Hora now announces the
  address literal of its end of the connection (`[192.0.2.10]`,
  `[IPv6:2001:db8::a]`), which is valid everywhere, including in a
  container whose hostname is a bare id.

### Added

- **`ehlo_name`** on `starttls = "smtp"` monitors: the identity to announce
  in `EHLO` instead of the address literal. Validated at load as a fully
  qualified domain or an RFC 5321 address literal.

### Changed

- **`croner` 4.0** (push `schedule` and `[digest]` crons). croner 4 rejects
  the shorthand steps 3.x accepted (`5/5`, `/10`); Hora parses with
  `sloppy_ranges` on, so existing schedules keep loading unchanged. Also
  `syn` 3.0.6, `rustix` 1.1.5, `cfg-if` 1.0.5 and `unicode-ident` 1.0.26.
  `yoke-derive` and `zerofrom-derive` stay on 0.8.2 / 0.1.7 (second
  `synstructure`, as in 0.9.4).
- **Alpine 3.24** runtime image (was 3.23).
- **Docs site**: `astro` 7.3.3, `@astrojs/starlight` 0.42.1.

## [0.9.4] - 2026-09-16

### Fixed

- **Matrix notifications on spec-strict homeservers** (Tuwunel, Conduit,
  conduwuit): messages went out as `POST …/rooms/{id}/send/m.room.message`
  without a transaction id, a Synapse leniency that those servers answer
  with `404 M_UNRECOGNIZED`, so every Matrix delivery failed. The notifier
  now uses the spec endpoint, `PUT …/send/m.room.message/{txnId}`, with an
  id unique per notification and reused across retries: a homeserver that
  stored the message but lost the response deduplicates instead of posting
  it twice.

### Changed

- **Dependency refresh**, TLS stack first: `rustls` 0.23.45, `tokio-rustls`
  0.26.5, `aws-lc-rs` 1.18.1 (`aws-lc-sys` 0.45), `reqwest` 0.13.5, plus
  `hickory` 0.26.3, `askama` 0.16.1, `toml` 1.1.6, `tower-http` 0.7.1 and
  the rest of the lockfile. `yoke-derive` and `zerofrom-derive` stay on
  0.8.2 / 0.1.7: their newer patches pull a second `synstructure`, which the
  duplicate-version ban rejects. The `wit-bindgen` cargo-deny skip is gone
  (one version left). `cargo audit` and `cargo deny` are clean.

## [0.9.3] - 2026-08-31

### Added

- **Badge styles**: the status and uptime badges accept `?style=flat-square`
  and `?style=for-the-badge` (the default stays `flat`). Contributed by
  [@vladkens](https://github.com/vladkens).

### Changed

- **Badge rendering** is delegated to
  [`badgelib`](https://github.com/vladkens/badgelib) instead of the built-in
  renderer; badges are slightly more compact thanks to accurate text-width
  measurement. Behaviour is otherwise unchanged - same colors, same 404 for
  an unknown or private monitor.

### Security

- **Dependency refresh**: `h2` 0.4.19 clears RUSTSEC-2026-0258 (unbounded
  queueing of empty DATA frames, low severity) and the yanked `chacha20`
  0.10.1 moves to 0.10.2.

## [0.9.2] - 2026-08-16

### Fixed

- **Slow first page build after boot**: without planner statistics `SQLite`
  favored a full index scan for the 24h aggregates (seconds on a seasoned
  database, four slow-statement warnings on the first rebuild). `PRAGMA
  optimize` now runs after migrations and after each maintenance tick; with
  statistics the same queries skip-scan in milliseconds.
- **Slow badges**: a badge fetch went through the summary cache, so a cache
  miss - the common case for a README badge - rebuilt the whole status page to
  extract one monitor's figure. Badges now answer from two indexed
  single-monitor queries, milliseconds regardless of the cache. Visibility is
  unchanged (public monitors only; a private id 404s like an unknown one).
- `lru` updated to 0.18.2, clearing RUSTSEC-2026-0253 (unsound
  `LruCache::pop()`, pulled via ratatui for `hora top`).

### Changed

- Dependencies refreshed workspace-wide (tokio 1.53, serde 1.0.229,
  async-trait 0.1.92); `tower-http` moves to 0.7 ahead of reqwest, which
  still pins 0.6 (documented cargo-deny skip until reqwest catches up).

## [0.9.1] - 2026-08-16

### Fixed

- **"database is locked" during maintenance and at boot**: the 6h maintenance
  tick rescanned the whole raw `checks` table inside write transactions - the
  downsampler re-derived every hourly bucket only to discard them, and the
  orphan sweep's anti-join full-scanned millions of rows to usually delete
  nothing - starving the scheduler's inserts and dropping check samples.
  Downsampling is now incremental (it resumes above the newest bucket), the
  orphan sweep reads first and only deletes what a config change actually
  orphaned, and the first maintenance tick waits out the boot burst (5 min).
- **Lockstep probing**: monitors sharing an interval all probed - and wrote -
  at the same instant, at boot and on every aligned tick after. Each monitor's
  cadence is now phase-shifted by a stable hash of its id (capped at one
  minute), spreading the load without changing any monitor's interval.

### Changed

- A summary rebuild (cache miss) runs its seven batched queries concurrently
  instead of sequentially; the page pays the slowest query, not the sum.

## [0.9.0] - 2026-08-15

### Added

- **Unified timeline** (`hora timeline [--days N]` / `GET /timeline`): every
  kind of recorded moment - down/recovered transitions, operator events,
  pushed alerts, announcements, silences - merged into one chronology,
  newest first, with a post-mortem link on each incident entry. Built
  entirely from data Hora already stores (the merge is a pure, tested
  function; the CLI and the page feed it different sources). Anonymous
  viewers of `/timeline` get exactly what they may see elsewhere - sanitized
  public incidents and announcements - never the operator streams.
- **`hora peers diff`** and **per-vantage latency on the status page**: a new
  authenticated mesh exchange (`GET /api/peer/monitors`, same strict
  `listen_token` model as `/api/peer/probe`) discloses a node's probeable
  monitors (kind + target, never names or credentials) with its live view of
  each (status, 24h median). On top of it:
  - `hora peers diff` compares this node's monitors with each peer's and
    exits non-zero on drift or an unreachable peer - the alignment
    `confirm_with_peers` silently relies on, now verifiable in CI.
  - each monitor card shows how the target looks *from elsewhere*
    ("Hora B: 220ms", "Hora C: down"), fed by a background poller (60s
    rounds, bounded reads, strictly fail-open: page builds read the last
    snapshot and never wait on the network). Also in `/api/summary` as a
    `vantages` array per monitor.

- **Event markers** (`hora event "deploy api v2.3"` / `POST /api/event`): the
  answer to the first diagnostic question, *"what changed?"*. A marker is one
  bounded title in a new `events` table, recorded from the CLI or a CI/deploy
  hook (`POST /api/event?title=...`, closed without `server.auth_token` - the
  same gesture as a silence). Three things happen with it:
  - **Charts**: every latency sparkline overlays the markers in its 24h span
    as dashed vertical lines (title on hover) - on the authenticated view
    only, deploy titles are operator info and never reach the public page.
  - **History**: an "Events · changes" section lists them on `/history`
    (authenticated view only, same reasoning).
  - **Correlation**: a monitor confirming down within an hour of the latest
    marker carries **"recent change: deploy api v2.3, 3m before"** in every
    notification channel (a new `event` field on down webhook payloads as
    `change`) and on the incident record itself (new `incidents.event`
    column), sanitized away from anonymous viewers unless the monitor opts in
    with `public_error_detail`. Markers age out with the pruner after a year.
- **Auto-generated post-mortems** (`hora postmortem <id|last>` /
  `/incident/{id}`): assemble everything the incident already knows - first
  failure and its reason, what the service actually answered (the stored
  snapshot), the multi-vantage verdict, the topology cause/impacted, the
  correlated change, the operator note and a start/end timeline - into
  **markdown ready to paste into a ticket**, plus a server-rendered page with
  the same content and the raw markdown in a copyable block. The multi-vantage
  verdict is now recorded onto the incident (new `incidents.vantage` column)
  when the peers answer, so the post-mortem replays what the mesh saw, not
  just what this node saw. Visibility mirrors `/history`: a private monitor's
  post-mortem answers 404 anonymously; a public one is sanitized (reason
  category, no snapshot/change/vantage) unless `public_error_detail`.
- **Certificate expiry over STARTTLS** (`starttls = "smtp" | "imap"` on a tcp
  monitor): the cert machinery - 12h expiry checks, `alerts.cert_expiry_days`
  warnings, `cert_pin` change detection - now covers the mail server's
  certificate on 587/143, the one nobody looks at until it expires. The
  watcher negotiates the protocol in plaintext (EHLO/STARTTLS for SMTP, the
  tagged STARTTLS for IMAP; every read bounded), then reads the leaf exactly
  like an https monitor. The regular tcp probe is unchanged - a connect is
  still the up/down signal. `check_cert = false` opts back out;
  `hora probe <id>` reads the certificate through the same negotiation.

### Changed

- The local quality gate moved from `make gate` to **`just gate`** (same
  recipes, a `justfile` instead of a `Makefile`).
- Dependencies refreshed across the workspace (ratatui unpinned to 0.30.2,
  surge-ping 0.9); `tower-http` stays on 0.6 until reqwest moves.

### Fixed

- **Status page slowed to a crawl as raw history accumulated**: the daily-bars
  aggregate scanned every raw check in the retention window (90 days by
  default) with a per-row `strftime`, several seconds per page/badge/summary
  request after a couple of months of data. The raw scan is now bounded to the
  last ~9 days - older days were already served by the hourly/daily buckets -
  and a new time-led covering index (migration `0017`) turns the windowed
  aggregates (daily bars, 24h availability, latency percentiles and
  sparklines) into range seeks instead of full-table scans.

## [0.8.1] - 2026-08-05

### Added

- **Numeric body assertion** (`number_regex` + `number_min`/`number_max`): an
  HTTP monitor can now extract a number from the response body - the first
  capture group, or the whole match - and go down when nothing matches or the
  value leaves the configured bounds. The assertion for pages that render a
  gauge inline with no JSON endpoint behind them: a queue depth, a stock
  level, an AIS station page whose "unique ships" counter hitting zero means
  the receiver went silent. Reasons ("number 0 below min 1") collapse to
  "content check failed" for anonymous viewers like the other body assertions.

- **Notification channel watchdog**: a delivery channel that breaks (a revoked
  Telegram bot token, a dead SMTP relay, a deleted Discord webhook) fails
  silently in the logs — you discover it during the real incident, when the
  alert that should have paged you never arrived. Hora now counts consecutive
  delivery failures per channel and, at `alerts.channel_fail_threshold` (default
  3), alerts the *other* channels: "channel 'telegram' is failing — 3
  consecutive delivery failures since 2h". The dead-man's switch applied to
  notifications themselves. Each `send_retrying` call already retries 3 times
  internally, so the default threshold represents 9 total failed attempts —
  enough to ride through a transient blip without crying wolf. The alert fires
  once per failure streak (a flag prevents re-spamming on every subsequent
  dispatch); a single successful delivery resets the counter and the flag, so a
  channel that recovers and breaks again alerts again. The failure counters
  survive a config reload (they belong to the channel *name*, not to a
  particular notifier instance), so touching an unrelated setting does not
  silently forgive a channel that has been failing for two days. Channels
  removed from the config drop their counter; channels renamed start fresh.
  - **`hora doctor`** now reports the configured notification channels
    (active vs. disabled by an empty secret), so a config with zero working
    channels — the one class of problem the watchdog itself can't warn about
    (it needs at least one working channel to reach you) — is caught at
    `doctor` time.
  - **`/api/summary`** (authenticated only) exposes a `channels` array with
    each channel's `failing` flag, `consecutive_failures` count and
    `failing_for_secs`; the public status page never sees it (channel names
    and delivery health are the operator's business, not a client's).
  - **`hora top`** shows failing channels in the trouble panel: a yellow line
    per broken channel with its failure count and duration.

## [0.8.0] - 2026-06-16

### Added

- **`POST /api/monitors/{id}/alert`**: let an external producer push its *own*
  failure straight to a monitor's notification channels - "patch
  materialization failed", "nightly export wrote 0 rows" - things a probe can
  never see. The alert fans out to the monitor's `notify` channels immediately
  and records a line on that monitor's `/history` timeline, but **never changes
  the monitor's up/down status**: status stays driven by probes and heartbeats
  alone, so a producer's hiccup can't make the status page lie. Stored in a new
  `pushed_alerts` table kept apart from `checks`/`incidents`, so it skews no
  uptime, MTTR or error-budget maths; surfaced in its own "Pushed alerts"
  section on `/history` with the same public/private visibility as incidents
  (anonymous viewers see public monitors only, and the free-form message is
  collapsed unless the monitor opts in with `public_error_detail`). Answers
  **202 Accepted**. Authenticated with the monitor's own `push_token`
  (`X-Push-Token`, kept out of access logs) or the global `server.auth_token`
  (`Authorization: Bearer` / `?token=`); the endpoint stays closed unless one
  is configured and matches.
  - **`dedup_key` + server-side anti-flood**: a repeat of the same key within
    `alerts.push_alert_window_secs` (default 300, `0` disables) is *coalesced* -
    dropped and counted, not dispatched again - and answers `202` with
    `{"status":"coalesced","suppressed":N,"retry_after_secs":…}`. The
    rate-limiting thus lives in Hora, so a flapping producer pages once and
    every producer benefits without writing its own throttle.
  - **`severity` → backend priority**: `info`/`warning`/`error`/`critical` map
    onto the native priority of ntfy, Pushover and Gotify (so `critical` pages
    louder than `info`); on the other channels the severity shows as a text
    label, and the generic JSON `webhook` channel receives it structured
    (`{"event":"alert","severity","title","message"}`).
  - **`tags`**: an optional key/value map folded into the message (sorted, so
    the same alert always reads identically).

## [0.7.2] - 2026-06-14

### Added

- **`hora probe <id|target> [--confirm] [--kind http|tcp|icmp|dns]`**: a
  one-shot ad-hoc probe from the terminal with full monitor semantics - status,
  latency, status code, HTTP/DNS assertions and TLS expiry (days left +
  notAfter for `https://` targets). A bare argument that matches a configured
  monitor id is probed with that monitor's exact config (assertions, cert pin,
  proxy, dns expectation); anything else is an ad-hoc target whose kind is
  inferred (URL → http, `host:port` → tcp, bare hostname → https, bare IP →
  icmp ping) or forced with `--kind`. **`--confirm`** asks the configured peers to probe the same target
  and prints the multi-vantage verdict from this node's perspective, both ways:
  a local *down* answers "down for everyone or just me?" ("confirmed down from
  3/3", or "seen UP by hora-b … network issue near this node?"), a local *up*
  answers "is it up from elsewhere too?" ("up from 3/3 vantage points", or "up
  here, but seen DOWN by hora-b … can they reach it?"). Reuses `probe::run`,
  `cert` and the peer-probe path unchanged. Exits non-zero when the target is
  down here, so it doubles as a scriptable health check (`hora probe url && deploy`).

## [0.7.1] - 2026-06-14

### Added

- **`hora tune [monitor_id] [--days N]`**: replay a monitor's stored check
  history against alternative anti-flap settings and recommend, per monitor,
  what to change - the project's thesis ("nobody helps tune a monitor")
  quantified on the user's own data. Pure read-only analytics over the raw
  `checks` table (which survives the full retention window, default 90 days):
  it never probes and never writes. For **`fail_threshold`** it counts how
  many down alerts each candidate value would have fired and the detection
  delay it costs ("threshold 5 -> 4 alerts instead of 11, same real outages,
  +40s"), recommending the value that sits just above the flap cluster (the
  widest gap in the failure-run lengths) so flaps are filtered but every real
  outage is still caught; it also flags a threshold so high a real multi-check
  outage went undetected. For **`degraded_over_ms`** it reports the latency
  distribution (p50/p95/p99/max) of the up checks and recommends a threshold
  near p99, warning when the current one flags normal traffic as degraded.
  **`probe_retries`** is deliberately not replayed - only a probe's final
  attempt reaches the database - so the command says so and surfaces the
  single-check-blip count, whose cross-tick lever is `fail_threshold`.

## [0.7.0] - 2026-06-12

### Added

- **`hora announce`** (and `POST /api/announce`): pin a public banner on the
  status page - "Fibre incident, ETA 6pm" - without touching the config. The
  communication side of incidents: severity-coloured, shown to every visitor
  (per-group pages included), with optional auto-expiry (`--until 4h` or
  `--until 18:00` UTC) so the classic stale "incident ongoing" banner cannot
  happen by default. Stored in the database next to the config-declared
  `[[incidents]]` banners; the API requires `server.auth_token` and busts the
  summary cache so the banner shows immediately; `hora announce list/clear`
  manage them and `DELETE /api/announce` clears remotely.
- **`hora top`**: a live terminal dashboard over the JSON API - per-monitor
  statuses, 24h uptime, p50/p95/p99, a latency sparkline for the selected
  monitor, and the current trouble, refreshed in place (default 5s).
  `--url`/`--token` point it at any Hora (the token also reads `HORA_TOKEN`,
  kept out of `ps`); without `--url` the local config's bind is used
  (wildcard binds map to loopback, so it works in `docker exec`). It acts,
  too: `a` pins an announcement (`title :: body`, `--severity`, `--until`),
  `s` silences the selected monitor, `C` clears the banners - all through
  the same authenticated API, with the outcome shown in the footer and the
  pinned banners visible in the trouble panel. Selection scrolling is
  debounced and HTTP 429 triggers a polling back-off, so the dashboard stays
  within the default per-IP rate limit. Tolerant of older/newer servers, and
  the terminal is restored even on a panic. Costs ~0.2 MB of binary.
- **Exec probes** (`kind = "exec"`): run an external check following the
  monitoring-plugins convention - exit 0 = up, 1 = degraded, anything else
  = down, first stdout line = message (`|perfdata` stripped, output bounded,
  the pipe drained so a chatty plugin never deadlocks). The whole
  Nagios/Icinga plugin ecosystem becomes usable from Hora - or a five-line
  script watching another container through a (rootless) Docker socket.
  The security model is the **`HORA_EXEC_DIR` environment variable**,
  deliberately *not* a config key: the hot-reloadable config alone must
  never be able to run code, so enabling exec probes takes deployment-level
  access too. `command` is a raw argv (no shell, no injection), `command[0]`
  is resolved strictly inside the directory (canonicalized - a planted
  symlink pointing outside is refused), plugins get a scrubbed environment
  (the daemon's own env carries channel tokens), stuck plugins are SIGKILLed
  at the monitor's timeout, and `hora doctor` verifies the directory and
  that every configured plugin is present. Exec monitors are excluded from
  multi-vantage confirmation (a local check has no remote vantage) and
  their failure detail collapses for anonymous viewers like any other
  operator detail.

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

[Unreleased]: https://github.com/uplg/hora/compare/v0.11.4...HEAD
[0.11.4]: https://github.com/uplg/hora/compare/v0.11.3...v0.11.4
[0.11.3]: https://github.com/uplg/hora/compare/v0.11.2...v0.11.3
[0.11.2]: https://github.com/uplg/hora/compare/v0.11.1...v0.11.2
[0.11.1]: https://github.com/uplg/hora/compare/v0.11.0...v0.11.1
[0.11.0]: https://github.com/uplg/hora/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/uplg/hora/compare/v0.9.6...v0.10.0
[0.9.6]: https://github.com/uplg/hora/compare/v0.9.5...v0.9.6
[0.9.5]: https://github.com/uplg/hora/compare/v0.9.4...v0.9.5
[0.9.4]: https://github.com/uplg/hora/compare/v0.9.3...v0.9.4
[0.9.3]: https://github.com/uplg/hora/compare/v0.9.2...v0.9.3
[0.9.2]: https://github.com/uplg/hora/compare/v0.9.1...v0.9.2
[0.9.1]: https://github.com/uplg/hora/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/uplg/hora/compare/v0.8.1...v0.9.0
[0.8.1]: https://github.com/uplg/hora/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/uplg/hora/compare/v0.7.2...v0.8.0
[0.7.2]: https://github.com/uplg/hora/compare/v0.7.1...v0.7.2
[0.7.1]: https://github.com/uplg/hora/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/uplg/hora/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/uplg/hora/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/uplg/hora/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/uplg/hora/compare/v0.4.2...v0.5.0
[0.4.2]: https://github.com/uplg/hora/compare/v0.4.1...v0.4.2
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
