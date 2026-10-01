#!/usr/bin/env bash
# Browser end-to-end: a real Chromium against two real `dollet` processes.
#
#   scripts/e2e.sh                     # every journey
#   scripts/e2e.sh e2e/routes.spec.js  # arguments go through to `playwright test`
#
# Deliberately not part of scripts/test.sh: it builds the SPA, builds the
# binary and drives a browser, and the builder image has neither a browser nor
# a reason to grow one.
#
# Two servers, because "empty instance" and "instance with data" are different
# products on the first screen: one offers to create an administrator, the
# other asks for a sign-in.
#
# The one thing here that writes outside the repo is Chromium: Playwright
# downloads it into ~/.cache/ms-playwright (~170 MB) the first time.
set -euo pipefail

cd "$(dirname "$0")/.."

# shellcheck source=scripts/lib/serve.sh
source "$(dirname "$0")/lib/serve.sh"

SEED="fixtures/synthetic/instance.sql"
BINARY="target/debug/dollet"

[[ -d web/node_modules ]] || npm --prefix web ci

# A debug build of rust-embed reads web/dist off disk per request — only the
# release build bakes it in — so the order here is not about embedding. It is
# that a missing web/dist answers "frontend not built" with a 404, and a 404 on
# every route reads like a routing bug rather than a missing step.
npm --prefix web run build
cargo build --bin dollet

EMPTY_DIR="$(mktemp -d -t dollet-e2e-empty.XXXXXX)"
SEEDED_DIR="$(mktemp -d -t dollet-e2e-seeded.XXXXXX)"
EMPTY_PID=""
SEEDED_PID=""

# By pid and by path, never by name: this environment has other people's
# `dollet` processes and temp dirs in it.
cleanup() {
  serve_stop "$EMPTY_PID" "$SEEDED_PID"
  rm -rf "$EMPTY_DIR" "$SEEDED_DIR"
}
trap cleanup EXIT

EMPTY_PORT="$(free_port)"
SEEDED_PORT="$(free_port)"
export E2E_EMPTY_URL="http://127.0.0.1:${EMPTY_PORT}"
export E2E_SEEDED_URL="http://127.0.0.1:${SEEDED_PORT}"

# Seeded before it is served: `dollet seed` refuses an instance that already
# has users, and it is the only way these tests get a known lineup.
DOLLET_DATA_DIR="$SEEDED_DIR" DOLLET_LOG=warn "$BINARY" seed "$SEED"

EMPTY_PID="$(serve_start "$BINARY" "$EMPTY_DIR" "$EMPTY_PORT")"
SEEDED_PID="$(serve_start "$BINARY" "$SEEDED_DIR" "$SEEDED_PORT")"
serve_wait "$E2E_EMPTY_URL" "$EMPTY_DIR"
serve_wait "$E2E_SEEDED_URL" "$SEEDED_DIR"

echo "empty:  $E2E_EMPTY_URL  ($EMPTY_DIR)"
echo "seeded: $E2E_SEEDED_URL ($SEEDED_DIR)"

# Downloads only what is missing, and prints one line when it is already there.
(cd web && npx playwright install chromium)

(cd web && npx playwright test "$@")
