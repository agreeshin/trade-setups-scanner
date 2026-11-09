# syntax=docker/dockerfile:1
FROM rust:1-slim-bookworm AS build
WORKDIR /build
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
COPY services/setups-platform-rs/ ./
RUN --mount=type=cache,target=/build/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release --bin setups-platform-server && \
    cp target/release/setups-platform-server /setups-platform-server

FROM node:24-bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends postgresql-client openssl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /setups-platform-server /usr/local/bin/setups-platform-server
COPY scripts/postgres-backup.mjs /app/scripts/postgres-backup.mjs
RUN chmod 0555 /app/scripts/postgres-backup.mjs
ENV BACKUP_SCRIPT_PATH=/app/scripts/postgres-backup.mjs
EXPOSE 8082
ENTRYPOINT ["/usr/local/bin/setups-platform-server"]
