#!/usr/bin/env bash
# Build the runtime image locally. Nothing is installed on the host.
set -euo pipefail

cd "$(dirname "$0")/.."

IMAGE="${IMAGE:-localhost/dollet-relay}"
TAG="${TAG:-dev}"

podman build \
  --file docker/Dockerfile \
  --tag "${IMAGE}:${TAG}" \
  .

echo
echo "built ${IMAGE}:${TAG}"
podman images --format '  {{.Repository}}:{{.Tag}}  {{.Size}}' "${IMAGE}"
echo
echo "check it serves what a browser and Plex expect:"
echo "  scripts/smoke.sh ${IMAGE}:${TAG}"
