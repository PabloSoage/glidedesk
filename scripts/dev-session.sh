#!/bin/sh
# Development helper: run a command next to a private headless GNOME Shell.
#
#   scripts/dev-session.sh [--app "cmd"] -- glidedesk-host capture-test
#
# Starts its own D-Bus session bus (dbus-run-session), so it does not touch any
# desktop session the user already has. GNOME Shell is stopped by PID on exit.
set -eu
if [ -z "${GLIDEDESK_IN_BUS:-}" ]; then
  GLIDEDESK_IN_BUS=1 exec dbus-run-session -- "$0" "$@"
fi
app=""
if [ "${1:-}" = "--app" ]; then app="$2"; shift 2; fi
[ "${1:-}" = "--" ] && shift

log="${XDG_RUNTIME_DIR:-/tmp}/glidedesk-shell.log"
gnome-shell --headless --no-x11 >"$log" 2>&1 &
shell=$!
trap 'kill -9 $shell 2>/dev/null || true' EXIT INT TERM

# Wait until Mutter's remote desktop interface is on the bus.
i=0
until gdbus introspect --session --dest org.gnome.Mutter.RemoteDesktop \
      --object-path /org/gnome/Mutter/RemoteDesktop >/dev/null 2>&1; do
  i=$((i + 1)); [ $i -gt 100 ] && { echo "gnome-shell did not start, see $log" >&2; exit 1; }
  sleep 0.1
done
export WAYLAND_DISPLAY
WAYLAND_DISPLAY=$(sed -n 's/.*Using Wayland display name .\(wayland-[0-9]*\).*/\1/p' "$log" | head -1)
if [ -n "$app" ]; then (sleep 1.5; sh -c "$app") >/dev/null 2>&1 & fi
"$@"
