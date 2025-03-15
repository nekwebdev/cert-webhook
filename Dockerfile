FROM rust:1-alpine3.20 AS builder

# Install build dependencies
RUN apk add --no-cache musl-dev openssl-dev pkgconfig openssl-libs-static

# Create a workspace for better caching
WORKDIR /app

# Copy only the Cargo.toml first to cache dependencies
COPY Cargo.toml ./

# Create a dummy source file to build dependencies
RUN mkdir src && \
    echo "fn main() {println!(\"dummy\")}" > src/main.rs && \
    cargo build --release && \
    rm -rf src/

# Now copy the actual source code
COPY src ./src

# Build the actual application
RUN cargo build --release

# Create a minimal runtime image
FROM alpine:3.20 AS runtime

# Define environment variables for runtime UID and GID configuration
ENV PUID=1000
ENV PGID=1000

# Install only the necessary certificates
RUN apk add --no-cache ca-certificates

WORKDIR /app

# Copy only the compiled binary
COPY --from=builder /app/target/release/cert-webhook /app/cert-webhook
RUN chmod +x /app/cert-webhook

# Add entrypoint script to handle dynamic user creation
COPY <<EOF /app/entrypoint.sh
#!/bin/sh
echo "Starting with PUID: \${PUID}, PGID: \${PGID}"

# Create group and user with specified IDs
addgroup -g \${PGID} -S webhook || echo "Group already exists"
adduser -u \${PUID} -S -G webhook webhook || echo "User already exists"

# Set ownership of application files
chown -R \${PUID}:\${PGID} /app

# Switch to webhook user and exec the command
exec su-exec webhook "\$@"
EOF

RUN chmod +x /app/entrypoint.sh && \
    apk add --no-cache su-exec

EXPOSE 8080

# Health check using built-in wget in BusyBox
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
  CMD wget -qO- http://localhost:8080/health || exit 1

ENTRYPOINT ["/app/entrypoint.sh"]
CMD ["/app/cert-webhook"]