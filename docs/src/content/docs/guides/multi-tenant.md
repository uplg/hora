---
title: Per-group pages & SLA reports
description: Lightweight multi-tenancy - per-group status pages, group tokens, and printable monthly SLA reports.
---

One Hora can host several clients' services: group the monitors, give each
client a page, a token, and a monthly report. The audience that self-hosts a
status page is exactly the audience hosting other people's sites.

## Per-group status pages

`/status/{group}` renders the status page restricted to one display group:
its monitors, its overall badge, the maintenance banners touching it - and
no peers section (the surveillance mesh is your business, not the client's).

Anonymous visitors see the group's public monitors. An unknown group - or a
fully private one viewed without a token - answers 404, revealing nothing.

## Group tokens

```toml
[server.group_tokens]
"Clients ACME" = "${ACME_TOKEN}"
```

A group token reveals the **full view of its group** - private monitors
included - **and nothing else**: it is never accepted as a global viewer
token, so handing it to a client exposes none of your other groups. Sent
like any viewer token (`Authorization: Bearer` or `?token=`):

```
https://status.example.com/status/Clients%20ACME?token=...
```

The global `server.auth_token` always works too. Tokens are validated at
load: an entry for a group no monitor belongs to fails startup (that is a
typo, not a plan).

## Monthly SLA reports

`/report/2026-05` renders a **printable** report for a calendar month (UTC):
uptime per monitor and per group, incidents, downtime clipped to the month,
MTTR (averaged over incidents resolved within the month), the SLO verdict
and the error budget consumed. Print-first design - "Save as PDF" is the
export.

Scope it to one client with `?group=`, where the group token is accepted:

```
https://status.example.com/report/2026-05?group=Clients%20ACME&token=...
```

The same report exists as text:

```sh
hora report            # last month - "here is your May report, 99.95%"
hora report 2026-05    # any month within the one-year aggregate retention
```

The running month is judged against elapsed time only, and consumption is
conservative: the covered part of the month is assumed fully monitored.
