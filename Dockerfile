# ─────────────────────────────────────────────
# Metis — multi-stage Docker build
# ─────────────────────────────────────────────
# Stage 1: build the Rust binary
# Stage 2: minimal runtime image
# ─────────────────────────────────────────────

# ── Builder ──────────────────────────────────
FROM rust:1.84-bookworm AS builder

WORKDIR /build

# Copy workspace manifests first for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY crates/metis-core/Cargo.toml crates/metis-core/Cargo.toml
COPY crates/metis-agent/Cargo.toml crates/metis-agent/Cargo.toml
COPY crates/metis-providers/Cargo.toml crates/metis-providers/Cargo.toml
COPY crates/metis-channels/Cargo.toml crates/metis-channels/Cargo.toml
COPY crates/metis-cron/Cargo.toml crates/metis-cron/Cargo.toml
COPY crates/metis-cli/Cargo.toml crates/metis-cli/Cargo.toml

# Create stub src files so cargo can resolve the workspace
RUN mkdir -p crates/metis-core/src && echo "pub fn stub(){}" > crates/metis-core/src/lib.rs && \
    mkdir -p crates/metis-agent/src && echo "pub fn stub(){}" > crates/metis-agent/src/lib.rs && \
    mkdir -p crates/metis-providers/src && echo "pub fn stub(){}" > crates/metis-providers/src/lib.rs && \
    mkdir -p crates/metis-channels/src && echo "pub fn stub(){}" > crates/metis-channels/src/lib.rs && \
    mkdir -p crates/metis-cron/src && echo "pub fn stub(){}" > crates/metis-cron/src/lib.rs && \
    mkdir -p crates/metis-cli/src && echo "fn main(){}" > crates/metis-cli/src/main.rs

# Pre-build dependencies (cached unless Cargo.toml/lock change)
RUN cargo build --release --features "telegram,discord,whatsapp,slack,email" 2>/dev/null || true

# Copy full source
COPY crates/ crates/

# Touch all source files to invalidate the stub builds
RUN find crates -name "*.rs" -exec touch {} +

# Build the real binary with all channel features
RUN cargo build --release --features "telegram,discord,whatsapp,slack,email"

# ── Bridge (Node.js) ─────────────────────────
FROM node:20-bookworm-slim AS bridge-builder

WORKDIR /bridge
COPY bridge/package.json bridge/package-lock.json* ./
RUN npm install --ignore-scripts
COPY bridge/ ./
RUN npm run build

# ── Runtime ──────────────────────────────────
FROM debian:bookworm-slim AS runtime

# Install Node.js 20 for the WhatsApp bridge sidecar
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        tmux \
        gnupg \
    && mkdir -p /etc/apt/keyrings \
    && curl -fsSL https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key \
       | gpg --dearmor -o /etc/apt/keyrings/nodesource.gpg \
    && echo "deb [signed-by=/etc/apt/keyrings/nodesource.gpg] https://deb.nodesource.com/node_20.x nodistro main" \
       > /etc/apt/sources.list.d/nodesource.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends nodejs \
    && apt-get purge -y gnupg \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*

# Create metis user
RUN useradd -m -s /bin/bash metis

# Copy binary from builder
COPY --from=builder /build/target/release/metis /usr/local/bin/metis

# Copy bundled skills
COPY --from=builder /build/crates/metis-agent/skills/ /usr/share/metis/skills/

# Copy WhatsApp bridge
COPY --from=bridge-builder /bridge/dist/ /usr/share/metis/bridge/dist/
COPY --from=bridge-builder /bridge/node_modules/ /usr/share/metis/bridge/node_modules/
COPY --from=bridge-builder /bridge/package.json /usr/share/metis/bridge/package.json

# Create config and workspace directories
RUN mkdir -p /home/metis/.metis /home/metis/workspace && \
    chown -R metis:metis /home/metis

USER metis
WORKDIR /home/metis

# Gateway default port + bridge WS port
EXPOSE 18790 3001

ENTRYPOINT ["metis"]
CMD ["status"]
