---
title: Incidents & history
description: Automatic incident records, failure snapshots, operator annotations, the history page and Atom feed.
---

Every confirmed down/up transition is recorded automatically: start, end,
duration, the failure reason, the root-cause annotation, and what the service
actually answered. The record survives restarts - an incident left open by a
crash is re-attached and closed on the next healthy tick.

## The history page and Atom feed

- `GET /history` - the incident journal as HTML, newest first.
- `GET /history.atom` - the same as an Atom feed you can subscribe to; an
  entry's `updated` moves when the incident resolves, so feed readers refresh
  instead of keeping a stale "Ongoing". Each entry links to the incident's
  post-mortem page, and the feed is titled after `[page] title`.

Both respect monitor visibility: incidents of private monitors only reach
authenticated viewers, and failure reasons shown to anonymous viewers
collapse to a safe category unless the monitor sets `public_error_detail`.

## Failure snapshots

When an HTTP probe confirms a down *with a response* - a bad status or a
failed assertion - the incident records **what the service actually
answered**: the status line, the headers and the start of the body.

```
HTTP/2.0 503 Service Unavailable
content-type: text/html
retry-after: 120

<html>upstream database unreachable</html>
```

That is the first question at 9 a.m. about the 3 a.m. alert. The capture is
bounded (24 headers, 160 chars per line, 2 KiB of body), shown on `/history`
in a collapsed *"what the service answered"* block, and as a status line in
`hora incidents`. Transport failures (timeouts, refused connections) have no
response, so no snapshot. DNS pin mismatches snapshot the full bounded answer
too - TXT records rarely fit the inline reason.

Credential headers (`Set-Cookie`, `Authorization`, `WWW-Authenticate`,
API-key headers) are redacted before the snapshot is stored.

Snapshots follow the same privacy rule as failure reasons: anonymous viewers
never see them unless the monitor opts in with `public_error_detail`.

## Operator annotations

Attach a free-form note to an incident - the human story next to the
machine's record:

```sh
hora incidents                       # list recent incidents with their ids
hora annotate 42 "fiber cut, ETA 6pm"
hora annotate last "fiber cut"       # 'last' targets the most recent incident
hora annotate 42 ""                  # an empty note clears it
```

Notes appear on `/history` and in the Atom feed. Unlike captured failure
detail, notes are written *for* visitors - they are deliberately shown to
anonymous viewers too.

## Event markers: what changed?

The first question about an outage is often *"what changed?"*. Record your
changes as they happen, from a deploy hook or by hand:

```sh
hora event "deploy api v2.3"
hora event list                      # the recent markers
hora event -- "list view redesign"   # a title that starts with 'list'
```

or over HTTP from CI (requires `server.auth_token`):

```sh
curl -fsS -X POST -H "Authorization: Bearer $HORA_TOKEN" \
  "https://status.example.com/api/event?title=deploy+api+v2.3"
```

A marker shows as a dashed line on each monitor's response-time chart
(`/monitor/{id}`) and a line on `/history` (for you, not for visitors). When a monitor confirms down within
the hour after a marker, the alert and the incident record say so: *"recent
change: deploy api v2.3, 3 min before"*.

## Post-mortems

Every incident already knows most of its own story: when it started, what
the service answered, what the peers saw, the cause and what it affected,
the change just before, your note and the timeline. Hora assembles it into
markdown ready to paste into a ticket:

```sh
hora postmortem 42
hora postmortem last
```

The same is at `/incident/{id}`, with the raw markdown to copy. Visitors get
the sanitized version (and a 404 for a private monitor's incident).

## The timeline

`hora timeline` (default 7 days, `--days N`) and the `/timeline` page merge
everything into one chronology, newest first: downs and recoveries, event
markers, pushed alerts, announcements, silences and the maintenance
windows planned at runtime (with who planned them: this node, or the peer
that announced it). *"What happened this week?"* in one command. Visitors
see public incidents and announcements only.

## Latency heatmaps

Each monitor's page (`/monitor/{id}`) shows a smokeping-style **heatmap**:
hours by days over the last 28 days, each cell coloured by how
slow that hour was *relative to the monitor's own median*. A weekly pattern -
"slow every Monday at 9 a.m." - shows up as a horizontal stripe rhythm, with
no threshold to tune and zero false-positive risk (it never alerts).

The same heatmap is an image at `GET /api/monitors/{id}/heatmap.svg`, with
the same visibility rules as the latency endpoint. Each cell's tooltip
carries the exact hour and average latency.

## Retention

Closed incidents age out after a year, alongside the daily aggregates. Open
incidents are never pruned: they are still being displayed, and close on the
next healthy tick. The incidents of a monitor removed from the config are
kept for 7 days, in case it comes back (see
[Removed monitors](../../configuration/#removed-monitors)).
