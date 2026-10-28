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
# Without a user PipeWire (no systemd user session: containers, WSL) start a private one.
helpers=""
if ! pw-cli info 0 >/dev/null 2>&1; then
  pipewire >/dev/null 2>&1 & helpers="$helpers $!"
  sleep 0.3
  wireplumber >/dev/null 2>&1 & helpers="$helpers $!"
fi
# Same flavour of GNOME as a local login: on Ubuntu its session mode (dock) and its settings
# overrides, which among other things give windows minimize and maximize buttons (plain
# GNOME only shows close).
mode=""
if [ -z "${XDG_CURRENT_DESKTOP:-}" ]; then
  if [ -f /usr/share/wayland-sessions/ubuntu.desktop ]; then
    export XDG_CURRENT_DESKTOP=ubuntu:GNOME XDG_SESSION_DESKTOP=ubuntu
    mode="--mode=ubuntu"
  elif ls /usr/share/glib-2.0/schemas/*ubuntu-settings.gschema.override >/dev/null 2>&1; then
    export XDG_CURRENT_DESKTOP=ubuntu:GNOME   # settings only (no Ubuntu shell theme)
  fi
fi
export XDG_CURRENT_DESKTOP="${XDG_CURRENT_DESKTOP:-GNOME}" XDG_SESSION_TYPE=wayland
# Full redraws: with clipped redraws the software cursor painted over the previous frame stays
# in the parts of the framebuffer that are not repainted, and the zero-copy capture (a blit of
# that framebuffer) would carry it although the client draws its own pointer.
# GLIDEDESK_SHELL_ARGS: extra gnome-shell flags for tests (e.g. --unsafe-mode for Shell.Eval).
# shellcheck disable=SC2086
CLUTTER_PAINT="${CLUTTER_PAINT:-disable-clipped-redraws}" gnome-shell --headless --no-x11 $mode ${GLIDEDESK_SHELL_ARGS:-} >"$log" 2>&1 &
shell=$!
trap 'kill -9 $shell $helpers 2>/dev/null || true' EXIT INT TERM

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
