#!/bin/sh
# PencariMovie Server - tray launcher (Linux).
#
# The tray is INSIDE the binary: `pencarimovie-server --tray` starts the server
# and the icon in one process (it writes its own embedded tray script to
# storage/tray/pm-tray.sh and runs it). Nothing beside the executable is needed.
#
# The icon itself is drawn by `yad --notification`; without yad the server still
# runs and the dashboard URL is printed.
#   Debian/Ubuntu: sudo apt install yad
#   Fedora:        sudo dnf install yad
#   Arch:          sudo pacman -S yad
set -eu
cd "$(dirname "$0")"
PORT="${PORT:-8088}"
[ -x ./pencarimovie-server ] || { echo "pencarimovie-server not found next to this script" >&2; exit 1; }
mkdir -p ./storage
ulimit -n 65536 2>/dev/null || true
exec ./pencarimovie-server --tray --port "$PORT" --no-open
