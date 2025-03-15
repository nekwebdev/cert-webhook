FROM rust:1-alpine3.20 AS builder

# Install build dependencies - add openssl-libs-static for static linking
RUN apk add --no-cache musl-dev openssl-dev pkgconfig openssl-libs-static

# Create a new empty project
WORKDIR /app
RUN USER=root cargo new --bin cert-webhook
WORKDIR /app/cert-webhook

# Create Cargo.toml with dependencies
COPY <<EOF ./Cargo.toml
[package]
name = "cert-webhook"
version = "0.1.0"
edition = "2021"

[dependencies]
actix-web = "4.3"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
log = "0.4"
env_logger = "0.10"
kube = { version = "0.84", features = ["runtime", "derive"] }
k8s-openapi = { version = "0.18", features = ["v1_26"] }
reqwest = { version = "0.11", features = ["json"] }
base64 = "0.21"
tokio = { version = "1.27", features = ["full"] }
anyhow = "1.0"
actix-web-prom = "0.6"
futures = "0.3"
num_cpus = "1.15"

[profile.release]
opt-level = 3
lto = true
codegen-units = 1
panic = "abort"
strip = true
EOF

# Build dependencies - this will be cached unless Cargo.toml changes
RUN cargo build --release

# Remove the dummy source code and copy the real source
RUN rm src/*.rs
COPY ./main.rs ./src/main.rs

# Build the application
RUN touch src/main.rs && cargo build --release

# Create a minimal runtime image
FROM alpine:3.20

# Install SSL certificates
RUN apk add --no-cache ca-certificates

WORKDIR /app

# Copy the binary from the builder stage
COPY --from=builder /app/cert-webhook/target/release/cert-webhook /app/cert-webhook

# Set executable permissions
RUN chmod +x /app/cert-webhook

# Create a non-root user to run the application
RUN addgroup -S webhook && adduser -S webhook -G webhook
USER webhook

EXPOSE 8080

# Health check
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
  CMD wget -qO- http://localhost:8080/health || exit 1

# Run the binary
CMD ["/app/cert-webhook"]