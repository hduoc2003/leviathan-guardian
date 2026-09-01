# syntax=docker/dockerfile:1
#
# For reproducible builds across machines, specify --platform:
#   docker build --platform linux/amd64 ...
#
# The cargo-chef and sccache caches are BuildKit cache mounts, so they never
# reach an image layer - but a clean-room verification build must clear them.

FROM rust:1.98.0-bookworm@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922 AS chef

# Install protobuf compiler (pinned to upstream 3.21.12; the Debian
# packaging revision floats so point-release rebuilds don't break the build)
# bindgen and the miden-* -sys crates need clang/libclang; git fetches the
# private crates pinned in Cargo.toml.
RUN apt-get update && apt-get install -y \
    "protobuf-compiler=3.21.12-*" \
    llvm clang libclang-dev cmake pkg-config libssl-dev libsqlite3-dev \
    git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

ARG CARGO_CHEF_VERSION=0.1.78
ARG SCCACHE_VERSION=v0.17.0
RUN cargo install --locked "cargo-chef@${CARGO_CHEF_VERSION}" \
 && curl -sSL "https://github.com/mozilla/sccache/releases/download/${SCCACHE_VERSION}/sccache-${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    | tar xz -C /tmp \
 && mv "/tmp/sccache-${SCCACHE_VERSION}-x86_64-unknown-linux-musl/sccache" /usr/local/bin/sccache \
 && chmod +x /usr/local/bin/sccache

ENV RUSTC_WRAPPER=sccache SCCACHE_DIR=/sccache CARGO_INCREMENTAL=0

# CARGO_HOME is spelled out because $HOME is unset here, which would silently
# remap nothing.
ENV SOURCE_DATE_EPOCH=0
ENV RUSTFLAGS="--remap-path-prefix /app=. --remap-path-prefix /usr/local/cargo=/cargo"

WORKDIR /app

# Recipe of the dependency graph only, so a source edit does not invalidate it.
FROM chef AS planner
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY benchmarks ./benchmarks
COPY examples ./examples
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS server-builder
ARG GUARDIAN_SERVER_FEATURES=postgres
COPY --from=planner /app/recipe.json recipe.json
RUN --mount=type=secret,id=gitea_token \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/sccache,sharing=locked \
    git config --global url."https://oauth2:$(cat /run/secrets/gitea_token)@git.softly.com/".insteadOf "https://git.softly.com/" && \
    if [ -n "$GUARDIAN_SERVER_FEATURES" ]; then \
      cargo chef cook --release --locked --recipe-path recipe.json --package guardian-server --bin server --features "$GUARDIAN_SERVER_FEATURES"; \
    else \
      cargo chef cook --release --locked --recipe-path recipe.json --package guardian-server --bin server; \
    fi && \
    rm -f /root/.gitconfig

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY benchmarks ./benchmarks
COPY examples ./examples

# build.rs reads this to stamp the git commit; the build context has no .git to fall back on.
ARG GUARDIAN_GIT_SHA

RUN --mount=type=secret,id=gitea_token \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/sccache,sharing=locked \
    git config --global url."https://oauth2:$(cat /run/secrets/gitea_token)@git.softly.com/".insteadOf "https://git.softly.com/" && \
    if [ -n "$GUARDIAN_SERVER_FEATURES" ]; then \
      cargo build --release --locked --package guardian-server --bin server --features "$GUARDIAN_SERVER_FEATURES"; \
    else \
      cargo build --release --locked --package guardian-server --bin server; \
    fi && \
    rm -f /root/.gitconfig

FROM chef AS benchmark-builder
COPY --from=planner /app/recipe.json recipe.json
RUN --mount=type=secret,id=gitea_token \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/sccache,sharing=locked \
    git config --global url."https://oauth2:$(cat /run/secrets/gitea_token)@git.softly.com/".insteadOf "https://git.softly.com/" && \
    cargo chef cook --release --locked --recipe-path recipe.json --package guardian-prod-benchmarks --bin guardian-prod-benchmarks && \
    rm -f /root/.gitconfig

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY benchmarks ./benchmarks
COPY examples ./examples

RUN --mount=type=secret,id=gitea_token \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/sccache,sharing=locked \
    git config --global url."https://oauth2:$(cat /run/secrets/gitea_token)@git.softly.com/".insteadOf "https://git.softly.com/" && \
    cargo build --release --locked --package guardian-prod-benchmarks --bin guardian-prod-benchmarks && \
    rm -f /root/.gitconfig

# Runtime stage
FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS benchmark-runner

RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=benchmark-builder /app/target/release/guardian-prod-benchmarks /app/guardian-prod-benchmarks
COPY --from=benchmark-builder /app/crates/contracts/masm /app/crates/contracts/masm

ENTRYPOINT ["/app/guardian-prod-benchmarks"]

# Runtime stage
FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS server-runner

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libpq5 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the binary from builder
COPY --from=server-builder /app/target/release/server /app/server

# Expose HTTP and gRPC ports
EXPOSE 3000 50051

CMD ["/app/server"]
