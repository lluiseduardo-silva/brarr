# brarr — production image.
#
# Multi-stage build that ships both the long-running orchestrator and
# the CLI in a single slim image:
#
#   /usr/local/bin/brarr-orchestrator   (default ENTRYPOINT)
#   /usr/local/bin/brarr                 (CLI; useful for `docker exec`)
#
# Build args:
#   - RUST_VERSION (default 1.95)        : matches Cargo.toml MSRV ≥ 1.85.
#   - APP_UID      (default 10001)       : numeric uid for the non-root user.
#
# Volumes:
#   /data       — sqlite db lives here (BRARR_DB_PATH=/data/brarr.db).
#   /plugins    — wasm plugin files referenced by tracker `plugin_path`.
#
# Ports:
#   3000        — admin UI (HTTP).
#   50051       — gRPC API.
#
# Required env in production:
#   BRARR_AUTH_TOKEN — opaque admin token. Unset → dev mode (logged warn).
#
# Usage:
#   docker build -t brarr:latest .
#   docker run --rm -p 3000:3000 -p 50051:50051 \
#     -v brarr-data:/data \
#     -e BRARR_AUTH_TOKEN="$(openssl rand -hex 32)" \
#     brarr:latest

# ------------------------------------------------------------------
# Stage 1 — build the workspace in release mode.
# ------------------------------------------------------------------
ARG RUST_VERSION=1.95
FROM rust:${RUST_VERSION}-slim-bookworm AS builder

# Build dependencies:
# - build-essential, cmake : C build-scripts (aws-lc-sys, etc.).
# - libssl-dev, pkg-config : reqwest default-tls (native-tls = openssl).
# - ca-certificates        : HTTPS for crates.io during `cargo fetch`.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        cmake \
        libssl-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy the entire workspace. We intentionally do **not** try to cache
# `cargo fetch` separately — the protobuf build-scripts depend on the
# in-tree proto files, so any layer reuse trick gets brittle fast.
# `.dockerignore` keeps target/, .git, and editor junk out of the
# context.
COPY . .

ENV CARGO_NET_RETRY=10 \
    CARGO_TERM_COLOR=never \
    RUST_BACKTRACE=1

RUN cargo build --release --workspace --bins

# ------------------------------------------------------------------
# Stage 2 — slim runtime.
# ------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# Runtime dependencies:
# - libssl3            : reqwest default-tls.
# - ca-certificates    : verify TLS roots for outbound HTTPS to trackers.
# - tini               : PID 1 reaper so SIGTERM reaches the orchestrator.
# - wget               : used by HEALTHCHECK against /healthz.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
        tini \
        wget \
    && rm -rf /var/lib/apt/lists/*

ARG APP_UID=10001
RUN groupadd --system --gid ${APP_UID} brarr \
    && useradd --system --uid ${APP_UID} --gid ${APP_UID} \
        --home-dir /data --shell /usr/sbin/nologin brarr \
    && mkdir -p /data /plugins /static \
    && chown -R brarr:brarr /data /plugins /static

# Static assets the orchestrator's `nest_service("/static", ...)` mounts.
COPY --chown=brarr:brarr crates/brarr-orchestrator/static /static

# Binaries.
COPY --from=builder /build/target/release/brarr-orchestrator /usr/local/bin/brarr-orchestrator
COPY --from=builder /build/target/release/brarr               /usr/local/bin/brarr

# Service defaults. Caller can override any of these with `-e VAR=...`.
ENV BRARR_DB_PATH=/data/brarr.db \
    BRARR_HTTP_ADDR=0.0.0.0:3000 \
    BRARR_GRPC_ADDR=0.0.0.0:50051 \
    BRARR_STATIC_DIR=/static \
    RUST_LOG=info

USER brarr:brarr
WORKDIR /data
VOLUME ["/data", "/plugins"]
EXPOSE 3000 50051

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD ["/usr/bin/wget", "--quiet", "--spider", "http://127.0.0.1:3000/healthz"]

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/brarr-orchestrator"]
