# Upgrade notes

Version-specific notes when moving between Hora releases. The general
procedure (pull the new image, recreate the container, history lives on the
`hora-data` volume) is in the [README](README.md#upgrade).

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
