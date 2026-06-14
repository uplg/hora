# Upgrade notes

Version-specific notes when moving between Hora releases. The general
procedure (pull the new image, recreate the container, history lives on the
`hora-data` volume) is in the [README](README.md#upgrade).

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
