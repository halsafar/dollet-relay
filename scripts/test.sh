#!/usr/bin/env bash
# Run the full suite in the builder image, so the host needs no toolchain.
#
#   scripts/test.sh                   # canonical: in the builder image
#   scripts/test.sh --local           # the host's cargo; much faster to iterate
#   scripts/test.sh --local --coverage
#
# --coverage runs cargo llvm-cov instead of cargo test and needs --local; the
# builder image has no llvm-cov.
set -euo pipefail

cd "$(dirname "$0")/.."

usage() { sed -n '2,/^set -e/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }

LOCAL=0
COVERAGE=0
for arg in "$@"; do
  case "$arg" in
    --local) LOCAL=1 ;;
    --coverage) COVERAGE=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown flag: $arg" >&2; usage >&2; exit 2 ;;
  esac
done
if [[ "$COVERAGE" == 1 && "$LOCAL" != 1 ]]; then
  echo "--coverage needs --local" >&2
  exit 2
fi

# One definition, run on the host or in the container: a gate written twice
# drifts, and the copy that drifts is the one CI runs.
RUST_GATE='cargo fmt --all --check && cargo clippy --all-targets --all-features -- -D warnings'
if [[ "$COVERAGE" == 1 ]]; then
  RUST_GATE+=' && cargo llvm-cov --workspace --all-features --summary-only'
else
  RUST_GATE+=' && cargo test --workspace --all-features'
fi

# `routes:check` regenerates web/route-manifest.json by running the client and
# fails if the committed copy has drifted. The Rust side asserts the server
# serves every path in that file, so a skipped check here lets the Rust half
# validate a stale manifest and report green.
run_web_suite() {
  if ! command -v npm >/dev/null 2>&1; then
    echo "npm not found: the route manifest and web tests were not checked" >&2
    return 1
  fi
  [[ -d web/node_modules ]] || npm --prefix web ci
  npm --prefix web run lint
  npm --prefix web run routes:check
  npm --prefix web run test
}

if [[ "$LOCAL" == 1 ]]; then
  bash -c "$RUST_GATE"
  run_web_suite
  exit 0
fi

IMAGE="localhost/dollet-relay-ci:latest"

podman build --file docker/Dockerfile --target builder --tag "$IMAGE" .

podman run --rm \
  --volume "$PWD:/src:z" \
  --workdir /src \
  "$IMAGE" \
  bash -lc "$RUST_GATE"

# On the host: the builder stage has cargo, not node.
run_web_suite
