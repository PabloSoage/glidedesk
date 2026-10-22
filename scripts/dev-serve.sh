#!/bin/sh
# Development helper: (re)start `glidedesk-host serve` next to a private headless GNOME
# Shell, in the background, logging to $XDG_RUNTIME_DIR/glidedesk-serve.log.
#
#   GLIDEDESK_PASSWORD=... scripts/dev-serve.sh [serve options]
#   scripts/dev-serve.sh --stop
#
# GLIDEDESK_APP: a command to start inside the session (e.g. "python3 tools/flash.py").
#
# Everything runs in its own process session (setsid), whose id is kept in
# $XDG_RUNTIME_DIR/glidedesk-serve.sid: a restart or --stop ends the whole session (shell,
# session bus, portals, apps started in it), not only the host.
set -eu
cd "$(dirname "$0")/.."
run="${XDG_RUNTIME_DIR:-/tmp}"
log="$run/glidedesk-serve.log"
sidfile="$run/glidedesk-serve.sid"
if [ -f "$sidfile" ]; then
  sid=$(cat "$sidfile")
  pkill -TERM -s "$sid" 2>/dev/null || true
  i=0
  while pgrep -s "$sid" >/dev/null && [ $i -lt 30 ]; do sleep 0.1; i=$((i + 1)); done
  pkill -KILL -s "$sid" 2>/dev/null || true
  rm -f "$sidfile"
fi
[ "${1:-}" = "--stop" ] && exit 0
: "${GLIDEDESK_PASSWORD:?set GLIDEDESK_PASSWORD}"
export GLIDEDESK_PASSWORD
if [ -n "${GLIDEDESK_APP:-}" ]; then
  set -- --app "$GLIDEDESK_APP" -- target/release/glidedesk-host serve "$@"
else
  set -- -- target/release/glidedesk-host serve "$@"
fi
RUST_LOG="${RUST_LOG:-info}" setsid scripts/dev-session.sh "$@" >"$log" 2>&1 </dev/null &
echo $! >"$sidfile"
sleep 4
grep -a glidedesk_host "$log" | sed 's/\x1b\[[0-9;]*m//g' || true
