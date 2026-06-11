---
title: Getting started
description: Run Hora with Docker or from source, write a first config, and open the status page.
---

Hora is a single self-contained binary: migrations and templates are compiled
in, history lives in one SQLite file. The fastest path is the Docker image - a
static musl binary on Alpine, about 15 MB.

## Quick start (Docker)

```sh
mkdir -p hora-config && cp config.example.toml hora-config/config.toml
# edit hora-config/config.toml

docker run -d --name hora --restart unless-stopped \
  -p 8787:8787 \
  -v "$PWD/hora-config:/etc/hora" \
  -v hora-data:/data \
  ghcr.io/uplg/hora:latest
```

The status page is at `http://localhost:8787/`. Put it behind your reverse
proxy on whatever domain you like - Hora is self-contained and assumes nothing
about who consumes it.

Your history lives on the `hora-data` volume and survives upgrades.

## A first config

```toml
[page]
title = "My services"

[server]
bind = "0.0.0.0:8787"
database_path = "/data/hora.db"

[alerts]
fail_threshold = 3        # consecutive failures before "down" - kills flapping
cert_expiry_days = 14     # warn before a TLS certificate expires

[[channels]]
name = "ops"
type = "telegram"
token = "${HORA_TELEGRAM_TOKEN}"
chat_id = "123456"

[[monitors]]
id = "website"
name = "Website"
target = "https://example.com"
interval_secs = 60
timeout_secs = 10
```

Any `${VAR}` in the file is replaced from the environment at load, so secrets
stay out of the config: pass `-e HORA_TELEGRAM_TOKEN=123:abc` to the
container. Validate with `hora check` (non-zero exit on error, CI-friendly),
and verify your notification chain with `hora test-alert` *before* the first
real incident.

## ICMP monitors in Docker

`kind = "icmp"` monitors use an unprivileged datagram socket, so they need no
extra capability as long as the container's group id is within the kernel's
`net.ipv4.ping_group_range` - Docker's default (`0 2147483647`) already covers
the image's `10001` user, **including rootless Docker**. If your host narrows
that range, either widen it
(`--sysctl net.ipv4.ping_group_range="0 2147483647"`) or grant
`--cap-add NET_RAW`; otherwise `icmp` monitors simply report down with a clear
reason.

## From source

```sh
git clone https://github.com/uplg/hora && cd hora
cp config.example.toml config.toml   # then edit
cargo run -p hora
```

Building requires a C toolchain and `cmake` (for `aws-lc-rs`, the rustls
crypto provider). The quality gate is `make gate` (fmt, clippy, cargo-deny,
cargo-audit, tests) - the exact checks CI runs.

## Environment variables

Only three are read directly (everything else lives in the config file):

| Variable | Meaning |
| --- | --- |
| `HORA_CONFIG` | Path to the config file (default `./config.toml`) |
| `HORA_DATABASE_PATH` | Overrides `server.database_path` |
| `HORA_BIND` | Overrides `server.bind` |

Plus `HORA_LOG` for log filtering (a `tracing` filter string, default `info`).

## Next steps

- [Configuration](../configuration/) - live reload, secrets, validation.
- [Monitors](../guides/monitors/) - every probe kind and its options.
- [Alerting & notifications](../guides/alerting/) - channels, routing,
  silences and maintenance windows.
