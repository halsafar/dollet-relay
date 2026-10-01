#!/usr/bin/env bash
# Scaffolding for the harnesses that drive a real `dollet` over a real socket:
# scripts/e2e.sh and scripts/smoke.sh --local.
#
# Sourced, never executed; callers are expected to be `set -euo pipefail`.

# A refused connection is the evidence nothing holds the port. Another process
# can still take it between here and the bind; the range is wide enough that
# retrying is cheaper than coordinating.
port_is_free() { ! (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

free_port() {
  local candidate
  for _ in $(seq 1 100); do
    candidate=$(((RANDOM % 20000) + 40000))
    if port_is_free "$candidate"; then
      echo "$candidate"
      return 0
    fi
  done
  echo "no free port in 40000-59999" >&2
  return 1
}

# serve_start <binary> <data-dir> <port> -> pid on stdout
#
# The log goes beside the data rather than to the terminal, so a server that
# never came up can be quoted by serve_wait instead of interleaving with the
# harness's own output.
serve_start() {
  local binary="$1" dir="$2" port="$3"
  DOLLET_DATA_DIR="$dir" DOLLET_LISTEN="127.0.0.1:${port}" DOLLET_LOG=warn \
    "$binary" serve >"${dir}/server.log" 2>&1 &
  echo $!
}

# serve_wait <base-url> <data-dir>
serve_wait() {
  local base="$1" dir="$2"
  for _ in $(seq 1 60); do
    if [[ "$(curl -s -o /dev/null -w '%{http_code}' "${base}/health")" == "200" ]]; then
      return 0
    fi
    sleep 0.5
  done
  echo "${base} never answered /health:" >&2
  tail -20 "${dir}/server.log" >&2
  return 1
}

# serve_stop <pid>...
#
# Every command tolerates its own failure, because this runs from a trap body
# under `set -e`; a failing `wait` would abort the trap and leave the data dirs
# behind. Empty arguments are skipped so a caller can pass pid variables that
# were never assigned.
serve_stop() {
  local pid
  for pid in "$@"; do
    [[ -n "$pid" ]] || continue
    kill "$pid" 2>/dev/null || true
  done
  for pid in "$@"; do
    [[ -n "$pid" ]] || continue
    # SIGTERM drains in-flight responses and stops the scheduler, which a
    # server holding a refresh against an unreachable provider can take tens of
    # seconds to finish. A test harness has nothing to drain, so it insists.
    for _ in $(seq 1 12); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.25
    done
    kill -9 "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
}
