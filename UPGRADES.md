# Upgrade notes

Version-specific notes when moving between Hora releases. The general
procedure (pull the new image, recreate the container, history lives on the
`hora-data` volume) is in the [upgrading guide](https://uplg.github.io/hora/upgrading/).

## 0.11.5 → 0.12.0

Three migrations run on the first start. One rewrites the `checks` table
(clustered on monitor and time, about half the size): it takes a moment on a
large history and needs free disk space of about the database's size while
it runs. The file does not shrink by itself afterwards (the freed pages are
reused); run `hora compact` with the daemon stopped to give the space back.
Another adds a column to `incidents`, and the third creates an empty table,
`maintenance_windows`. None is undone by starting 0.11 again: back up
`hora.db` first if you may need to roll back.

New optional keys - `server.view_token`, `allow_unauthenticated` (on a push
monitor or a watched peer), `fail_threshold`, `method` and `host` on a
monitor, `host` on `[health]`, `host` and `accept_maintenance_secs` on a
peer - and a new form of an old one (`expected_status` as a list or a class): deploy
the binary before a config that uses them (`deny_unknown_fields`). This release
closes several doors that 0.11 only warned about, so **some configs no
longer load, and some scripts are refused**. Run `hora check` against your
config with the new binary first; a hot reload that fails validation keeps
the previous config. `hora check` now also prints what the loader only
warns about, with a count; `hora check --strict` exits non-zero on a
warning, if you want your deploy to stop there too. Check these before you
roll out:

- **Unset variables and empty channel secrets: nothing breaks, more is
  said.** A `${VAR}` that is not in the daemon's environment still expands
  to nothing, and a channel left without its token or URL is still left out
  while the others deliver. Both are now warned about by name at every load
  and reload (`channels[0].webhook_url: environment variable OPS_DISCORD is
  not set ...`, `channel ops: webhook_url is empty, the channel is disabled
  until it is set`). `hora check` prints and counts these warnings and still
  exits 0; **`hora check --strict` exits non-zero on them**, so a pipeline
  that adopts `--strict` must run it in the daemon's own environment (the
  same env file, the same container). Where a value really is optional, say
  so with `${VAR:-}` (no warning), or give a default with
  `${VAR:-fallback}`; the fallback also stands in for a variable that is set
  but empty, and cannot contain `{` or `}`. One difference with 0.11: a
  `${...}` that is not a variable name (`${not a name}`) is left as written,
  with a warning, where it expanded to nothing; `$${` is still the way to
  write a literal `${`.
- **A push monitor needs a `push_token`.** Without one the config fails
  with `monitor <id>: a push monitor requires a push_token ...`. Set
  `push_token = "${SOME_TOKEN}"` and send it as the `X-Push-Token` header,
  or, on a network where everyone who can reach `/api/push` is trusted, add
  `allow_unauthenticated = true` to that monitor to keep accepting a bare
  id.
- **A watched peer needs a `listen_token`.** A `[[peers]]` entry with
  `expect_every_secs` and no `listen_token` fails with `peer <id>: a
  watched peer requires a listen_token ...`. Set it (it is the other node's
  `ping_token` for you), or add `allow_unauthenticated = true` to that peer.
- **Tokens of fewer than 16 characters are refused**: `server.auth_token`,
  `server.view_token`, `[server.group_tokens]`, `push_token` and
  `listen_token` fail with `<key> is too short: use at least 16
  characters`. Generate longer ones (`openssl rand -hex 16`) and update the
  jobs, peers and bookmarks that send them. Change a peer's `listen_token`
  and the other node's `ping_token` together.
- **`?token=` is refused on write endpoints.** Push, alert, announce,
  silence, event and peer probe answer 401 (`?token= is not accepted on
  write endpoints: ...`) to any request that carries a `token` query
  parameter. It was deprecated in 0.11.0; scripts that still use it stop
  working: a cron job pinging `/api/push/<id>?token=...` reads as a missed
  heartbeat and the monitor goes down. Move them to
  `-H "X-Push-Token: ..."` (push, alert) or
  `-H "Authorization: Bearer ..."` (alert, announce, silence, event) before
  you upgrade - both headers already work on 0.11. Read-only pages keep
  `?token=`.
- **`client_ip_header = "x-forwarded-for"` behind two proxies.** The
  client's address is now taken from the right of the list, one entry per
  proxy of yours. With a CDN in front of your own reverse proxy, both
  appending, set `trusted_proxies = 2` under `[server]`, or every visitor is
  keyed on the CDN's address and shares one rate-limit bucket. One proxy, or
  a single-valued header (`x-real-ip`, `cf-connecting-ip`): nothing to do.
- **Push monitors alert sooner.** A missed heartbeat no longer waits for
  `fail_threshold`: it is down, and alerted, within a minute of its deadline
  (`interval_secs` after the last heartbeat, or the scheduled run plus
  `grace_secs`). If you relied on the threshold to absorb a late job, give
  the job more room instead: a longer `interval_secs`, or a longer
  `grace_secs` with a `schedule`.
- **Local-only downs page again, flagged.** If you use `confirm_with_peers`
  and never set `alerts.notify_unconfirmed`, a down that every peer sees up
  was only recorded; it now goes to the monitor's own channels, marked as
  seen from this node only. To keep it silent, set `notify_unconfirmed = []`
  under `[alerts]`; to send it to a quiet channel, name that channel there.
  A config that already sets `notify_unconfirmed` behaves as before.
- **`confirm_with_peers` requires https peers.** With it on, a peer whose
  `ping_url` starts with `http://` fails with `peer <id>:
  confirm_with_peers requires an https ping_url ...` (`localhost` and
  loopback addresses excepted). Put the peer behind TLS, or turn
  `confirm_with_peers` off.
- **A mesh on two versions and `hora peers diff`.** 0.12 lists its targets
  to a peer with the secrets of their URLs masked (`https://***:***@host/`,
  `?api_key=***`). Between two 0.12 nodes nothing changes: both sides are
  compared masked. A 0.12 node asking a 0.11 peer is fine too (it masks what
  the peer still sends verbatim). A **0.11** node asking a 0.12 peer compares
  its own verbatim target with the masked one: for monitors whose target
  carries a credential, `hora peers diff` run on the 0.11 node reports a
  drift that is not one, and that node's status page loses the 0.12 peer's
  line for them. Monitors without a secret in their target are unaffected.
  Upgrade the nodes that run the diff first, or ignore it until all are on
  0.12.
- **A mesh on two versions and peer confirmation.** To confirm a down, 0.12
  sends the target masked too, with a fingerprint of the check. A 0.11 node
  asking a 0.12 peer is answered as before (the peer masks what it
  receives, and without a fingerprint takes its first monitor on the
  target). A **0.12** node asking a 0.11 peer: nothing changes for a target
  without a secret (the 0.11 peer ignores the fingerprint); for a target
  that carries a credential the 0.11 peer compares the masked target with
  its verbatim one and answers that it does not watch it, so such a down is
  alerted as before but reads "no peer watches this target, unconfirmed",
  and is never made local-only, until that peer is on 0.12.
- **Peers that check one target differently stop confirming each other.**
  Between two 0.12 nodes, a monitor is confirmed only by a peer's monitor
  with the same kind, target, expected status, assertions, headers (values
  included), `dual_stack`, DNS record, pinned answer and resolver, and
  STARTTLS protocol. Timeouts, intervals, thresholds and `proxy` may
  differ. Run `hora peers diff` once the nodes are upgraded: it lists such
  monitors as "same target, checked differently". Until they are aligned
  their downs alert as usual, unconfirmed.
- **HTTP monitors that redirect to an internal address.** A monitor whose
  target resolves to a public address and redirects to a loopback,
  link-local, private or carrier-grade-NAT one now goes down with
  `redirected to a private address` (for instance a public name fronting a
  login page served on `10.x`). Point the monitor at the internal URL
  directly, or set `expected_status = 302` to assert the redirect itself.
  Targets that are internal to begin with are not affected.
- **Expiry alerts, once more.** The first sweep after the upgrade sends the
  alert of every certificate and domain already inside its warning window,
  as every restart did until now; from then on a restart is silent.
- **Webhook consumers**: `days_left` on `cert_expiring` and
  `domain_expiring` is rounded down, so an expired certificate reads -1
  (and below) instead of 0. A new `secs_left` carries the exact figure.
- **Kiosk and shared URLs**: a wall display or a bookmark that carries
  `?token=<auth_token>` holds the operator token. Set a `server.view_token`
  and put that one in the URL instead: it shows the same monitors and
  cannot write.
- **Gotify, ntfy and Pushover titles.** The title is the event's headline
  ("API is DOWN") where it was "Hora Alert", and the message no longer
  starts with that headline. Update a rule that matched the old title or
  the first line of the message.
- **A config mounted as a single file is warned about.** If your container
  mounts `config.toml` itself rather than its directory, the daemon logs a
  warning at startup and `hora check` counts one (so `--strict` fails):
  such a mount stops following the file the first time it is replaced on
  the host. Mount the directory (`./config:/etc/hora`). On a network
  filesystem, where an edit used to wait for a SIGHUP, it is now applied by
  itself within about a minute (the file is re-read every 60 s), and the
  same warning says so.
- **Logs lose their colours when not on a terminal** (`docker logs`,
  journald, a file): a filter that matched the escape codes around a level
  no longer does.
- **Text that a filter or a script may match.** Durations are written
  "3 h 7 min" where they read "3h 7m": notification text, CLI output and
  Atom titles change, and with them the post-mortem, the weekly digest, the
  webhook's `change` phrase ("deploy api v2.3, 3 min before") and the
  display labels of `/api/summary` (`uptime_label`, `budget_label`,
  `cert_label`). In the same texts a percentage and a latency take a space
  ("99.97 %", "is slow (812 ms)"), and an expiry warning reads "expires in
  3 d" instead of "expires in 3 days". Update any mail rule, chat filter or
  script that parses those strings. Numeric fields are untouched (JSON
  seconds such as `duration_s` and `secs_left`, per-mille figures,
  Prometheus metrics, the webhook's numbers and its `burn_window`), and so
  is what you type: `hora silence api 2h30m`, `duration=10m`, config keys.
  Incidents already recorded keep the wording they were stored with.
- **Planned maintenance, and mixed 0.11 / 0.12 meshes.** Nothing changes
  for a config that sets none of the new keys (`host`,
  `accept_maintenance_secs`): no window is announced with a host, and no
  announcement is accepted. A 0.12 node with windows planned through
  `/api/maintenance` adds a JSON body to its heartbeat; a 0.11 peer never
  reads a heartbeat's body, so it keeps counting the heartbeat and mutes
  nothing - it alerts for the reboot as it did before. A 0.11 node sends no
  body, which a 0.12 peer reads as "nothing planned". Upgrade both nodes,
  then add `host` and `accept_maintenance_secs`: see
  [planned maintenance of a host](https://uplg.github.io/hora/guides/peers/#planned-maintenance-of-a-host).
  A job's heartbeat body is still ignored; a peer's announcement is read
  up to 64 KiB.
- **Connections are bounded and timed.** New optional key
  `server.max_connections`: how many HTTP connections are served at once
  (default 1024; fewer when the container's memory limit is small - 128 at
  64 MiB; never more than half the open-files limit). The log line at
  startup says the number. Connections beyond it wait instead of being
  refused. A connection is now closed when it stalls: 30 s without a first
  byte, 60 s for a request to be answered, 30 s for a client that stops
  reading, 3 minutes idle on keep-alive (above the usual reverse proxies'
  own idle timeout; if yours keeps upstream connections longer, it will
  simply reopen them).
- **One client address holds a quarter of the connections at most.** New
  optional key `server.max_connections_per_ip` (0 turns it off). It does
  not apply to loopback, nor when `client_ip_header` is set. If a reverse
  proxy on another machine fronts Hora and `client_ip_header` is not set,
  set it (or the key): the proxy is otherwise held to a client's share,
  and Hora closes the connections beyond it.
- **A peer's maintenance has a daily budget**: four times its
  `accept_maintenance_secs` of muted time in any 24 hours. Nothing to do
  for a host that reboots a few times a day; a peer that announces windows
  back to back is cut, with a warning in the log.
- **Request bodies are limited to 64 KiB** on the JSON endpoints (it was
  2 MiB): a pushed alert with a larger body gets a 413.
- **`/healthz` has a new field, `probes_ok`**, and a new reason to answer
  503: the node cannot open sockets for its checks (out of file
  descriptors or memory). It skips them instead of recording failures. An
  external monitor matching `"status":"ok"` needs no change.
- **Memory.** The page cache ceiling goes from 32 to 36 MiB (nine
  connections in two pools instead of eight in one). Measured on the image
  with 300 monitors and a year of history: 18 MiB at rest, 45 MiB at the
  worst of a flood; a 64 MiB limit holds, 32 MiB does not for a fleet that
  size.
- **Building from source.** `.cargo/config.toml` now carries a compile flag
  for the bundled SQLite (`LIBSQLITE3_FLAGS`). `cargo build` in the
  repository picks it up; a packager who builds elsewhere, or sets
  `LIBSQLITE3_FLAGS` already, should add `SQLITE_DEFAULT_MEMSTATUS=0` (a
  test fails without it).

## 0.11.4 → 0.11.5

No migration, no new key. Pull the new image and recreate the container.

HTTP probes no longer reuse a connection: each check opens its own, so the
latency of an HTTPS monitor now includes the TCP and TLS handshakes every
time (it used to on the first check only). Figures go up by a handshake -
tens of milliseconds, more for a distant target - without the target having
slowed down. Raise a `degraded_over_ms` that sat just above the old figures,
and expect a step in the latency history at the upgrade.

## 0.11.3 → 0.11.4

No migration, no new key: a dependency update (tokio 1.54.1, ctutils 0.4.6)
for two advisories. Pull the new image and recreate the container. Building
from source now needs Rust 1.99.1 or later.

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
