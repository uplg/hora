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
- **Details**: custom headers, intervals and timeouts, push tokens.
- **Accepted status codes**: single codes and whole classes become
  `expected_status` (`["200-299", "401"]` → `expected_status = ["2xx", 401]`).
  A range that is not a whole class (`301-302`) has no equivalent and comes
  out as a comment.
- **Method**: `HEAD` becomes `method = "HEAD"`. Any other method than `GET`
  comes out as a comment (Hora sends no request body), and so does a `HEAD`
  on a monitor with a keyword or JSON assertion, which needs the body.
- **Retries**: `maxretries = N` becomes `fail_threshold = N + 1` on the
  monitor, the same number of failed checks in a row. Kuma retries at its
  own `retryInterval`, Hora at the monitor's interval; at 0 the line is left
  out and `[alerts] fail_threshold` applies.
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
