# syntax=docker/dockerfile:1

# ---- Dependency planner (cargo-chef caches deps between builds) ----
FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---- Build ----
FROM chef AS builder
# Optimised by default (CI). On low-memory machines (< 4 GB for Docker), build with:
#   --build-arg CARGO_PROFILE_RELEASE_LTO=false --build-arg CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
ARG CARGO_PROFILE_RELEASE_LTO=thin
ARG CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
# sqlx macros check queries against the committed .sqlx metadata: no database at build time.
ENV SQLX_OFFLINE=true
RUN cargo build --release --locked --bin mouzoun-api

# ---- Runtime: tiny, no shell, non-root ----
FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /app
COPY --from=builder /app/target/release/mouzoun-api /app/mouzoun-api
ENV APP_HOST=0.0.0.0 APP_PORT=8080 LOG_JSON=true
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/app/mouzoun-api"]
