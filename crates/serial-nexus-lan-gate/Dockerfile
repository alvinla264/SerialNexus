# syntax=docker/dockerfile:1
#
# The SerialNexus Docker LAN gate image.
#
# Both containers run this one image; the orchestrator picks the role with the
# command (`host` or `client`). The build stage compiles the real workspace
# binaries, so the image cannot silently test a stale or locally built binary.
#
# Images are pinned by digest. `scripts/lan-docker/run.sh` passes them as build
# arguments from `pins.env`, where the justification for each pin lives.

# Both base images are required and pinned by digest in
# scripts/lan-docker/pins.env, which the orchestrator passes here; that file is
# the single source of truth for the digests.
ARG RUST_IMAGE
ARG RUNTIME_IMAGE

FROM ${RUST_IMAGE} AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
        -p serial-nexus-core \
        -p serial-nexus-terminal \
        -p serial-nexus-mcp \
        -p serial-nexus-lan-gate \
 && mkdir -p /out \
 && cp target/release/serial-nexus-core \
        target/release/serial-nexus-terminal \
        target/release/serial-nexus-mcp \
        target/release/serial-nexus-lan-gate \
        /out/

FROM ${RUNTIME_IMAGE}

# Everything the gate executes is the workspace's own build output. The runtime
# image adds only the C library the binaries link against.
COPY --from=build /out /opt/snx/bin

ENV SNX_LAN_GATE_BIN_DIR=/opt/snx/bin \
    SNX_LAN_GATE_STATE_DIR=/state

ENTRYPOINT ["/opt/snx/bin/serial-nexus-lan-gate"]
