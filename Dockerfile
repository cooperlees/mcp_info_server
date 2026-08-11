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
# Baked in by docker.yml (GIT_SHA=${{ github.sha }}, a UTC build timestamp) -
# "unknown" for any image built without them (local `docker build .`). Read
# at startup by main.rs's root-route banner; not meaningful to the app
# otherwise, so plain ENV rather than threaded through Config::from_env.
ARG GIT_SHA=unknown
ARG BUILD_DATE=unknown
ENV GIT_SHA=${GIT_SHA}
ENV BUILD_DATE=${BUILD_DATE}
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --uid 10001 mcp_info_server

COPY --from=builder /build/target/release/mcp_info_server /usr/local/bin/mcp_info_server

USER mcp_info_server
EXPOSE 6969
ENTRYPOINT ["/usr/local/bin/mcp_info_server"]
