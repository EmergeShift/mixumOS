#!/bin/bash
# Build and push the gateway image used by cloud/k8s/60-gateway.yaml.
# Usage: TAG=... bash cloud/gateway/build.sh [--push]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TAG="${TAG:-ghcr.io/mixumos/mixumos-gateway:0.1.3}"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

(cd "$ROOT/rust" && cargo build --release -p mixum-gateway)
cp "$ROOT/rust/target/release/mixum-gateway" "$STAGE/mixum-gateway"
docker build -f "$ROOT/cloud/gateway/Dockerfile" -t "$TAG" "$STAGE"

if [ "${1:-}" = "--push" ]; then
    docker push "$TAG"
fi

echo "gateway image: $TAG"
