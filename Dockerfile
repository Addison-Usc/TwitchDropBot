# Multi-stage build: compile with Rust toolchain, ship a slim runtime image.
FROM rust:1.92-slim AS build
WORKDIR /app

# System deps for reqwest/openssl.
RUN apt-get update \
  && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
  && rm -rf /var/lib/apt/lists/*

# Pre-copy manifests to leverage Docker layer caching.
COPY Cargo.toml Cargo.lock ./
COPY vendor ./vendor
COPY src ./src

RUN cargo build --release

FROM debian:bookworm-slim
WORKDIR /app

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/*

# Bring in the compiled binary.
COPY --from=build /app/target/release/twitchdrops_miner /usr/local/bin/twitchdrops_miner

# Persist auth and claimed-drop cache outside the container if desired.
VOLUME ["/app/data"]

ENTRYPOINT ["twitchdrops_miner"]
