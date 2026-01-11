# syntax=docker/dockerfile:1

# Build stage
FROM rust:latest AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    curl \
    jq \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests first for better layer caching
COPY Cargo.toml Cargo.lock ./
COPY crates/mcp-server/Cargo.toml crates/mcp-server/
COPY crates/wiki-links/Cargo.toml crates/wiki-links/
COPY crates/obsidian-fs/Cargo.toml crates/obsidian-fs/
COPY crates/semantic-embeddings/Cargo.toml crates/semantic-embeddings/
COPY crates/auth-service/Cargo.toml crates/auth-service/

# Create dummy source files for dependency compilation
RUN mkdir -p crates/mcp-server/src && echo "fn main() {}" > crates/mcp-server/src/main.rs \
    && mkdir -p crates/wiki-links/src && echo "" > crates/wiki-links/src/lib.rs \
    && mkdir -p crates/obsidian-fs/src && echo "" > crates/obsidian-fs/src/lib.rs \
    && mkdir -p crates/semantic-embeddings/src && echo "" > crates/semantic-embeddings/src/lib.rs \
    && mkdir -p crates/auth-service/src && echo "fn main() {}" > crates/auth-service/src/main.rs

# Build dependencies only (cached layer)
RUN cargo build --release -p obsidian-memory-mcp --features "http,embedded-model" --no-default-features 2>/dev/null || true

# Copy actual source code
COPY crates/ crates/
COPY scripts/ scripts/

# Download and prepare embedding model
RUN chmod +x scripts/download-model.sh && ./scripts/download-model.sh

# Touch source files to invalidate the cache for actual build
RUN touch crates/mcp-server/src/main.rs \
    crates/wiki-links/src/lib.rs \
    crates/obsidian-fs/src/lib.rs \
    crates/semantic-embeddings/src/lib.rs

# Build with embedded model and HTTP features
RUN cargo build --release -p obsidian-memory-mcp --features "http,embedded-model" --no-default-features

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user (UID 1000 for consistency with other containers)
RUN useradd -m -u 1000 memory

# Copy binary
COPY --from=builder /app/target/release/obsidian-memory /usr/local/bin/

# Create vault and config mount points
RUN mkdir -p /notes /config && chown memory:memory /notes /config

USER memory
WORKDIR /notes

EXPOSE 3000

# Health check - verify process is running (MCP endpoint is SSE, can't curl it)
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD pgrep -x obsidian-memory > /dev/null || exit 1

ENV OBSIDIAN_VAULT_PATH=/notes

ENTRYPOINT ["obsidian-memory"]
CMD ["--http", "--port", "3000", "--bind", "0.0.0.0"]
