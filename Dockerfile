# syntax=docker/dockerfile:1

# -----------------------------------------------------------------------------
# Stage 1: Build release binaries (`fluxwright` CLI/HTTP server & `fluxwright-mcp`)
# -----------------------------------------------------------------------------
FROM rust:1-bookworm AS builder

WORKDIR /usr/src/fluxwright

# Copy workspace manifests and all workspace members defined in root Cargo.toml
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY benchmark-server ./benchmark-server
COPY benchmarks/Cargo.toml ./benchmarks/Cargo.toml
COPY benchmarks/src ./benchmarks/src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/src/fluxwright/target \
    cargo build --release --locked -p fluxwright-cli -p fluxwright-mcp \
    && mkdir -p /out \
    && cp /usr/src/fluxwright/target/release/fluxwright /out/fluxwright \
    && cp /usr/src/fluxwright/target/release/fluxwright-mcp /out/fluxwright-mcp \
    && strip /out/fluxwright /out/fluxwright-mcp

# -----------------------------------------------------------------------------
# Stage 2: Minimal Debian Bookworm runtime with system Chromium and tini
# -----------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        chromium \
        curl \
        fonts-dejavu-core \
        fonts-liberation \
        fonts-noto-color-emoji \
        tini \
    && rm -rf /var/lib/apt/lists/*

# Run as a dedicated non-root user with writable home and temporary profile directories.
# Note: The Chromium sandbox remains enabled by default; run the container with
# `--cap-add=SYS_ADMIN` or an appropriate seccomp profile, or explicitly opt out
# via `-e FLUXWRIGHT_NO_SANDBOX=1` only in environments without user namespaces.
RUN groupadd --gid 10001 fluxwright \
    && useradd --uid 10001 --gid 10001 --create-home --shell /usr/sbin/nologin fluxwright \
    && mkdir -p /tmp/fluxwright \
    && chown -R fluxwright:fluxwright /home/fluxwright /tmp/fluxwright

COPY --from=builder /out/fluxwright /usr/local/bin/fluxwright
COPY --from=builder /out/fluxwright-mcp /usr/local/bin/fluxwright-mcp

ENV FLUXWRIGHT_CHROMIUM=/usr/bin/chromium \
    HOME=/home/fluxwright \
    TMPDIR=/tmp/fluxwright \
    RUST_LOG=fluxwright=info

USER fluxwright
WORKDIR /home/fluxwright

EXPOSE 7331

HEALTHCHECK --interval=15s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:7331/healthz || exit 1

ENTRYPOINT ["/usr/bin/tini", "--", "fluxwright"]
CMD ["serve", "--listen", "0.0.0.0:7331"]
