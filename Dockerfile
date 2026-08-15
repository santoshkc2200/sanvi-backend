# syntax=docker/dockerfile:1

# ---------- planner: cargo-chef computes the dependency graph ----------
FROM rust:1.93-slim AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---------- builder: cached dependencies, then the workspace ----------
FROM rust:1.93-slim AS builder
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
ARG GIT_COMMIT=unknown
ARG BUILD_TIME=unknown
ENV GIT_COMMIT=${GIT_COMMIT} \
    BUILD_TIME=${BUILD_TIME} \
    SQLX_OFFLINE=true

RUN cargo build --release -p sanvi-api -p sanvi-worker -p sanvi-cli

# ---------- runtime: slim, non-root, no shell, healthcheck ----------
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /app --shell /usr/sbin/nologin sanvi

WORKDIR /app
COPY --from=builder /app/target/release/sanvi-api /usr/local/bin/sanvi-api
COPY --from=builder /app/target/release/sanvi-worker /usr/local/bin/sanvi-worker
COPY --from=builder /app/target/release/sanvi-cli /usr/local/bin/sanvi-cli
COPY migrations /app/migrations
COPY configs /app/configs

USER sanvi

ARG GIT_COMMIT=unknown
LABEL org.opencontainers.image.source="https://github.com/sanvi/sanvi-backend" \
      org.opencontainers.image.revision="${GIT_COMMIT}" \
      org.opencontainers.image.title="sanvi-api"

ENV SANVI_ENV=production
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["curl", "-fsS", "http://127.0.0.1:8080/healthz"] || exit 1

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/usr/local/bin/sanvi-api"]
