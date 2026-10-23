#!/bin/sh
# Remove the LaunchAgent installed by autostart-on.sh and stop the server it
# started.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
label="com.pencarimovie.server"
plist="$HOME/Library/LaunchAgents/$label.plist"
launchctl unload "$plist" >/dev/null 2>&1 || true
rm -f "$plist"
pkill -f "pencarimovie-server --port" 2>/dev/null || true
rmdir "$here/storage" 2>/dev/null || true
echo "Removed $plist (if it existed)."
