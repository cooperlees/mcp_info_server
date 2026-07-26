# syntax=docker/dockerfile:1
FROM rust:1-slim-bookworm AS builder
WORKDIR /build

# Cache dependency builds in their own layer: compile against a dummy main.rs
# first so `target/release/deps` is populated before any of our own source
# (which changes far more often) is copied in.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --uid 10001 mcp_info_server

COPY --from=builder /build/target/release/mcp_info_server /usr/local/bin/mcp_info_server

USER mcp_info_server
EXPOSE 6969
ENTRYPOINT ["/usr/local/bin/mcp_info_server"]
