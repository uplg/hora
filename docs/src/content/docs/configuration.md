---
title: Configuration
description: The config file, environment interpolation, live reload, and validation.
---

Everything lives in one TOML file, read from `$HORA_CONFIG` (default
`./config.toml`). The annotated
[`config.example.toml`](https://github.com/uplg/hora/blob/main/config.example.toml)
documents every option; this page covers the mechanics.

## Sections

| Section | What it holds |
| --- | --- |
| `[page]` | Status page title, days rendered in the uptime bars |
| `[server]` | Bind address, database path, CORS, rate limits, viewer token |
| `[[channels]]` | Named notification channels (Telegram, Discord, email, ...) |
| `[alerts]` | Fail threshold, cert expiry warning, retention, alert grouping |
| `[[maintenance]]` | Scheduled windows that mute alerts |
| `[[incidents]]` | Manual announcements shown as a status page banner |
| `[[monitors]]` | The monitors themselves |
| `[health]` / `[[peers]]` | Mutual surveillance ([dead-man's switch](../guides/peers/)) |

## Secrets stay in the environment

Any `${VAR}` in the file is replaced with the environment variable at load:

```toml
[[channels]]
name = "ops"
type = "telegram"
token = "${HORA_TELEGRAM_TOKEN}"
chat_id = "123456"
```

An empty secret (an unset variable) **disables that channel** rather than
half-configuring it - and an empty *access token* (`auth_token`, `push_token`,
`listen_token`, `ping_token`) fails startup loudly instead of silently meaning
"no token required".

## Live reload - no blind window

To add, remove or change a monitor **without downtime**, just edit the config:

- **Bare metal / mounted directory:** Hora watches the file and reloads
  automatically.
- **Anywhere:** `kill -HUP <pid>` - in Docker, `docker kill -s HUP hora`.

On reload, unchanged monitors keep running untouched; only new, removed or
changed ones are started or stopped, and the notification channels are
rebuilt - adding a Telegram token takes effect live too. Existing checks never
pause, so there is no window where nothing is watching.

Only `server.bind` and the API rate-limit settings are read once at startup
and require a restart.

## Validation

```sh
hora check
```

validates the configuration and exits non-zero on error - made for CI and
pre-deploy hooks. Validation is strict: inverted maintenance windows, cyclic
`depends_on` graphs, private monitors without a viewer token, short or empty
tokens and malformed cron schedules are all rejected at load, not discovered
at 3 a.m.

## Retention and downsampling

Raw checks are kept per monitor (`retention_days`, default
`alerts.default_retention_days = 90`), then automatically downsampled: hourly
buckets after 7 days, daily buckets after 90, kept for a year. The daily
uptime bars keep working beyond the raw retention window, and the database
never grows forever. Closed incidents age out after a year; expired silences
are swept too.

For backups, `hora backup <dest>` snapshots the live database with SQLite's
`VACUUM INTO` - see the [CLI reference](../reference/cli/#hora-backup).
