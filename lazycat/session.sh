#!/bin/bash
set -euo pipefail

export DISPLAY=:1
export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1
export QT_ACCESSIBILITY=1
export NO_AT_BRIDGE=0
export RUST_LOG=mimicwx=info

Xtigervnc :1 -geometry 1280x800 -depth 24 \
    -SecurityTypes None -localhost yes -AlwaysShared \
    -nolisten tcp -rfbport 5901 > /tmp/mimicwx-vnc.log 2>&1 &
desktop_pid=$!
trap 'kill "$desktop_pid" 2>/dev/null || true' EXIT

for attempt in $(seq 1 30); do
    if xdpyinfo >/dev/null 2>&1; then
        break
    fi
    sleep 1
done
xdpyinfo >/dev/null
xset s off
xset -dpms || true
xset s noblank
openbox > /tmp/mimicwx-openbox.log 2>&1 &

export AT_SPI_BUS_ADDRESS
AT_SPI_BUS_ADDRESS="$(dbus-send --session --print-reply=literal \
    --dest=org.a11y.Bus /org/a11y/bus org.a11y.Bus.GetAddress)"
if [ -z "$AT_SPI_BUS_ADDRESS" ]; then
    printf 'The accessibility bus did not start.\n' >&2
    exit 1
fi

websockify --web=/usr/share/novnc --heartbeat=30 \
    0.0.0.0:6080 127.0.0.1:5901 > /tmp/mimicwx-novnc.log 2>&1 &
wechat --no-sandbox --disable-gpu > /tmp/mimicwx-wechat.log 2>&1 &
printf '%s\n' "$!" > /tmp/wechat.pid

printf 'MimicWX desktop is ready. Open the application and scan the WeChat QR code.\n'
cd /home/wechat/mimicwx-linux
while true; do
    exit_code=0
    /usr/local/bin/mimicwx || exit_code=$?
    if [ "$exit_code" -ne 42 ]; then
        exit "$exit_code"
    fi
    sleep 2
done
