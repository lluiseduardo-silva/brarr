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
# Global ARGs — declared before any FROM so each stage's FROM line
# can reference them. Buildx scopes ARGs declared *after* a FROM to
# that single stage, so re-declaring them here at the top is the only
# way to use them in subsequent FROMs (or in different stages).
# ------------------------------------------------------------------
ARG TAILWIND_VERSION=v4.1.16
ARG RUST_VERSION=1.95
ARG APP_UID=10001

# ------------------------------------------------------------------
# Stage 1a — compile the Tailwind v4 bundle.
#
# Uses the upstream standalone binary (no Node), pinned by checksum
# in the install script to keep CI reproducible. The output lands at
# /css/app.css and is copied into the runtime image alongside the
# other static assets.
# ------------------------------------------------------------------
FROM debian:bookworm-slim AS css-builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Re-import the global ARGs we need inside this stage. ARGs only
# inherit into a stage's RUN/ENV instructions when re-declared here.
ARG TAILWIND_VERSION
ARG TARGETARCH
WORKDIR /css

# Pick the right Tailwind asset for the build platform. We support
# x86_64 + arm64 hosts (covers GHA runners + Apple Silicon laptops).
RUN set -eu; \
    case "${TARGETARCH:-amd64}" in \
      amd64) ASSET=tailwindcss-linux-x64 ;; \
      arm64) ASSET=tailwindcss-linux-arm64 ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    curl --fail --location --silent --show-error \
      --output /usr/local/bin/tailwindcss \
      "https://github.com/tailwindlabs/tailwindcss/releases/download/${TAILWIND_VERSION}/${ASSET}"; \
    chmod +x /usr/local/bin/tailwindcss

# Only the inputs the compiler reads — keeps this layer's cache hot.
COPY crates/brarr-orchestrator/styles/    /css/styles/
COPY crates/brarr-orchestrator/templates/ /css/templates/
COPY crates/brarr-orchestrator/src/       /css/src/

RUN tailwindcss --input /css/styles/input.css --output /css/app.css --minify

# ------------------------------------------------------------------
# Stage 1b — build the workspace in release mode.
# ------------------------------------------------------------------
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

# Re-import the global ARG inside this stage so the `${APP_UID}` token
# in the RUN below expands to the build-arg value (or its default).
ARG APP_UID
RUN groupadd --system --gid ${APP_UID} brarr \
    && useradd --system --uid ${APP_UID} --gid ${APP_UID} \
        --home-dir /data --shell /usr/sbin/nologin brarr \
    && mkdir -p /data /plugins /static \
    && chown -R brarr:brarr /data /plugins /static

# Static assets the orchestrator's `nest_service("/static", ...)` mounts.
# The CSS bundle comes from stage 1a (Tailwind compile); everything
# else (JS, icons, future assets) is checked in to the repo.
COPY --chown=brarr:brarr crates/brarr-orchestrator/static /static
COPY --chown=brarr:brarr --from=css-builder /css/app.css /static/app.css

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
