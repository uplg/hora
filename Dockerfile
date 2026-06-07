# syntax=docker/dockerfile:1

# --- Build stage: fully static musl binary via BlackDex's musl toolchain ---
# Per-arch builder images run natively on the build host and cross-compile.
FROM --platform=$BUILDPLATFORM ghcr.io/blackdex/rust-musl:x86_64-musl-stable AS build-amd64
ENV CARGO_BUILD_TARGET=x86_64-unknown-linux-musl

FROM --platform=$BUILDPLATFORM ghcr.io/blackdex/rust-musl:aarch64-musl-stable AS build-arm64
ENV CARGO_BUILD_TARGET=aarch64-unknown-linux-musl

FROM build-${TARGETARCH} AS builder
USER root
WORKDIR /build
COPY . .
# Migrations and templates are embedded at compile time; the binary is static.
# --locked honours the committed Cargo.lock for reproducible builds.
RUN cargo build --release --locked -p hora \
    && cp "target/${CARGO_BUILD_TARGET}/release/hora" /hora

# --- Runtime stage: Alpine + CA certs (a few MB) --------------------------
FROM alpine:3.23 AS runtime

# A non-root user owns /data (a fresh named volume inherits this ownership).
RUN apk add --no-cache ca-certificates \
    && addgroup -S hora \
    && adduser -S -G hora -u 10001 hora \
    && mkdir -p /data \
    && chown hora:hora /data

COPY --from=builder /hora /usr/local/bin/hora

# Config is mounted at /etc/hora; the SQLite database lives on the /data volume.
ENV HORA_CONFIG=/etc/hora/config.toml \
    HORA_DATABASE_PATH=/data/hora.db \
    HORA_BIND=0.0.0.0:8787

VOLUME ["/data"]
EXPOSE 8787
USER 10001:10001
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
    CMD wget -qO- http://127.0.0.1:8787/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/hora"]
