# syntax=docker/dockerfile:1
#
# For reproducible builds across machines, specify --platform:
#   docker build --platform linux/amd64 ...
#
# The cargo-chef and sccache caches are BuildKit cache mounts, so they never
# reach an image layer - but a clean-room verification build must clear them.

FROM rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS chef

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
ARG TARGETARCH
RUN case "${TARGETARCH}" in \
      amd64) sccache_arch=x86_64 ;; \
      arm64) sccache_arch=aarch64 ;; \
      *) echo "unsupported TARGETARCH '${TARGETARCH}'" >&2; exit 1 ;; \
    esac \
 && cargo install --locked "cargo-chef@${CARGO_CHEF_VERSION}" \
 && curl -fsSL "https://github.com/mozilla/sccache/releases/download/${SCCACHE_VERSION}/sccache-${SCCACHE_VERSION}-${sccache_arch}-unknown-linux-musl.tar.gz" \
    | tar xz -C /tmp \
 && mv "/tmp/sccache-${SCCACHE_VERSION}-${sccache_arch}-unknown-linux-musl/sccache" /usr/local/bin/sccache \
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
      cargo build --release --locked --package guardian-server --bin server --bin ack-keygen --features "$GUARDIAN_SERVER_FEATURES"; \
    else \
      cargo build --release --locked --package guardian-server --bin server --bin ack-keygen; \
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
FROM debian:bookworm-slim@sha256:7e490910eea2861b9664577a96b54ce68ea3e02ce7f51d89cb0103a6f9c386e0 AS benchmark-runner

RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=benchmark-builder /app/target/release/guardian-prod-benchmarks /app/guardian-prod-benchmarks
COPY --from=benchmark-builder /app/crates/contracts/masm /app/crates/contracts/masm

ENTRYPOINT ["/app/guardian-prod-benchmarks"]

# Runtime stage
FROM debian:bookworm-slim@sha256:7e490910eea2861b9664577a96b54ce68ea3e02ce7f51d89cb0103a6f9c386e0 AS server-runner

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libpq5 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the server and the ACK identity generator from builder. ack-keygen lets a
# self-managed deployment mint its stable identity from the image alone:
#   docker run --rm --user "$(id -u):$(id -g)" -v "$PWD/ack-keys:/out" <image> /app/ack-keygen --out-dir /out
COPY --from=server-builder /app/target/release/server /app/server
COPY --from=server-builder /app/target/release/ack-keygen /app/ack-keygen

# Expose HTTP and gRPC ports
EXPOSE 3000 50051

CMD ["/app/server"]
