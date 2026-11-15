# syntax=docker/dockerfile:1.7
FROM rust:1.96-slim-trixie AS build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config cmake ca-certificates && rm -rf /var/lib/apt/lists/*
COPY . .
# The commit /ping shows: read from .git/HEAD (let through by .dockerignore), or
# given: OMNI_GIT_COMMIT=$(git rev-parse --short HEAD) docker compose build
ARG OMNI_GIT_COMMIT=
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p omni-bot && cp target/release/omni /usr/local/bin/omni

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/omni /usr/local/bin/omni
WORKDIR /app
ENV OMNI_CONFIG=/app/config/omni.toml
ENTRYPOINT ["/usr/local/bin/omni"]
CMD ["run"]
