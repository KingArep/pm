#!/bin/sh
# PencariMovie Server - Linux tray icon.
#
# EMBEDDED IN THE BINARY. `pencarimovie-server --tray` writes this script to
# `storage/tray/pm-tray.sh` and runs it, so a folder holding only the executable
# still gets a tray icon: there is no loose script to ship, lose or keep in sync.
#
# Only the ICON RENDERER is external: `yad --notification` (GTK). There is no way
# to draw a GTK/AppIndicator icon without a GUI toolkit, so on a desktop without
# yad the server still runs and prints the dashboard URL.
#   Debian/Ubuntu: sudo apt install yad
#   Fedora:        sudo dnf install yad
#   Arch:          sudo pacman -S yad
#
# Inputs (set by the binary, all optional so the script also works by hand):
#   PORT        listen port                       (default 8088)
#   PM_TRAY_EXE the server executable             (default ./pencarimovie-server)
#   PM_TRAY_ICON logo used by the icon            (default <exe dir>/pencarimovie.png)
#   PM_TRAY_LOG the log file opened by "Open log" (default ./storage/server.log)
#   PM_TRAY_PID pid "Stop server" signals         (default: none, item hidden)
set -u

PORT="${PORT:-8088}"
EXE="${PM_TRAY_EXE:-./pencarimovie-server}"
EXE_DIR=$(dirname "$EXE")
ICON="${PM_TRAY_ICON:-$EXE_DIR/pencarimovie.png}"
LOG="${PM_TRAY_LOG:-./storage/server.log}"
PID="${PM_TRAY_PID:-}"
URL="http://127.0.0.1:$PORT/"
ADDON="http://127.0.0.1:$PORT/manifest.json"

# The LAN address a TV box can reach: same RFC1918-only rule as the dashboard.
lan_url() {
    ip=""
    if command -v ip >/dev/null 2>&1; then
        ip=$(ip route get 1.1.1.1 2>/dev/null | awk '{for (i = 1; i < NF; i++) if ($i == "src") { print $(i + 1); exit }}')
    fi
    case "$ip" in
        10.*|192.168.*|172.1[6-9].*|172.2[0-9].*|172.3[0-1].*) ;;
        *) ip="127.0.0.1" ;;
    esac
    printf 'http://%s:%s/manifest.json' "$ip" "$PORT"
}

# wl-copy (Wayland), xclip or xsel (X11): whichever session has.
set_clip() {
    if command -v wl-copy >/dev/null 2>&1; then
        printf %s "$1" | wl-copy
    elif command -v xclip >/dev/null 2>&1; then
        printf %s "$1" | xclip -selection clipboard
    elif command -v xsel >/dev/null 2>&1; then
        printf %s "$1" | xsel --clipboard --input
    else
        return 1
    fi
}

open_url() {
    if command -v xdg-open >/dev/null 2>&1; then
        xdg-open "$1" >/dev/null 2>&1 &
    fi
}

menu="Open dashboard!xdg-open $URL"
menu="$menu|Open add-on page!xdg-open $URL#addon"
menu="$menu|Copy add-on URL!sh -c 'printf %s $(lan_url) | (wl-copy 2>/dev/null || xclip -selection clipboard 2>/dev/null || xsel --clipboard --input 2>/dev/null) || true'"
menu="$menu|Open log!xdg-open $LOG"
if [ -n "$PID" ]; then
    menu="$menu|Stop server!kill $PID"
fi
menu="$menu|Hide tray icon (server keeps running)!quit"

[ -f "$ICON" ] || ICON=utilities-terminal

if command -v yad >/dev/null 2>&1; then
    exec yad --notification \
        --image="$ICON" \
        --text="PencariMovie Server - port $PORT" \
        --command="xdg-open $URL" \
        --menu="$menu"
fi

# No yad: the icon is impossible, the server is not. Say where it is.
echo "PencariMovie Server is running on $URL (install yad for a tray icon)"
if command -v notify-send >/dev/null 2>&1; then
    notify-send "PencariMovie Server" "Running on $URL (install yad for a tray icon)"
fi
