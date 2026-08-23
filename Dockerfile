# syntax=docker/dockerfile:1.7

FROM rust:1.88-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked --package mini-app-mcp --features s3-upload

FROM debian:bookworm-slim
LABEL io.modelcontextprotocol.server.name="io.github.ynishi/mini-app-mcp"
LABEL org.opencontainers.image.source="https://github.com/ynishi/mini-app-mcp"
LABEL org.opencontainers.image.licenses="MIT OR Apache-2.0"
LABEL org.opencontainers.image.description="Agent-First CRUD store MCP server — 1 daemon per table, schema.yaml driven, SQLite backend"

# ca-certificates: required for TLS to S3-compatible upload endpoints.
# curl + jq: used by the bundled backup script (contrib/backup/).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl jq \
    && rm -rf /var/lib/apt/lists/*

# supercronic: container-friendly cron for scheduled backups (see
# contrib/docker/entrypoint.sh — inert unless BACKUP_CRON is set).
ADD --checksum=sha256:a53ae236602c7338aba3fbaff40bda6300eae3b9fedb8261eb06cfe3724430c1 \
    https://github.com/aptible/supercronic/releases/download/v0.2.49/supercronic-linux-amd64 \
    /usr/local/bin/supercronic

COPY --from=builder /build/target/release/mini-app-mcp /usr/local/bin/mini-app-mcp
COPY contrib/backup/mini-app-backup.sh /usr/local/bin/mini-app-backup
COPY contrib/docker/entrypoint.sh /usr/local/bin/mini-app-entrypoint
RUN chmod +x /usr/local/bin/supercronic /usr/local/bin/mini-app-backup /usr/local/bin/mini-app-entrypoint

WORKDIR /data
ENTRYPOINT ["mini-app-entrypoint"]
