#!/bin/sh
# Development helper: (re)start `glidedesk-host serve` next to a private headless GNOME
# Shell, in the background, logging to $XDG_RUNTIME_DIR/glidedesk-serve.log.
#
#   GLIDEDESK_PASSWORD=... scripts/dev-serve.sh [serve options]
set -eu
cd "$(dirname "$0")/.."
log="${XDG_RUNTIME_DIR:-/tmp}/glidedesk-serve.log"
if pgrep -x glidedesk-host >/dev/null; then
  pkill -x glidedesk-host
  # dev-session.sh stops its shell when the host exits.
  i=0
  while pgrep -x glidedesk-host >/dev/null && [ $i -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
fi
: "${GLIDEDESK_PASSWORD:?set GLIDEDESK_PASSWORD}"
export GLIDEDESK_PASSWORD
RUST_LOG="${RUST_LOG:-info}" setsid scripts/dev-session.sh -- target/release/glidedesk-host serve "$@" \
  >"$log" 2>&1 </dev/null &
sleep 4
grep -a glidedesk_host "$log" | sed 's/\x1b\[[0-9;]*m//g' || true
