#!/usr/bin/env bash
# Start a built `dollet` and check what a browser and Plex actually receive.
#
#   scripts/smoke.sh localhost/dollet-relay:dev   # the image
#   scripts/smoke.sh --local                      # a release binary, no podman
#
# The suite proves the router answers; this proves the *build* does — no test
# in the tree asks the built binary for a URL. Run before any push:
# `scripts/release.sh` does.
#
# `--local` builds the SPA and a release binary and serves them straight off a
# loopback port, so an agent in a container with no podman can still gate its
# own work on this. Every check below holds in both modes except the last one,
# which is the image's own healthcheck. What `--local` therefore cannot see is
# the image around the binary: /data owned by the `dollet` uid, tini reaping
# orphans as PID 1, the HEALTHCHECK directive being wired at all, and ffmpeg
# and ca-certificates being present. Those are release gates and `release.sh`
# runs the image mode in front of every push.
set -euo pipefail

cd "$(dirname "$0")/.."

# shellcheck source=scripts/lib/serve.sh
source "$(dirname "$0")/lib/serve.sh"

usage() { sed -n '2,/^set -e/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }

MODE="image"
IMAGE=""
for arg in "$@"; do
  case "$arg" in
    --local) MODE="local" ;;
    -h|--help) usage; exit 0 ;;
    -*)
      echo "unknown flag: $arg" >&2
      usage >&2
      exit 2
      ;;
    *) IMAGE="$arg" ;;
  esac
done

if [[ "$MODE" == "image" && -z "$IMAGE" ]]; then
  usage >&2
  exit 2
fi
if [[ "$MODE" == "local" && -n "$IMAGE" ]]; then
  echo "refusing: --local builds what it serves; it cannot also test ${IMAGE}" >&2
  exit 2
fi

BINARY="target/release/dollet"

# Named after this process, so two of these can run at once and so cleanup can
# name exactly what it created. Never a prune of any kind: other containers in
# this environment are not this script's to reap.
NAME="dollet-smoke-$$"
VOLUME="dollet-smoke-$$"
BODY="$(mktemp -t dollet-smoke-body.XXXXXX)"
DATA_DIR=""
SERVER_PID=""

cleanup() {
  if [[ "$MODE" == "image" ]]; then
    podman rm -f "$NAME" >/dev/null 2>&1 || true
    podman volume rm -f "$VOLUME" >/dev/null 2>&1 || true
  else
    serve_stop "$SERVER_PID"
    if [[ -n "$DATA_DIR" ]]; then
      rm -rf "$DATA_DIR"
    fi
  fi
  rm -f "$BODY"
}
trap cleanup EXIT

PORT="$(free_port)"
BASE="http://127.0.0.1:${PORT}"

FAILURES=0

pass() { printf '  ok    %s\n' "$1"; }

fail() {
  printf '  FAIL  %s\n        expected: %s\n        actual:   %s\n' "$1" "$2" "$3" >&2
  FAILURES=$((FAILURES + 1))
}

# Sets HTTP_CODE and CONTENT_TYPE, leaves the body in $BODY. The charset is
# stripped: what is under test is the type the browser dispatches on.
fetch() {
  local meta
  meta="$(curl -s -o "$BODY" -w '%{http_code} %{content_type}' "${BASE}$1" || true)"
  HTTP_CODE="${meta%% *}"
  CONTENT_TYPE="${meta#* }"
  CONTENT_TYPE="${CONTENT_TYPE%%;*}"
  CONTENT_TYPE="${CONTENT_TYPE% }"
}

# expect <path> <status> <content-type> [substring...]
expect() {
  local path="$1" status="$2" ctype="$3"
  shift 3

  fetch "$path"
  if [[ "$HTTP_CODE $CONTENT_TYPE" != "$status $ctype" ]]; then
    fail "GET $path" "$status $ctype" "$HTTP_CODE $CONTENT_TYPE"
    return
  fi

  local needle
  for needle in "$@"; do
    if ! grep -qF -- "$needle" "$BODY"; then
      fail "GET $path" "a body containing \`$needle\`" "$(head -c 160 "$BODY" | tr '\n' ' ')"
      return
    fi
  done

  pass "GET $path -> $HTTP_CODE $CONTENT_TYPE"
}

if [[ "$MODE" == "image" ]]; then
  echo "== smoke: $IMAGE on $BASE =="

  podman run -d \
    --name "$NAME" \
    --volume "${VOLUME}:/data" \
    --publish "127.0.0.1:${PORT}:9191" \
    --env DOLLET_LOG=warn \
    "$IMAGE" >/dev/null

  ready=0
  for _ in $(seq 1 60); do
    if [[ "$(curl -s -o /dev/null -w '%{http_code}' "${BASE}/health")" == "200" ]]; then
      ready=1
      break
    fi
    sleep 0.5
  done

  if [[ "$ready" != "1" ]]; then
    echo "  FAIL  /health did not answer 200 within 30s" >&2
    podman logs "$NAME" 2>&1 | tail -30 >&2
    exit 1
  fi
else
  # Release, not debug: a debug build reads web/dist off disk per request, so
  # it would exercise the fallback's content types but prove nothing about what
  # ships. The SPA is built first because the release embed is baked at compile
  # time; cargo then rebuilds on its own, because rust-embed's `include_bytes!`
  # makes every file in web/dist a tracked input. That ordering is what keeps a
  # binary from serving a page older than the one on disk, so nothing below has
  # to check for it.
  [[ -d web/node_modules ]] || npm --prefix web ci
  npm --prefix web run build
  cargo build --release --bin dollet

  DATA_DIR="$(mktemp -d -t dollet-smoke-data.XXXXXX)"
  echo "== smoke: $BINARY on $BASE =="
  SERVER_PID="$(serve_start "$BINARY" "$DATA_DIR" "$PORT")"
  serve_wait "$BASE" "$DATA_DIR"
fi

# --- the SPA ----------------------------------------------------------------
#
# Derived from the client's own nav rather than listed here, so a page added to
# the sidebar is covered the day it lands. `/` and `/login` are not in it: one
# is the redirect every bookmark uses and the other is reachable only when
# signed out.
mapfile -t ROUTES < <(sed -n "s/.*to: '\([^']*\)'.*/\1/p" web/src/layout/nav.js)
if [[ "${#ROUTES[@]}" -lt 5 ]]; then
  echo "  FAIL  read ${#ROUTES[@]} routes out of web/src/layout/nav.js; the parse is wrong" >&2
  exit 1
fi

for route in / /login "${ROUTES[@]}"; do
  # A client route has no extension, so a fallback that guesses the type from
  # the *request* answers application/octet-stream and the browser saves the
  # page to disk instead of running it.
  expect "$route" 200 text/html "<div id=\"root\">"
done

# The bundle the binary embedded, named from the page it just served: hashed at
# build time, so nothing here can hard-code it.
fetch /
ASSET="$(grep -o '/assets/[A-Za-z0-9._-]*\.js' "$BODY" | head -1)"
if [[ -z "$ASSET" ]]; then
  fail "GET /" "an HTML page referencing /assets/<hash>.js" "$(head -c 160 "$BODY" | tr '\n' ' ')"
else
  expect "$ASSET" 200 text/javascript
fi

# --- what Plex reads --------------------------------------------------------
expect /hdhr/discover.json 200 application/json '"DeviceID":"12345678"' '"FriendlyName":"Dollet'
expect /hdhr/device.xml 200 application/xml '<DeviceID>12345678</DeviceID>'
expect /output/m3u 200 audio/x-mpegurl '#EXTM3U'
expect /output/epg 200 application/xml '<tv'

# An empty instance has no lineup, and `[]` is the only answer Plex accepts for
# one — `{}` or a 404 reads as a tuner that is broken rather than empty.
fetch /hdhr/lineup.json
if [[ "$HTTP_CODE $CONTENT_TYPE" == "200 application/json" && "$(cat "$BODY")" == "[]" ]]; then
  pass "GET /hdhr/lineup.json -> 200 application/json []"
else
  fail "GET /hdhr/lineup.json" "200 application/json []" \
    "$HTTP_CODE $CONTENT_TYPE $(head -c 160 "$BODY" | tr '\n' ' ')"
fi

# --- the API ----------------------------------------------------------------
expect /api/accounts/initialize-superuser/ 200 application/json '"superuser_exists":false'

# 401 rather than the page: a settings request answered with HTML would reach
# the client as `response.ok` and be rendered as data.
fetch /api/core/settings/
if [[ "$HTTP_CODE" == "401" ]]; then
  pass "GET /api/core/settings/ -> 401"
else
  fail "GET /api/core/settings/" "401" "$HTTP_CODE $CONTENT_TYPE"
fi

# A mistyped API path must 404 as an API, not fall through to index.html.
expect /api/nope/ 404 application/json '"detail"'

# Bad Xtream credentials answer with a 404 page rather than a 401,
# because a 401 makes a player prompt for a password it was never given.
fetch '/player_api.php?username=x&password=y'
if [[ "$HTTP_CODE" == "404" ]]; then
  pass "GET /player_api.php (bad credentials) -> 404"
else
  fail "GET /player_api.php?username=x&password=y" "404" "$HTTP_CODE $CONTENT_TYPE"
fi

# --- the healthcheck --------------------------------------------------------
#
# `dollet health` dials whatever DOLLET_LISTEN names, which is how the same
# subcommand serves the image's HEALTHCHECK and this.
if [[ "$MODE" == "image" ]]; then
  if podman exec "$NAME" /usr/local/bin/dollet health >/dev/null 2>&1; then
    pass "podman exec dollet health -> 0"
  else
    fail "podman exec $NAME /usr/local/bin/dollet health" "exit 0" "exit $?"
  fi
else
  if DOLLET_LISTEN="127.0.0.1:${PORT}" "$BINARY" health >/dev/null 2>&1; then
    pass "dollet health -> 0"
  else
    fail "$BINARY health" "exit 0" "exit $?"
  fi
fi

TARGET="$IMAGE"
if [[ "$MODE" == "local" ]]; then
  TARGET="$BINARY"
fi

echo
if [[ "$FAILURES" -gt 0 ]]; then
  echo "smoke FAILED: $FAILURES check(s) on $TARGET" >&2
  exit 1
fi
echo "smoke passed: $TARGET"
