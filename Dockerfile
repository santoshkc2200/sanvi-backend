# syntax=docker/dockerfile:1

# ---------- planner: cargo-chef computes the dependency graph ----------
FROM rust:1.97.1-slim-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---------- builder: cached dependencies, then the workspace ----------
# Must inherit from `chef`, not a fresh rust image: that's where cargo-chef
# itself was installed, and `cargo chef cook` below needs it on PATH.
# Pure rustls stack (tls-rustls-ring-webpki); no OpenSSL headers needed.
FROM chef AS builder
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

# ---------- runtime: distroless, non-root, no shell, healthcheck ----------
# cc-debian12 (glibc + libgcc, no package manager, no /bin/sh) matches what
# `rust:*-slim` links against; `:nonroot` runs as uid/gid 65532 by default,
# so no useradd step is needed. curl/tini are gone too: signals reach the
# app directly since CMD is already the raw binary (no shell to relay them
# through), and the HEALTHCHECK below is `sanvi-cli healthcheck`, a
# dependency-free HTTP probe (see apps/cli/src/healthcheck.rs) — the whole
# point of going distroless is that there is no curl to shell out to here.
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
WORKDIR /app
COPY --from=builder /app/target/release/sanvi-api /usr/local/bin/sanvi-api
COPY --from=builder /app/target/release/sanvi-worker /usr/local/bin/sanvi-worker
COPY --from=builder /app/target/release/sanvi-cli /usr/local/bin/sanvi-cli
COPY migrations /app/migrations
COPY configs /app/configs
COPY deployment/migration-policy.json /app/deployment/migration-policy.json
COPY deployment/image-contract /app/deployment/image-contract

ARG GIT_COMMIT=unknown
LABEL org.opencontainers.image.source="https://github.com/sanvi/sanvi-backend" \
      org.opencontainers.image.revision="${GIT_COMMIT}" \
      org.opencontainers.image.title="sanvi-api"

ENV SANVI_ENV=production
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/sanvi-cli", "healthcheck"]

CMD ["/usr/local/bin/sanvi-api"]
