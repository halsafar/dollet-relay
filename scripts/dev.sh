#!/usr/bin/env bash
# Run the server against a local data dir, or Vite's dev server proxying to it.
#
#   scripts/dev.sh        # the server, on 127.0.0.1:9191
#   scripts/dev.sh web    # the frontend, with hot reload
set -euo pipefail

cd "$(dirname "$0")/.."

usage() { sed -n '2,/^set -e/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }

export DOLLET_DATA_DIR="${DOLLET_DATA_DIR:-$PWD/.dev-data}"
export DOLLET_LISTEN="${DOLLET_LISTEN:-127.0.0.1:9191}"
export DOLLET_LOG="${DOLLET_LOG:-dollet=debug,dollet_core=debug,dollet_stream=debug,info}"

mkdir -p "$DOLLET_DATA_DIR"

case "${1:-}" in
  "") exec cargo run --bin dollet -- serve ;;
  web) exec npm --prefix web run dev ;;
  -h|--help) usage; exit 0 ;;
  *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
esac
