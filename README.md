# Hora

[![CI](https://github.com/uplg/hora/actions/workflows/ci.yml/badge.svg)](https://github.com/uplg/hora/actions/workflows/ci.yml)
[![Docker](https://github.com/uplg/hora/actions/workflows/docker.yml/badge.svg)](https://github.com/uplg/hora/actions/workflows/docker.yml)
[![Image](https://img.shields.io/badge/ghcr.io-uplg%2Fhora-2496ED?logo=docker&logoColor=white)](https://github.com/uplg/hora/pkgs/container/hora)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
![Rust edition 2024](https://img.shields.io/badge/rust-edition%202024-orange?logo=rust)

A tiny, self-hosted uptime monitor written in Rust. One small binary probes your
services, stores history in SQLite, alerts you when something breaks (or a TLS
certificate is about to expire), and serves a server-rendered status page plus a
JSON API. The Docker image is a static musl binary on Alpine — about 15 MB.

Named after the **Horai** — the Greek goddesses of the hours.

## Features

- **HTTP & TCP probes** with per-monitor interval, timeout, expected status and a
  "degraded if slower than" threshold.
- **Server-rendered status page** (no JavaScript framework): a compact, responsive
  grid of monitors — daily uptime bars and an inline SVG latency chart over the last
  24h, auto-refreshing.
- **JSON API** to read status and latency history from anywhere, with a generated
  **OpenAPI 3.1** document at `/api/openapi.json`.
- **TLS certificate expiry monitoring** with advance warnings.
- **Pluggable notifications** via a `Notifier` trait — Telegram built in, more to
  come. Alerts fire only after _N_ consecutive failures (so flapping never wakes you
  up) and include a snippet of the failing response body, so you see _what_ broke.
- **Per-IP API rate limiting** on the JSON endpoints (configurable, off the page).
- **Live config reload**: edit `config.toml` (or send `SIGHUP`) and monitors,
  thresholds, retention _and notification channels_ are reconciled in place —
  existing checks never pause, so there is no blind window.
- **Per-monitor retention** with automatic pruning; the database does not grow
  forever.
- Single self-contained binary: migrations and templates are compiled in.

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

The status page is at `http://localhost:8787/`. Put it behind your reverse proxy
on whatever domain you like — Hora is self-contained and assumes nothing about who
consumes it.

Secrets are best passed as environment variables rather than in the file:

```sh
-e HORA_TELEGRAM_TOKEN=123:abc -e HORA_TELEGRAM_CHAT_ID=456
```

## Upgrade

```sh
docker pull ghcr.io/uplg/hora:latest
docker stop hora && docker rm hora
docker run -d --name hora --restart unless-stopped \
  -p 8787:8787 \
  -v "$PWD/hora-config:/etc/hora" \
  -v hora-data:/data \
  ghcr.io/uplg/hora:latest
```

Your history lives on the `hora-data` volume and survives upgrades.

## Configuration & live reload

See [`config.example.toml`](config.example.toml) for every option. The file is
read from `$HORA_CONFIG` (default `./config.toml`).

To add, remove or change a monitor **without downtime**, just edit the config:

- **Bare metal / mounted directory:** Hora watches the file and reloads
  automatically.
- **Anywhere:** `kill -HUP <pid>` — or in Docker, `docker kill -s HUP hora`.

On reload, unchanged monitors keep running untouched; only new/removed/changed
ones are started or stopped, and the notification channels are rebuilt — so
adding a Telegram token takes effect live too. Only `server.bind` and the API
rate-limit settings are read once at startup and still require a restart.

## JSON API

| Endpoint | Description |
| --- | --- |
| `GET /` | The HTML status page. |
| `GET /api/summary` | All monitors: status, 24h uptime (per-mille), cert days left, daily history. |
| `GET /api/monitors/{id}/latency?hours=24` | Latency samples `[{ "t", "latency_ms" }]` (404 if unknown). |
| `GET /api/openapi.json` | The OpenAPI 3.1 spec, generated from the code (`utoipa`). |
| `GET /healthz` | Liveness probe. |

The `/api/*` routes are **rate-limited per client IP** (configurable; the limit is
read once at startup) and send `x-ratelimit-*` / `retry-after` headers. The client
IP comes from `X-Forwarded-For`, so run Hora behind a proxy that sets it — a direct
client could otherwise spoof it. `allowed_origins` controls CORS (empty = allow any,
since the data is read-only and public). Responses carry a strict CSP and
`X-Content-Type-Options: nosniff`.

Point any client (Bruno, Insomnia, Scalar, Swagger Editor…) at `/api/openapi.json`.

## Architecture

A small Cargo workspace:

- **`hora-notify`** — the `Notifier` trait, `Event` type, `Dispatcher`, and the
  Telegram implementation. Add a channel by implementing the trait.
- **`hora-core`** — configuration, probing, SQLite storage, TLS-expiry checks, the
  per-monitor scheduler, and the supervisor that owns live config + reconciles
  monitor tasks on reload.
- **`hora-web`** — the axum router, view model and Askama status page template.
- **`hora`** — the binary that wires it all together.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo deny check

# run locally
cp config.example.toml config.toml   # then edit
cargo run -p hora
```

Requires a C toolchain + `cmake` (for `aws-lc-rs`, the rustls crypto provider).

## License

MIT — see [LICENSE](LICENSE).

The status page embeds the [Cal Sans](https://github.com/calcom/font) font, used
under the SIL Open Font License — see
[`crates/hora-web/assets/OFL.txt`](crates/hora-web/assets/OFL.txt).
