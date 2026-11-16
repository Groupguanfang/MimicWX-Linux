#!/bin/bash
set -euo pipefail

profile_dir=/var/lib/mimicwx/profile
identity_file=/var/lib/mimicwx/machine-id
install -d -o wechat -g wechat -m 0700 "$profile_dir"

if [ "$(stat -c '%d:%i' /home/wechat)" != "$(stat -c '%d:%i' "$profile_dir")" ]; then
    shopt -s dotglob nullglob
    for source in /home/wechat/*; do
        case "${source##*/}" in
            .cache|.xwechat|xwechat_files|mimicwx-linux) continue ;;
        esac
        cp -a "$source" "$profile_dir/"
    done
    if [ -d /home/wechat/.cache/profiles ]; then
        install -d -o wechat -g wechat -m 0700 "$profile_dir/.cache"
        cp -a /home/wechat/.cache/profiles "$profile_dir/.cache/"
    fi
fi

if [ ! -s "$identity_file" ]; then
    if [ -s /var/lib/dbus/machine-id ]; then
        install -m 0600 /var/lib/dbus/machine-id "$identity_file"
    elif [ -s /etc/machine-id ]; then
        install -m 0600 /etc/machine-id "$identity_file"
    else
        umask 077
        dbus-uuidgen > "$identity_file"
    fi
fi

if ! grep -Eq '^[0-9a-f]{32}$' "$identity_file"; then
    printf 'The saved machine identity is invalid; refusing to replace it.\n' >&2
    exit 1
fi

if [ "${1:-}" != "--snapshot" ]; then
    install -m 0644 "$identity_file" /etc/machine-id
    install -m 0644 "$identity_file" /var/lib/dbus/machine-id
fi

printf 'WeChat profile and machine identity are preserved.\n'
