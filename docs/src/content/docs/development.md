---
title: Development
description: How the code is laid out, how to build and test it, and the licences.
---

Hora is a small Cargo workspace in Rust (edition 2024), MIT-licensed, at
[github.com/uplg/hora](https://github.com/uplg/hora).

## Architecture

| Crate | What it holds |
| --- | --- |
| `hora-notify` | The `Notifier` trait, the `Event` type, the dispatcher, and the ten channels (Telegram, Discord, Slack, Matrix, ntfy, Gotify, Pushover, email, Free Mobile, webhook). Add a channel by implementing the trait. |
| `hora-core` | Configuration and its validation, the probes, SQLite storage and migrations, certificate and domain checks, the per-monitor scheduler, alert grouping, the peer mesh, and the supervisor that owns the live config and reconciles monitor tasks on reload. |
| `hora-web` | The axum router, the view model, the Askama templates of the status page and its siblings, the JSON API and its OpenAPI document. |
| `hora` | The binary: it wires the rest together and holds the CLI subcommands. |

Migrations and templates are compiled into the binary, so a release is one
file. The Docker image is a static musl build on Alpine.

## Build and run

```sh
git clone https://github.com/uplg/hora && cd hora
cp config.example.toml config.toml   # then edit
cargo run -p hora
```

Building needs Rust 1.99.1 or later, a C toolchain and `cmake`, for
`aws-lc-rs` (the rustls crypto provider).

## Quality gate

`just gate` runs exactly what CI runs:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo deny check      # licences, advisories, banned crates
cargo audit
cargo test --workspace --locked
```

`just fix` applies formatting and the machine-applicable lints. `just deps`
reports what is behind on every surface (crates, these docs, pinned Actions,
base images) without changing anything.

## These docs

The site is [Starlight](https://starlight.astro.build/), in `docs/`, built
with Bun and deployed to GitHub Pages by the Docs workflow:

```sh
cd docs
bun install --frozen-lockfile
bun run dev      # http://localhost:4321/hora/
bun run build
```

The [changelog page](/hora/changelog/) renders the repository's
`CHANGELOG.md` directly, so there is one file to edit.

## Licences

Hora is under the [MIT licence](https://github.com/uplg/hora/blob/main/LICENSE).

The status page embeds the [Cal Sans](https://github.com/calcom/font) font,
under the SIL Open Font License (see
[`crates/hora-web/assets/OFL.txt`](https://github.com/uplg/hora/blob/main/crates/hora-web/assets/OFL.txt)).
This site also uses Fraunces, Borel Display and Mona Sans Mono, all under
the same licence; the licence files sit next to the fonts in
`docs/public/fonts/`.
