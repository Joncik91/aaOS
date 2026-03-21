FROM rust:1.85-slim

RUN apt-get update && apt-get install -y \
    pkg-config libssl-dev socat \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /aaos

# Cache dependencies by copying Cargo files first
COPY Cargo.toml Cargo.lock ./
COPY crates/aaos-core/Cargo.toml crates/aaos-core/Cargo.toml
COPY crates/aaos-ipc/Cargo.toml crates/aaos-ipc/Cargo.toml
COPY crates/aaos-runtime/Cargo.toml crates/aaos-runtime/Cargo.toml
COPY crates/aaos-tools/Cargo.toml crates/aaos-tools/Cargo.toml
COPY crates/aaos-llm/Cargo.toml crates/aaos-llm/Cargo.toml
COPY crates/agentd/Cargo.toml crates/agentd/Cargo.toml

# Create stub lib.rs files so cargo can resolve the workspace
RUN mkdir -p crates/aaos-core/src && echo "" > crates/aaos-core/src/lib.rs \
    && mkdir -p crates/aaos-ipc/src && echo "" > crates/aaos-ipc/src/lib.rs \
    && mkdir -p crates/aaos-runtime/src && echo "" > crates/aaos-runtime/src/lib.rs \
    && mkdir -p crates/aaos-tools/src && echo "" > crates/aaos-tools/src/lib.rs \
    && mkdir -p crates/aaos-llm/src && echo "" > crates/aaos-llm/src/lib.rs \
    && mkdir -p crates/agentd/src && echo "fn main() {}" > crates/agentd/src/main.rs

# Pre-fetch dependencies
RUN cargo fetch

# Now copy actual source
COPY . .

# Build
RUN cargo build --workspace

# Default: run tests
CMD ["cargo", "test", "--workspace"]
