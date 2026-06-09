#!/usr/bin/env bash
# Run `cargo check` (or another cargo subcommand) inside the dev-build
# container. Persistent named volumes cache the registry and target dir.
set -euo pipefail

cd "$(dirname "$0")/../.."

IMAGE="mprocs-dev-build"
docker image inspect "$IMAGE" >/dev/null 2>&1 || \
  docker build -t "$IMAGE" -f scripts/dev-build/Dockerfile scripts/dev-build

SUBCMD="${1:-check}"
shift || true

exec docker run --rm \
  -v "$PWD":/work \
  -v mprocs-cargo-home:/cargo \
  -v mprocs-cargo-target:/target \
  "$IMAGE" \
  cargo "$SUBCMD" "$@"
