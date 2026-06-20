# Mote — Docker sandbox build.
#
# Build:    docker build -f docker/Dockerfile -t mote:latest .
# Run:      docker run -it --rm -v "$PWD:/workspace" -w /workspace mote:latest
# Wrapper:  ./docker/run.sh [workspace-dir]
#
# See docker/README.md for detailed usage.

# ── Builder stage ──────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder
WORKDIR /build

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Copy dependency manifests first to leverage Docker layer caching.
COPY Cargo.toml Cargo.lock ./
COPY protocol/Cargo.toml protocol/Cargo.toml
COPY server/Cargo.toml server/Cargo.toml
COPY client/Cargo.toml client/Cargo.toml

# Cargo still expects each workspace member to have at least one target file
# when resolving the workspace, even for `cargo fetch`.
RUN mkdir -p protocol/src server/src client/src && \
    printf 'pub fn placeholder() {}\n' > protocol/src/lib.rs && \
    printf 'fn main() {}\n' > server/src/main.rs && \
    printf 'fn main() {}\n' > client/src/main.rs

# Pre-fetch crates. This is simpler and more reliable than caching the
# target directory, because the release binaries must remain in the image
# filesystem for the later COPY --from=builder step.
RUN cargo fetch

# Copy the real source and build release binaries.
COPY protocol protocol
COPY server server
COPY client client
RUN cargo build --release -p mote-server -p mote-client

# ── Runtime stage ─────────────────────────────────────────
FROM debian:bookworm-slim

# Install runtime dependencies: ripgrep is used by the grep tool.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    ripgrep \
    && rm -rf /var/lib/apt/lists/*

RUN mkdir -p /root/.config/mote

# Copy server and client binaries from the builder stage.
COPY --from=builder /build/target/release/mote-server /usr/local/bin/
COPY --from=builder /build/target/release/mote-client /usr/local/bin/

# Default config files so the container is usable out of the box.
# Users can override by mounting their own ~/.config/mote/.
COPY config.toml.example /root/.config/mote/config.toml
COPY keybindings.toml.example /root/.config/mote/keybindings.toml
COPY auth.json.example /root/.config/mote/auth.json
COPY prompts/ /root/.config/mote/prompts/

# Default workspace inside the container.
WORKDIR /workspace

# Server port (exposed for host-client → container-server setups).
EXPOSE 9847

# Default entrypoint: start the TUI (which spawns the server automatically).
ENTRYPOINT ["mote-client"]
