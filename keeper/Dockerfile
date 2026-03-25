FROM rust:1.87-slim as builder
WORKDIR /app

# Copy Cargo files first for layer caching
COPY Cargo.toml Cargo.lock ./

# Dummy build to cache dependencies
RUN mkdir src && echo "fn main(){}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

# Copy real source and config
COPY src ./src
COPY config.toml ./

# Rebuild (touch main.rs to force recompile)
RUN touch src/main.rs && cargo build --release

# ── Runtime image ──────────────────────────────────────────────────────────────
FROM debian:bookworm-slim
# alloy uses rustls which requires CA certificates
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/keeper .
COPY --from=builder /app/config.toml .

CMD ["./keeper"]
