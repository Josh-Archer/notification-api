# Use the official Rust image as a builder
FROM rust:1.88-slim AS builder

# Create app directory
WORKDIR /usr/src/app

# Install system dependencies (for musl-based static builds, add musl-tools if needed)
RUN apt-get update && apt-get install -y pkg-config libssl-dev build-essential ca-certificates

# Copy manifests and dependencies
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Build release version
RUN cargo build --release

# ---- Runtime Image ----
FROM debian:bookworm-slim

# Create app directory
WORKDIR /app

# Install runtime dependencies (optional: remove if statically linked)
RUN apt-get update && apt-get install -y libssl3 ca-certificates && rm -rf /var/lib/apt/lists/*

# Copy binary from builder
COPY --from=builder /usr/src/app/target/release/notification-api .

# Set environment variables for secrets (to be injected at runtime)
ENV PUSHOVER_TOKEN=""
ENV PUSHOVER_USER=""

# Copy .env file for non-sensitive config
COPY .env .

# Expose port your app listens on
EXPOSE 3000

# Optional: Create config and logs dirs
RUN mkdir /config /logs

# Set default command
CMD ["./notification-api"]
