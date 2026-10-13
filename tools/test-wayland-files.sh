#!/usr/bin/env bash
# Exercise the native file clipboard in a private headless Wayland session.
# Requires sway on PATH. Optional arguments are forwarded to cargo test.
set -euo pipefail

cw_test_root=$(mktemp -d /tmp/cw-wayland.XXXXXX)
cw_sway_pid=""
cleanup() {
  if [[ -n "$cw_sway_pid" ]]; then
    kill "$cw_sway_pid" 2>/dev/null || true
    wait "$cw_sway_pid" 2>/dev/null || true
  fi
  rm -rf -- "$cw_test_root"
}
trap cleanup EXIT

mkdir -m 700 "$cw_test_root/runtime"
export XDG_RUNTIME_DIR="$cw_test_root/runtime"
export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1
unset DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET SWAYSOCK DBUS_SESSION_BUS_ADDRESS
unset HYPRLAND_INSTANCE_SIGNATURE

cat > "$cw_test_root/sway.conf" <<'EOF'
xwayland disable
output HEADLESS-1 resolution 1280x720
seat * fallback true
EOF
sway --config "$cw_test_root/sway.conf" --debug > "$cw_test_root/sway.log" 2>&1 &
cw_sway_pid=$!

for ((cw_attempt = 0; cw_attempt < 100; cw_attempt++)); do
  if ! kill -0 "$cw_sway_pid" 2>/dev/null; then
    cat "$cw_test_root/sway.log"
    exit 1
  fi
  for cw_socket in "$XDG_RUNTIME_DIR"/wayland-*; do
    if [[ -S "$cw_socket" ]]; then
      export WAYLAND_DISPLAY="${cw_socket##*/}"
      break 2
    fi
  done
  sleep 0.1
done

if [[ -z "${WAYLAND_DISPLAY:-}" ]]; then
  cat "$cw_test_root/sway.log"
  echo "Timed out waiting for the test Wayland compositor" >&2
  exit 1
fi

cargo test -p clipboardwire-core --test wayland_file_clipboard "$@" -- --ignored --nocapture
