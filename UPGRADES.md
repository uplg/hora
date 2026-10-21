# Upgrade notes

Version-specific notes when moving between Hora releases. The general
procedure (pull the new image, recreate the container, history lives on the
`hora-data` volume) is in the [upgrading guide](https://uplg.github.io/hora/upgrading/).

## 0.11.2 → 0.11.3

No migration, no new key: a dependency update (rustls-webpki 0.103.16,
webpki-roots 1.0.10) for three certificate advisories, and askama without
its `config` feature. Pull the new image and recreate the container.

## 0.11.1 → 0.11.2

No migration, no new key: a dependency update (rustls 0.23.46) for four
TLS advisories. Pull the new image and recreate the container.

## 0.11.0 → 0.11.1

No migration. Two new optional keys, `[page] logo` / `logo_dark` and the
`[updates]` section: deploy the binary before a config that sets them
(`deny_unknown_fields`). One change to know:

- **Hora now asks GitHub for its latest release, once a day.** One
  anonymous request to `api.github.com`; a newer release shows on the
  operator's status page and is sent once through every channel. To route
  it, set `[updates] notify = ["<channel>"]` (`[]`: the banner alone); to
  turn it off, `[updates] check = false`. Webhook receivers get one more
  `release_available` payload, with `monitor = "Hora"` and the release's
  opening in `message`.

## 0.10.0 → 0.11.0

Four schema migrations apply automatically, in seconds even on hundreds of
millions of checks: `0019` (a `source` column on `checks` telling probes,
pushes and recorded misses apart; older rows are read exactly as before),
`0020` (a latency histogram per hourly roll-up, plus two indexes; hours
rolled up before it are read from raw checks until they age out), `0021`
(a failure reason code on checks and incidents; older rows keep the
previous wording-based reading) and `0022` (how an incident's down was
routed, so a restart mid-incident sends a local-only recovery to the quiet
channels only; schema-only, instant). One new optional key,
`alerts.notify_unconfirmed`: deploy the binary before a config that sets it
(`deny_unknown_fields`). The first page request after the upgrade waits for
the first summary build (a few seconds on a large database). Check these
before you roll out:

- **Local-only downs no longer page.** With `confirm_with_peers`, a down
  that every peer that answered sees up is no longer sent to the monitor's
  channels (it was, softened). It goes to `alerts.notify_unconfirmed` if
  you set it - a quiet channel - or is only recorded. If you relied on
  those softened alerts, add `notify_unconfirmed = ["<channel>"]` under
  `[alerts]`. Downs that no peer could judge, or that any peer also sees,
  page exactly as before.
- **ICMP monitors behind a router that answers "unreachable" turn down.**
  They were reported up (the router's ICMP error passed for the host's echo
  reply). A monitor that goes red after the upgrade was never reaching its
  host.
- **An https monitor that redirects to plain http is now down**
  (*redirected to plain http*). Point the monitor at the final https URL,
  or fix the redirect.
- **New webhook event and field.** `cert_unreadable` (with the read error
  in `message`) is a new `event`, and `down` / `recovered` carry
  `local_only`. A webhook consumer that rejects unknown events or fields
  has to learn them first.
- **One `cert_unreadable` alert may arrive after the upgrade** for every
  monitor whose certificate the watcher cannot read: those used to fail in
  the log only. Typical causes: a server that only speaks TLS 1.0/1.1, a
  STARTTLS dialogue that changed, or a proxied monitor behind an `https://`
  or `socks4://` proxy (the certificate is read through `http://`,
  `socks5://` and `socks5h://` proxies only).
- **`POST /api/push/{id}` answers 401 for an unknown id** (it was 404). A
  script that told the two apart sees a wrong token in both cases.
- **Probe failure wording is more precise** (*connection failed: connection
  refused*, *proxy failed: ...*, *domain does not exist (NXDOMAIN)*, *+ 20s
  grace*). Update any client-side filter that matched the old texts.
- **HTTP probes send `Accept-Encoding: gzip, br, deflate, zstd`** and
  decode the answer before the assertions run; a server may now answer
  compressed where it did not before. `max_body_kb` caps the decoded body.
- **Announcements**: an `until` longer than a year (`366d`) is refused.

- **Configs that no longer load.** `interval_secs`, `timeout_secs`,
  `expect_every_secs` or `health.interval_secs` above 30 days
  (2592000), an `expected_status` outside 100-599, an IPv6 tcp target
  without brackets (`::1:80` → `[::1]:80`), or a `dns_resolver` given as a
  hostname. Run `hora check` against your config with the new binary first;
  a hot reload that fails validation keeps the previous config.
- **Push monitors and peers that never sent a heartbeat start alerting**
  ("no heartbeat received yet") once their interval has passed. If you
  have monitors declared for jobs that do not run yet, comment them out
  until they do.
- **Write endpoints and `?token=`.** Push, alert, announce, silence and event
  still accept the token in the query string, but answer with a
  `Deprecation` header and log a warning once per endpoint. Move scripts to
  `Authorization: Bearer` (or `X-Push-Token` for push and alert) - see the
  [authentication docs](https://uplg.github.io/hora/reference/api/#authentication).
- **Rate limit on pages.** Pages, badges, reports and `/metrics` are now
  limited per client IP. Behind a reverse proxy, set
  `server.client_ip_header` (`x-real-ip` set by your proxy, or
  `cf-connecting-ip` behind Cloudflare), otherwise every visitor shares the
  proxy's address - and its bucket.
- **Notification wording changed** on several channels (`DOWN: API` → `API
  is DOWN` on ntfy, Gotify and Pushover; `[TAG] <headline>` email
  subjects). Update any client-side filter that matched the old prefixes.
- **Atom readers** show existing incidents once more as new: entry ids now
  point at `/incident/{id}`.
- **`/healthz` answers 503** on a degraded node. The image's `HEALTHCHECK`
  relies on it; an external monitor that matched the body still works.
- Removed monitors now keep their history for 7 days before it is deleted
  (a warning names them and the date).
- **One daemon per database.** The daemon now holds `<db>.lock` next to the
  database (the directory must be writable, as it already is for the WAL);
  a second daemon on the same file refuses to start.
- **The status page no longer reloads itself.** A wall screen or kiosk adds
  `?refresh=30` to its URL.
- **Latency percentiles** are now read from histograms: identical below
  64 ms, within 1.6% above. Prometheus quantiles follow.
- The new design moves the latency heatmaps from `/history` to each
  monitor's page (`/monitor/{id}`); the monthly report is titled "Service
  report". The JSON API is unchanged.

## 0.9.6 → 0.10.0

One schema migration, applied automatically at start (`0018`: the
`release_watch` table, empty until a monitor opts in). One new optional key,
`release` on a monitor (the upstream release watch); deploy the binary before
a config that sets it (`deny_unknown_fields`). The `release_available` event
is new on every channel and in the webhook payload (`project`, `current`,
`latest`, `url`): a webhook consumer that rejects unknown events has to learn
it before a monitor opts in. Monitors without `release` behave exactly as
before, and Hora makes no request to GitHub.

## 0.9.5 → 0.9.6

No schema migration, no config change. The first maintenance tick after the
upgrade (5 minutes after start) rolls the last 7 days of raw checks up into
hourly buckets in one pass (about 0.3 s on 270k checks); after that each tick
only rolls up the hours since the previous one. Raw checks are kept exactly
as before (`retention_days`).

## 0.9.4 → 0.9.5

No schema migration. One new optional key, `ehlo_name` on `starttls = "smtp"`
monitors; deploy the binary before a config that sets it
(`deny_unknown_fields`). Without it, the SMTP certificate check now announces
the local address literal (`[192.0.2.10]`) instead of `hora`, so a monitor on
an MX's port 25 that logged `550 Invalid EHLO domain` starts recording its
certificate with no config change.

## 0.8.0 → 0.8.1

No schema migration. New optional config keys only - as always, deploy the
binary before a config that sets them (`deny_unknown_fields`):
`alerts.channel_fail_threshold` (consecutive delivery failures before the
channel watchdog alerts the other channels, default 3), and the numeric body
assertion on http monitors (`number_regex` + `number_min`/`number_max`).
Existing monitors and channels behave exactly as before until a config opts
in.

## 0.7.2 → 0.8.0

One schema migration (the `pushed_alerts` table) applies automatically. One
new optional config key, `alerts.push_alert_window_secs` (the anti-flood
window for pushed alerts, default 300) - as always with new config keys,
deploy the binary before a config that sets it (`deny_unknown_fields`). The
new `POST /api/monitors/{id}/alert` endpoint stays closed until a monitor
`push_token` or `server.auth_token` is configured, like the other write
endpoints. No change to existing monitors: a pushed alert never affects a
monitor's up/down status.

## 0.7.1 → 0.7.2

No behavioural changes, no schema migration, no new config keys. One new
read-only subcommand, **`hora probe`**, a one-shot ad-hoc check from the
terminal (with `--confirm` for the multi-vantage verdict); it never writes and
never touches the database, so it is safe to run anytime, daemon up or down.

## 0.7.0 → 0.7.1

No behavioural changes, no schema migration, no new config keys. One new
read-only subcommand, **`hora tune`**, replays the stored check history to
recommend `fail_threshold` / `degraded_over_ms` per monitor; it never probes
and never writes, so it is safe to run against the live database.

## 0.6.0 → 0.7.0

No behavioural changes: everything is opt-in or a new subcommand. Notes:

- **Exec probes are double-gated.** `kind = "exec"` requires the
  `HORA_EXEC_DIR` environment variable (deployment-level consent, never just
  the config) and only runs executables inside that directory. Without the
  variable, a config declaring an exec monitor fails at load - intended.
- **One schema migration** (the `announcements` table) applies automatically.
- **`POST /api/announce` and `DELETE /api/announce`** exist but answer 401
  for everyone until `server.auth_token` is configured, like `/api/silence`.
- As always with new config/monitor keys (`command` on monitors): **deploy
  the binary before a config that uses them** (`deny_unknown_fields`).

## 0.5.1 → 0.6.0

No behavioural changes: everything is opt-in. Notes:

- **Multi-vantage confirmation is off by default.** To enable it, set
  `[health] confirm_with_peers = true` on the node(s) that should ask, make
  sure each `[[peers]]` entry has a `ping_url` (its origin is where probe
  requests go) and that the *responding* node sets a `listen_token` for the
  requester - `POST /api/peer/probe` strictly requires it. Both nodes must
  know the monitor (same kind + target); a peer never probes a target outside
  its own config.
- **One new endpoint** (`POST /api/peer/probe`) exists even with the feature
  off; without a matching authenticated peer it answers 401 to everyone.
- New config keys (`confirm_with_peers`, plus 0.6.0's `[digest]`,
  `server.group_tokens`) are rejected by older binaries
  (`deny_unknown_fields`): **deploy the binary before the config**, on every
  node of the mesh. One schema migration (the digest's `meta` table) applies
  automatically.

## 0.5.0 → 0.5.1

Two things to know:

- **Deploy the binary before the config.** The config is validated with
  `deny_unknown_fields`, so a config using the new keys (`domain_expiry` on a
  monitor, `alerts.domain_expiry_days`) is *rejected by a 0.5.0 binary* -
  upgrade Hora first, then ship the config that uses the new features. One
  schema migration (the `domain_expiry` table) applies automatically.
- **`hora test-alert` now exits non-zero when a channel fails** (naming the
  failing channels). A script that relied on it always exiting 0 will now
  surface broken channels - which is the point, but check your pipelines.

## 0.4.2 → 0.5.0

No behavioural changes: every addition is opt-in or a new subcommand. Notes:

- **Three schema migrations** (incident notes, ad-hoc silences, failure
  snapshots) apply automatically on first start. They only add columns and a
  table - an existing database is untouched otherwise. Take a
  `hora backup pre-0.5.db` first if you want a trivial rollback.
- **`POST /api/silence` strictly requires `server.auth_token`.** Muting
  alerts is an operator action: without a configured token the endpoint
  answers 401 for everyone. The CLI (`hora silence`) writes to the database
  directly and is not affected.
- **Failure snapshots follow the existing privacy rule**: anonymous viewers
  never see the captured response unless the monitor already opted into
  `public_error_detail`. Nothing new is exposed by default.

## 0.4.1 → 0.4.2

No behavioural changes: both additions are opt-in.

- **`dual_stack = true`** (per monitor) requires the *probing host* to have
  working IPv4 **and** IPv6. In Docker this is the catch: default bridge
  networks have no IPv6, so a dual-stack monitor would report
  "IPv6 failing" about your container's network, not your service. Enable
  IPv6 on the daemon/compose network (or use host networking) before
  turning it on.
- **`hora test-alert`** sends a clearly-labelled test notification through
  the real chain - safe to run anytime; it never touches the database, the
  incident history or the uptime numbers.

## 0.4.0 → 0.4.1

Security hardening, six behavioural changes:

- **Empty tokens fail startup.** `server.auth_token`, `push_token`,
  `listen_token` and `ping_token` set to `""` — typically a `${VAR}`
  interpolating an unset variable — were silently treated as "no token"; an
  empty token would have authorized a blank `?token=`. Hora now refuses to
  start; set the variable or remove the key.
- **`cert_pin` is validated.** It must be 64 hex chars (the SHA-256 of the
  leaf public key); anything else fails startup instead of silently never
  matching.
- **Probe headers stop at the origin.** Per-monitor `headers` (which often
  carry credentials) are no longer sent when a redirect leaves the monitor's
  scheme/host/port. A monitor that redirects to a sibling host expecting the
  same header will now fail; point `target` at the final host.
- **Rate limiting falls back to the TCP peer, not `X-Forwarded-For`.** A
  direct client could mint fresh buckets by rotating the header. Behind a
  reverse proxy you must set `server.client_ip_header` (e.g.
  `cf-connecting-ip`), otherwise all visitors now share the proxy's bucket.
- **Anonymous viewers see categorized failure reasons.** The status page,
  `/api/summary`, `/history` and the Atom feed show a public monitor's
  failure as a safe category ("HTTP 500", "content check failed", "unexpected
  DNS answer") instead of the stored detail, which can carry response-body
  snippets and DNS answers. The full reason still shows with the viewer
  token, a monitor can opt back in with `public_error_detail = true` (e.g. a
  push monitor whose `msg` is meant for the page), and topology annotations
  ("caused by", "impacts") never name a private monitor publicly either.
- **`${VAR}` expands after parsing, in string values only.** A `${VAR}`
  inside a comment is no longer looked up (commented-out examples stop
  warning about unset variables, and a TOML syntax error can no longer echo
  an expanded secret). Unquoted interpolation outside a string — e.g.
  `interval_secs = ${X}` — is no longer supported; quote it or set the value
  directly.

Also: the database file is created with `0600` permissions (existing files
keep their mode — `chmod 600` once if you care), access logs record only the
request path (query strings carried push/viewer tokens), notifier log
redaction strips every channel secret including its percent-encoded forms,
probes follow at most 10 redirects with the monitor's timeout covering
the whole chain, and Hora warns at startup when a push monitor or watched
peer has no token — the id alone authorizes `/api/push/{id}`, and ids are
discoverable on the page, the API and `/healthz`.

## 0.3 → 0.4

No breaking changes, one behavioural one. Opt-in additions: the `dns` monitor
kind, the `public` / `cert_pin` / `slo_uptime` / `schedule` monitor fields,
`server.auth_token`, the `/metrics`, `/history` and `/history.atom` endpoints,
the ntfy / Gotify / Pushover channels, and the `check` / `import` subcommands.
The incident and downsampling tables are created by migrations on first start.

Behavioural: **root-cause alert grouping is on by default**
(`alerts.group_window_secs = 30`). A monitor confirmed down while one of its
`depends_on` upstreams is also down waits up to 30 s, then folds into the
upstream's notification instead of sending its own (its recovery stays silent
too). Monitors without `depends_on` are unaffected. Set
`group_window_secs = 0` to restore one-alert-per-monitor.

## 0.2 → 0.3

No breaking changes: ICMP monitors, dependency topology (`group` /
`depends_on`) and mutual surveillance (`[health]` / `[[peers]]`) are all
opt-in additions.

## 0.1.x → 0.2

Notification config moved from per-type singletons to **named channels**, so
you can run several of the same type and route monitors to specific ones. The
fixed `HORA_*` secret variables are replaced by `${VAR}` interpolation.

```toml
# 0.1.x                              # 0.2
[telegram]                           [[channels]]
token = "…"   # or HORA_TELEGRAM_…   name = "telegram"
chat_id = "…"                        type = "telegram"
                                     token = "${HORA_TELEGRAM_TOKEN}"   # same env var still works
                                     chat_id = "…"
```

`HORA_BIND` / `HORA_DATABASE_PATH` are unchanged. If you run the container as
the non-root user for the first time on an existing volume, fix its ownership
once: `docker run --rm -v hora-data:/data alpine chown -R 10001:10001 /data`.
