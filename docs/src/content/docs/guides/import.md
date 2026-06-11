---
title: Importing from Uptime Kuma
description: Convert an Uptime Kuma backup into Hora monitors with one command.
---

Moving off Uptime Kuma is one command:

```sh
hora import kuma backup.json > monitors.toml
```

Export the backup from Kuma (Settings → Backup → Export), feed it to
`hora import kuma`, and review the TOML it prints before pasting it into your
config.

## What maps

- **Monitor types**: http / keyword / json-query, port (→ `tcp`), ping
  (→ `icmp`), dns and push.
- **Assertions**: keywords (including inverted) and JSONPath queries with
  their expected values.
- **Details**: custom headers, intervals and timeouts, expected status codes,
  push tokens.
- **Groups**: Kuma groups become Hora display groups on the status page.

Anything Hora does not support comes out as a **commented stub** with the
original type named, so nothing is silently dropped - you decide what to do
with each one.

## After the import

```sh
hora check        # validate the merged config
hora test-alert   # verify the notification chain end to end
```

Notification channels are not part of a Kuma backup's monitor data in a form
that maps cleanly - configure your `[[channels]]` in Hora directly (it takes
a handful of lines; see [Alerting & notifications](../alerting/)).
