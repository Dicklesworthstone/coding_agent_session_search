# Build the cass binary first
FROM rust:latest AS builder

WORKDIR /build

# Copy Rust project
COPY Cargo.toml Cargo.lock ./
COPY rust-toolchain.toml ./
COPY src ./src
COPY tests ./tests
COPY build.rs ./
COPY benches ./benches

# Build release binary
RUN cargo build --release --bin cass

# Runtime image
FROM python:3.12-slim

WORKDIR /app

# Install system dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Copy the compiled cass binary from builder
COPY --from=builder /build/target/release/cass /app/cass
RUN chmod +x /app/cass

# Copy MCP server files
COPY mcp_server/mcp_server.py /app/

# Install Python dependencies directly
RUN pip install --no-cache-dir mcp starlette uvicorn httpx sse-starlette

# Create data directory
RUN mkdir -p /data

# Environment variables
ENV CASS_BINARY=/app/cass
ENV CASS_DATA_DIR=/data
ENV PORT=8080
ENV HOST=0.0.0.0

# Expose port
EXPOSE 8080

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

# Run the MCP server
CMD ["python", "mcp_server.py"]
