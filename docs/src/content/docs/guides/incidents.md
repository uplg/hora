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
  instead of keeping a stale "Ongoing".

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

## Retention

Closed incidents age out after a year, alongside the daily aggregates. Open
incidents are never pruned - they are still being displayed, and close on the
next healthy tick.
