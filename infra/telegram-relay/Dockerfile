# syntax=docker/dockerfile:1
# Independent edge target; certificates/config storage is writable, not the rootfs.
FROM caddy:2-alpine AS edge
USER root
RUN mkdir -p /data/caddy /config/caddy && chown -R 10001:10001 /data /config
COPY infra/telegram-relay/Caddyfile /etc/caddy/Caddyfile
USER 10001:10001

FROM rust:1-slim-bookworm AS build
WORKDIR /build
RUN apt-get update && apt-get install -y --no-install-recommends gcc libc6-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# The per-Dockerfile ignore file admits only Rust sources/manifests and Cargo.lock.
# Other workspace manifests exist for Cargo discovery; only this leaf is built.
COPY services/setups-platform-rs/Cargo.toml services/setups-platform-rs/Cargo.lock ./
COPY services/setups-platform-rs/crates ./crates
RUN --mount=type=cache,target=/build/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --locked --release -p setups-telegram-relay --bin setups-telegram-relay \
    && cp target/release/setups-telegram-relay /setups-telegram-relay

FROM debian:bookworm-slim AS relay
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /var/lib/telegram-relay \
    && chown 10001:10001 /var/lib/telegram-relay \
    && chmod 0700 /var/lib/telegram-relay
COPY --from=build --chown=10001:10001 --chmod=0555 /setups-telegram-relay /usr/local/bin/setups-telegram-relay
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/setups-telegram-relay"]
