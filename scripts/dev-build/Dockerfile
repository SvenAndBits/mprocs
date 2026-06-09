# Build environment for mprocs development.
# Not shipped — used by scripts/dev-build/check.sh to run cargo against a
# host-mounted source tree without requiring rustup on the host.
FROM rust:1-bookworm

RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /work
ENV CARGO_HOME=/cargo
ENV CARGO_TARGET_DIR=/target
