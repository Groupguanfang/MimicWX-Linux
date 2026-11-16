#!/bin/bash
set -euo pipefail

install -d -o wechat -g wechat -m 0700 \
    /home/wechat/.xwechat /home/wechat/xwechat_files \
    /home/wechat/mimicwx-linux /var/lib/mimicwx
install -d -m 0755 /run/dbus
install -d -m 1777 /tmp/.X11-unix
chown wechat:wechat /home/wechat/.xwechat /home/wechat/xwechat_files \
    /home/wechat/mimicwx-linux /var/lib/mimicwx

if [ ! -s /var/lib/mimicwx/api-token ]; then
    umask 077
    openssl rand -hex 32 > /var/lib/mimicwx/api-token
fi

if [ ! -e /var/lib/mimicwx/wechat-key ]; then
    install -o wechat -g wechat -m 0600 /dev/null /var/lib/mimicwx/wechat-key
fi
ln -sfn /var/lib/mimicwx/wechat-key /tmp/wechat_key.txt

if [ ! -f /home/wechat/mimicwx-linux/config.toml ]; then
    umask 077
    {
        printf '[api]\ntoken = "%s"\n\n' "$(cat /var/lib/mimicwx/api-token)"
        printf '[listen]\nauto = []\n'
        printf '\n[onebot]\nenabled = true\ntoken = ""\n'
    } > /home/wechat/mimicwx-linux/config.toml
fi
if ! grep -Eq '^\[onebot\][[:space:]]*$' /home/wechat/mimicwx-linux/config.toml; then
    printf '\n[onebot]\nenabled = true\ntoken = ""\n' >> /home/wechat/mimicwx-linux/config.toml
fi
chown wechat:wechat /home/wechat/mimicwx-linux/config.toml \
    /var/lib/mimicwx/api-token /var/lib/mimicwx/wechat-key
chmod 0600 /home/wechat/mimicwx-linux/config.toml \
    /var/lib/mimicwx/api-token /var/lib/mimicwx/wechat-key

dbus-daemon --system --fork --nopidfile

if [ ! -s /var/lib/mimicwx/wechat-key ]; then
    (
        for attempt in $(seq 1 90); do
            if [ -s /tmp/wechat.pid ]; then
                sleep 2
                gdb --batch --nx --pid="$(cat /tmp/wechat.pid)" \
                    --command=/usr/local/bin/extract-key.py \
                    > /var/lib/mimicwx/key-extraction.log 2>&1
                exit "$?"
            fi
            sleep 1
        done
        printf 'Waiting for WeChat process timed out.\n' \
            > /var/lib/mimicwx/key-extraction.log
    ) &
fi

exec runuser --user wechat -- dbus-run-session -- /usr/local/bin/mimicwx-session
