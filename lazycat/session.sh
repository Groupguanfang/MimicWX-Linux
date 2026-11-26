#!/bin/bash
set -euo pipefail

export DISPLAY=:1
export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1
export QT_ACCESSIBILITY=1
export NO_AT_BRIDGE=0
export RUST_LOG=mimicwx=info
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/mimicwx-runtime}"
export PULSE_SERVER="unix:/tmp/mimicwx-pulse/native"
install -d -m 0700 "$XDG_RUNTIME_DIR" /tmp/mimicwx-pulse

cat > /tmp/mimicwx-pulse.pa <<'PULSE'
.fail
load-module module-native-protocol-unix socket=/tmp/mimicwx-pulse/native auth-anonymous=1
load-module module-null-sink sink_name=mimicwx_output sink_properties=device.description=MimicWX_Output
load-module module-null-sink sink_name=mimicwx_voice_sink rate=24000 channels=1 format=s16le sink_properties=device.description=MimicWX_Voice
load-module module-remap-source master=mimicwx_voice_sink.monitor source_name=mimicwx_voice source_properties=device.description=MimicWX_Voice
set-default-sink mimicwx_output
set-default-source mimicwx_voice
PULSE

if ! pactl info >/dev/null 2>&1; then
    pulseaudio --daemonize=yes --exit-idle-time=-1 --disallow-exit \
        --log-target=file:/tmp/mimicwx-pulse.log -n -F /tmp/mimicwx-pulse.pa
fi
for attempt in $(seq 1 20); do
    pactl info >/dev/null 2>&1 && break
    sleep 0.25
done
pactl info >/dev/null
pactl set-default-sink mimicwx_output
pactl set-default-source mimicwx_voice

Xtigervnc :1 -geometry 1280x800 -depth 24 \
    -SecurityTypes None -localhost yes -AlwaysShared -AcceptSetDesktopSize=0 \
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

wechat_supervisor() {
    local failures=0
    while true; do
        local started_at
        started_at=$(date +%s)
        wechat --no-sandbox --disable-gpu >> /tmp/mimicwx-wechat.log 2>&1 &
        local wechat_pid=$!
        printf '%s\n' "$wechat_pid" > /tmp/wechat.pid

        local status=0
        wait "$wechat_pid" || status=$?
        local running_for=$(( $(date +%s) - started_at ))
        printf 'WeChat exited with code %s after %ss; restarting.\n' \
            "$status" "$running_for" >> /tmp/mimicwx-wechat.log

        if [ "$running_for" -ge 60 ]; then
            failures=0
        else
            failures=$((failures + 1))
        fi
        if [ "$failures" -ge 3 ]; then
            sleep 30
            failures=0
        else
            sleep 2
        fi
    done
}

wechat_supervisor_pid=
cleanup() {
    if [ -n "$wechat_supervisor_pid" ]; then
        kill "$wechat_supervisor_pid" 2>/dev/null || true
    fi
    if [ -s /tmp/wechat.pid ]; then
        kill "$(cat /tmp/wechat.pid)" 2>/dev/null || true
    fi
    kill "$desktop_pid" 2>/dev/null || true
}
trap cleanup EXIT

wechat_supervisor &
wechat_supervisor_pid=$!

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
