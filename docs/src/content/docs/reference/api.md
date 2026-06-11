---
title: HTTP API
description: Endpoints, authentication, rate limiting, security headers and badges.
---

Everything the status page knows is also available as JSON, plus push
heartbeats and ad-hoc silencing. A generated **OpenAPI 3.1** document lives at
`/api/openapi.json` - point any client (Bruno, Insomnia, Scalar, Swagger
Editor...) at it.

## Endpoints

| Endpoint | Description |
| --- | --- |
| `GET /` | The HTML status page - or an aligned plain-text rendering for curl/wget. |
| `GET /metrics` | Prometheus metrics (text exposition format). |
| `GET /history` | Incident history page (HTML). |
| `GET /history.atom` | Incident history as an Atom feed. |
| `GET /api/summary` | All monitors: status, 24h uptime (per-mille), p50/p95/p99 latency, cert days left, daily history; plus active incidents. |
| `GET /api/monitors/{id}/latency?hours=24` | Latency samples `[{ "t", "latency_ms" }]` (404 if unknown). |
| `POST /api/push/{id}` | Record a heartbeat for a push monitor. |
| `POST /api/silence` | Mute alerts ad hoc (deploy hook). |
| `GET /api/badge/{id}/status` | Embeddable SVG status badge. |
| `GET /api/badge/{id}/uptime` | Embeddable SVG 24h-uptime badge. |
| `GET /api/openapi.json` | The OpenAPI 3.1 spec, generated from the code. |
| `GET /healthz` | Liveness probe (this node and its view of watched peers). |

## Authentication

With `server.auth_token` set, the page, `/api/summary`,
`/api/monitors/{id}/latency`, `/metrics`, `/history` and `/history.atom`
accept the token - as `Authorization: Bearer <token>` (preferred) or
`?token=` - to include monitors marked `public = false`. Without it they
serve the public subset only; a private monitor answers exactly like a
missing one (404), so its existence is not revealed either way.

## `POST /api/push/{id}`

Record a heartbeat for a push monitor (or a watched peer). Send the token as
an `X-Push-Token` header - preferred, it stays out of proxy access logs - or
as `?token=`:

```sh
curl -fsS -X POST -H "X-Push-Token: ${TOKEN}" \
  "https://status.example.com/api/push/nightly-backup?status=up&ping=42"
```

Optional query: `status=up|down|degraded` (default up), `msg=...` (recorded
with the heartbeat, bounded), `ping=<ms>`. Answers 401 on a wrong token, 404
if the id is not a push target.

## `POST /api/silence`

Mute alerts for some monitors ad hoc - made for CI deploy hooks:

```sh
curl -fsS -X POST -H "Authorization: Bearer $HORA_TOKEN" \
  "https://status.example.com/api/silence?monitors=api,web&duration=10m&reason=deploy"
```

`monitors` is a comma-separated id list or `all`; `duration` looks like
`10m` / `1h30m` (max 7 days); `reason` is optional. **Strictly requires
`server.auth_token`** - muting alerts is an operator action, so without a
configured token the endpoint is closed. Unknown ids answer 404 (a typo'd
hook fails loudly), an unparseable duration 400. Checks keep recording; only
alerting is muted.

## Rate limiting & security headers

The `/api/*` endpoints (summary, latency, push, silence) are **rate-limited
per client IP** (configurable; read once at startup) and send
`x-ratelimit-*` / `retry-after` headers; the badges and `/api/openapi.json`
are not. The client IP is taken from `X-Forwarded-For` / `X-Real-IP` by
default, so run Hora behind a proxy that sets it - a direct client could
otherwise spoof it. Behind Cloudflare, set
`server.client_ip_header = "cf-connecting-ip"` and lock the origin down.

`allowed_origins` controls CORS (empty = allow any, since the data is
read-only and public). Responses carry a strict CSP,
`X-Content-Type-Options: nosniff` and `X-Frame-Options: DENY`, plus an
`x-request-id` (an inbound one is honoured, otherwise minted) echoed on the
response for log correlation.

## Badges

Embed a monitor's live status and 24h uptime in a README, by its config `id`:

```md
![status](https://status.example.com/api/badge/web/status)
![uptime](https://status.example.com/api/badge/web/uptime)
```

Flat shields-style SVGs: green when up / uptime is high, amber for minor
incidents, red for an outage. Badges are embeddable and unauthenticated; a
private monitor's badge is a 404, not a leak.
