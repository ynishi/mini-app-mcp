# syntax=docker/dockerfile:1.7

FROM rust:1.88-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked --package mini-app-mcp

FROM debian:bookworm-slim
LABEL io.modelcontextprotocol.server.name="io.github.ynishi/mini-app-mcp"
LABEL org.opencontainers.image.source="https://github.com/ynishi/mini-app-mcp"
LABEL org.opencontainers.image.licenses="MIT OR Apache-2.0"
LABEL org.opencontainers.image.description="Agent-First CRUD store MCP server — 1 daemon per table, schema.yaml driven, SQLite backend"

COPY --from=builder /build/target/release/mini-app-mcp /usr/local/bin/mini-app-mcp

WORKDIR /data
ENTRYPOINT ["mini-app-mcp"]
