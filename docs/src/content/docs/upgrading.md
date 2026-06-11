---
title: Upgrading
description: How to upgrade Hora, and where the version-specific notes live.
---

Hora stores everything in one SQLite file; schema migrations are embedded in
the binary and applied automatically on startup. Upgrading is: replace the
binary (or image), restart.

## Docker

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

## Version-specific notes

Behavioural changes that may need operator attention are documented in
[`UPGRADES.md`](https://github.com/uplg/hora/blob/main/UPGRADES.md) - for
example, 0.4.1 tightened security defaults (empty access tokens fail startup)
and 0.4 itself was a no-breaking-changes upgrade. The full history is in the
[CHANGELOG](https://github.com/uplg/hora/blob/main/CHANGELOG.md).

## Before you upgrade

A consistent snapshot of the database is one command, safe while the daemon
runs:

```sh
hora backup /backups/hora-pre-upgrade.db
```
