#!/usr/bin/env bash
# Record the demo video and screenshots from the real app.
#
# The model's turns come from scenarios.txt (the offline demo model); the
# agent loop, tools, approvals, sub-agents, git worktrees and UI are real.
#
# Needs: a built `swe` binary with the UI embedded, node with playwright,
# ffmpeg, git and python3.
#
#   scripts/demo/record.sh [output-dir]
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
out="$(mkdir -p "${1:-$repo/site/assets}" && cd "${1:-$repo/site/assets}" && pwd)"
root="${DEMO_ROOT:-$(mktemp -d)}"
port="${DEMO_PORT:-4599}"
swe="${SWE_BIN:-$repo/target/release/swe}"
[ -x "$swe" ] || swe="$repo/target/debug/swe"
[ -x "$swe" ] || { echo "build the binary first: cargo build --release" >&2; exit 1; }

server_pid=""
stop_server() {
  if [ -n "$server_pid" ]; then kill "$server_pid" 2>/dev/null || true; wait "$server_pid" 2>/dev/null || true; fi
  server_pid=""
}
trap stop_server EXIT

# A fresh workspace, home directory and server for every pass.
start_server() {
  stop_server
  rm -rf "$root/.swe-agent"
  "$here/setup-workspace.sh" "$root/notes-api"
  ( cd "$root/notes-api" && SWE_AGENT_DEMO=1 SWE_AGENT_DEMO_SCRIPT="$here/scenarios.txt" \
      SWE_AGENT_HOME="$root/.swe-agent" exec "$swe" serve --no-open --port "$port" ) >"$root/server.log" 2>&1 &
  server_pid=$!
  for _ in $(seq 1 50); do
    curl -sf "http://127.0.0.1:$port/api/health" >/dev/null && return 0
    sleep 0.2
  done
  echo "the server did not start; see $root/server.log" >&2
  exit 1
}

work="$root/recording"
rm -rf "$work"; mkdir -p "$work"

start_server
DEMO_URL="http://127.0.0.1:$port/" DEMO_OUT="$work" DEMO_MODE=video node "$here/record.cjs"
raw="$(ls "$work"/*.webm | head -1)"

start_server
DEMO_URL="http://127.0.0.1:$port/" DEMO_OUT="$work" DEMO_MODE=shots node "$here/record.cjs"
stop_server

# H.264 plays everywhere; VP9 is the smaller alternative for browsers that take it.
ffmpeg -y -loglevel error -i "$raw" -an -c:v libx264 -preset slow -crf 24 -pix_fmt yuv420p -movflags +faststart "$out/demo.mp4"
ffmpeg -y -loglevel error -i "$raw" -an -c:v libvpx-vp9 -crf 36 -b:v 0 -row-mt 1 "$out/demo.webm"
# Only the screenshots the project page and the README use.
for shot in approval parallel-agents board orchestrated; do
  cp "$work/$shot.png" "$out/"
done
cp "$out/parallel-agents.png" "$out/poster.png"

echo "wrote:"; ls -la "$out"
