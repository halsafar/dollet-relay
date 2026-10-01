#!/usr/bin/env bash
# Build, verify, tag and push a release, in one command from a clean tree:
#
#   scripts/release.sh <version>      # plain x.y.z
#
# The version is user-visible at runtime — `/health`, the startup log line, the
# user-agent providers see — so it has to be the one the binary was built with.
# It lives in four files that nothing else keeps in agreement, so this script
# writes all four and makes the release commit itself.
#
# The order is the point. The version is written into the working tree, the
# image is built from exactly those bytes and smoke-tested, and only then does
# anything enter history — a commit and a tag for an image that was refused
# would name a release that never shipped. A failure before that point leaves
# the bump sitting uncommitted, which is also where a re-run picks it up: every
# step below is a no-op once it has happened, so a release that died at the
# push is resumed by running the same command again.
#
# Pushing the tag is what publishes. CI runs the suite against the tagged commit
# and pushes the image to the registry configured on the forge. The image built
# here exists to be smoke-tested, which CI cannot do.
#
# Set REGISTRY to publish from this machine as well, for when CI cannot:
#
#   REGISTRY=ghcr.io/halsafar scripts/release.sh <version>
#
# The images go up before the tag does. CI still runs on the tag and pushes the
# same three tags again, so they end as CI's build of the same commit, under a
# different digest.
#
# Set PUSH=0 to build, verify, commit and tag without pushing anything.
set -euo pipefail

cd "$(dirname "$0")/.."

usage() { sed -n '2,/^set -e/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }

VERSION="${1:-}"
case "$VERSION" in
  "") usage >&2; exit 2 ;;
  -h|--help) usage; exit 0 ;;
esac

# Plain x.y.z only: the git tag, the three image tags and the version the binary
# announces are all this one string.
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "refusing: ${VERSION} is not an x.y.z version" >&2
  exit 1
fi

# cargo and npm are here to write their own lock files. Checked up front so a
# missing one is a sentence rather than a half-bumped tree.
for tool in git podman cargo npm; do
  command -v "$tool" >/dev/null \
    || { echo "refusing: ${tool} is not installed" >&2; exit 1; }
done

if [[ -n "$(git status --porcelain)" ]]; then
  echo "refusing: working tree is dirty. Bumping the version is this script's" >&2
  echo "job — commit or revert everything else, then run it on a clean tree." >&2
  exit 1
fi

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [[ "$BRANCH" == "HEAD" ]]; then
  echo "refusing: detached HEAD; the release commit would sit on no branch" >&2
  exit 1
fi

TAG="v${VERSION}"
if git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null \
   && [[ "$(git rev-parse "${TAG}^{commit}")" != "$(git rev-parse HEAD)" ]]; then
  echo "refusing: ${TAG} already exists on $(git rev-parse --short "${TAG}^{commit}"), not on HEAD" >&2
  exit 1
fi

REGISTRY="${REGISTRY:-}"
IMAGE="${REGISTRY:-localhost}/dollet-relay"
PUSH="${PUSH:-1}"

sed -i -E "0,/^version = \"[^\"]+\"/s//version = \"${VERSION}\"/" Cargo.toml
cargo update --workspace --quiet
(cd web && npm version "$VERSION" --no-git-tag-version --allow-same-version >/dev/null)

# A sed that matched nothing leaves the binary announcing the old version.
CARGO_VERSION="$(sed -nE '0,/^version = "([^"]+)"/s//\1/p' Cargo.toml)"
WEB_VERSION="$(sed -nE 's/^[[:space:]]*"version": "([^"]+)",?$/\1/p' web/package.json | head -1)"
if [[ "$CARGO_VERSION" != "$VERSION" || "$WEB_VERSION" != "$VERSION" ]]; then
  echo "bug: bumped to Cargo.toml ${CARGO_VERSION}, web ${WEB_VERSION}, wanted ${VERSION}" >&2
  exit 1
fi

podman build \
  --file docker/Dockerfile \
  --tag "${IMAGE}:${VERSION}" \
  --tag "${IMAGE}:latest" \
  .

echo "built ${IMAGE}:${VERSION}"

# Before anything is committed, and fatal: nothing in the suite asks the built
# image for a URL.
scripts/smoke.sh "${IMAGE}:${VERSION}"

# Named paths, never `git add -A`: this tree is often shared, and a sweep has
# put someone else's half-finished work into a commit here before.
git add Cargo.toml Cargo.lock web/package.json web/package-lock.json
git diff --cached --quiet || git commit -qm "$VERSION"

SHA="$(git rev-parse --short HEAD)"
podman tag "${IMAGE}:${VERSION}" "${IMAGE}:sha-${SHA}"

git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null \
  || git tag -a "${TAG}" -m "dollet-relay ${VERSION}"

if [[ "$PUSH" == "1" ]]; then
  if [[ -n "$REGISTRY" ]]; then
    for tag in "$VERSION" "sha-${SHA}" latest; do
      podman push "${IMAGE}:${tag}"
    done
    echo "pushed ${IMAGE}:${VERSION}"
  fi
  # The branch as well as the tag: pushing the tag alone sends the commit's
  # objects but leaves the release commit reachable from no branch upstream.
  git push origin "HEAD:${BRANCH}"
  git push origin "${TAG}"
  echo "pushed ${BRANCH} and ${TAG}; CI publishes the image once the suite passes"
else
  echo "PUSH=0, not pushing; ${TAG} is tagged locally on ${SHA}"
fi
