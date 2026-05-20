#!/usr/bin/env bash
# Cargo runner for x86_64-pc-windows-gnu: executes Windows binaries under
# wine, wrapped in xvfb so the tray icon and event loop have a display.
#
# Wired up via .cargo/config.toml. Used both for `cargo test
# --target x86_64-pc-windows-gnu` and for ad-hoc smoke-tests of the
# cross-built binary.
#
# The xvfb screen size is generous so the wine taskbar / notification
# area have room to lay out; smaller sizes have caused tray-icon
# placement to fail in past CI runs.

set -euo pipefail
exec xvfb-run --auto-servernum \
  --server-args='-screen 0 1280x720x24' \
  wine "$@"
