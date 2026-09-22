#!/usr/bin/env bash

# Install optic-daemon as a system service running as liam, and migrate away
# from the former systemd user service. Also installs the bounded NTP wait
# that the unit is ordered after. Design: docs/optic-daemon-system-service.md.
#
# Run from a checkout or staged source tree on the Pi (it reads systemd/ next
# to this script). Deploys never run this; they only restart the unit, and
# refuse to deploy if the installed unit differs from systemd/.

set -euo pipefail

export LC_ALL=C
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

ORIGINAL_ARGS=("$@")
MODE=install
DAEMON_USER=liam
PROJECT_ROOT=$(cd "$(dirname "$0")/.." && pwd)
UNIT_NAME=optic-daemon.service
UNIT_SOURCE="$PROJECT_ROOT/systemd/$UNIT_NAME"
UNIT_TARGET="/etc/systemd/system/$UNIT_NAME"
DROPIN_SOURCE="$PROJECT_ROOT/systemd/systemd-time-wait-sync.service.d/optic-bounded-wait.conf"
DROPIN_TARGET="/etc/systemd/system/systemd-time-wait-sync.service.d/optic-bounded-wait.conf"
USER_UNIT="/home/$DAEMON_USER/.config/systemd/user/$UNIT_NAME"
BINARY="/home/$DAEMON_USER/.local/bin/optic-daemon"
MANAGE_RULE="/etc/polkit-1/rules.d/64-optic-daemon-manage-unit.rules"
BACKUP_DIR="/var/backups/optic-hardening"
USER_UNIT_BACKUP="$BACKUP_DIR/optic-daemon-user-unit.service"

usage() {
    cat <<'EOF'
Usage: setup-optic-daemon-system-service.sh [--dry-run | --rollback] [--help]

Installs /etc/systemd/system/optic-daemon.service (User=liam) and the bounded
systemd-time-wait-sync drop-in, stops and removes the old user unit, starts
the system unit, and checks its health. A failed health check rolls back to
the user unit automatically.

  --dry-run   Report what would change; modifies nothing.
  --rollback  Return to the user service: stop and remove the system unit
              and drop-in, restore the saved user unit, and start it.
  --help      Display this help.
EOF
}

while (($#)); do
    case "$1" in
        --dry-run) MODE=dry-run ;;
        --rollback) MODE=rollback ;;
        --help|-h) usage; exit 0 ;;
        *) printf 'Unknown argument: %s\n' "$1" >&2; usage >&2; exit 64 ;;
    esac
    shift
done

user_systemctl() {
    systemctl --user -M "$DAEMON_USER@" "$@"
}

# Healthy means the HTTP API answers with the camera detected, so a device
# access mistake in the unit's sandbox fails the check instead of passing.
healthy() {
    local status
    for _ in {1..60}; do
        if status=$(curl -fsS --max-time 2 http://127.0.0.1:8000/api/status 2>/dev/null) &&
           python3 -c 'import json, sys; sys.exit(0 if json.load(sys.stdin)["camera"]["detected"] else 1)' \
               <<<"$status" 2>/dev/null; then
            return 0
        fi
        sleep 1
    done
    return 1
}

file_state() {
    local source=$1 target=$2
    if [[ ! -e $target ]]; then
        printf 'missing'
    elif cmp -s "$source" "$target"; then
        printf 'matches'
    else
        printf 'differs'
    fi
}

[[ -r $UNIT_SOURCE ]] || { printf 'Unit source is missing: %s\n' "$UNIT_SOURCE" >&2; exit 69; }
[[ -r $DROPIN_SOURCE ]] || { printf 'Drop-in source is missing: %s\n' "$DROPIN_SOURCE" >&2; exit 69; }

if [[ $MODE == dry-run ]]; then
    printf '%s\n' 'Project Optic optic-daemon system service dry run'
    printf '[%s] %s\n' "$(file_state "$UNIT_SOURCE" "$UNIT_TARGET")" "$UNIT_TARGET"
    printf '[%s] %s\n' "$(file_state "$DROPIN_SOURCE" "$DROPIN_TARGET")" "$DROPIN_TARGET"
    printf '[%s] systemd-time-wait-sync.service\n' \
        "$(systemctl is-enabled systemd-time-wait-sync.service 2>/dev/null || true)"
    printf '[system unit %s/%s]\n' \
        "$(systemctl is-enabled "$UNIT_NAME" 2>/dev/null || true)" \
        "$(systemctl is-active "$UNIT_NAME" 2>/dev/null || true)"
    if [[ -e $USER_UNIT ]]; then
        printf '[CHANGE] Stop, disable, and back up the user unit: %s\n' "$USER_UNIT"
    else
        printf '[OK] No user unit installed.\n'
    fi
    [[ -x $BINARY ]] || printf '[MISSING] Daemon binary: %s\n' "$BINARY"
    [[ -e $MANAGE_RULE ]] || printf '[UNKNOWN] %s not visible (needs root to read, or not installed yet).\n' "$MANAGE_RULE"
    exit 0
fi

if ((EUID != 0)); then
    if [[ -f "$0" ]] && command -v sudo >/dev/null 2>&1; then
        exec sudo -- "$0" "${ORIGINAL_ARGS[@]}"
    fi
    printf '%s\n' 'This setup must run as root.' >&2
    exit 77
fi

backup_file() {
    local path=$1 name
    [[ -e $path ]] || return 0
    install -d -m 0700 "$BACKUP_DIR"
    name=${path#/}
    name=${name//\//_}
    cp -a -- "$path" "$BACKUP_DIR/$name.$(date +%Y%m%dT%H%M%S).bak"
}

start_user_unit() {
    printf 'Restoring the user unit from %s\n' "$USER_UNIT_BACKUP"
    install -d -o "$DAEMON_USER" -g "$DAEMON_USER" -m 0755 "$(dirname "$USER_UNIT")"
    install -o "$DAEMON_USER" -g "$DAEMON_USER" -m 0644 "$USER_UNIT_BACKUP" "$USER_UNIT"
    user_systemctl daemon-reload
    user_systemctl enable --now "$UNIT_NAME"
}

remove_system_unit() {
    systemctl disable --now "$UNIT_NAME" 2>/dev/null || true
    backup_file "$UNIT_TARGET"
    rm -f -- "$UNIT_TARGET" "$DROPIN_TARGET"
    rmdir --ignore-fail-on-non-empty "$(dirname "$DROPIN_TARGET")" 2>/dev/null || true
    systemctl disable systemd-time-wait-sync.service 2>/dev/null || true
    systemctl daemon-reload
}

rollback() {
    printf '%s\n' 'Rolling back to the optic-daemon user service.'
    [[ -r $USER_UNIT_BACKUP ]] || {
        printf 'No saved user unit at %s; the system unit is left in place.\n' "$USER_UNIT_BACKUP" >&2
        exit 1
    }
    remove_system_unit
    start_user_unit
    if healthy; then
        printf '%s\n' 'Rollback complete: optic-daemon runs as the user service again.'
    else
        printf '%s\n' 'Rollback finished, but the user service is not healthy.' >&2
        user_systemctl --no-pager --full status "$UNIT_NAME" >&2 || true
        exit 1
    fi
}

if [[ $MODE == rollback ]]; then
    rollback
    exit 0
fi

# Preconditions, all checked before anything changes.
id "$DAEMON_USER" >/dev/null 2>&1 || { printf 'User %s does not exist.\n' "$DAEMON_USER" >&2; exit 69; }
[[ -x $BINARY ]] || { printf 'Daemon binary is missing: %s (deploy it first).\n' "$BINARY" >&2; exit 69; }
[[ $(findmnt -n -o FSTYPE /mnt/capture 2>/dev/null || true) == tmpfs ]] || {
    printf '%s\n' '/mnt/capture is not the Phase 6 tmpfs; no changes were made.' >&2
    exit 69
}
for group in video render; do
    id -nG "$DAEMON_USER" | tr ' ' '\n' | grep -qx "$group" || {
        printf '%s is not in group %s; no changes were made.\n' "$DAEMON_USER" "$group" >&2
        exit 69
    }
done
[[ -e $MANAGE_RULE ]] || {
    printf '%s is missing; run scripts/setup-phase-08-daemon-host-access.sh first, so deploys and Restart daemon keep working.\n' \
        "$MANAGE_RULE" >&2
    exit 69
}

printf '%s\n' 'Installing optic-daemon as a system service'
backup_file "$UNIT_TARGET"
backup_file "$DROPIN_TARGET"
install -m 0644 "$UNIT_SOURCE" "$UNIT_TARGET"
install -d -m 0755 "$(dirname "$DROPIN_TARGET")"
install -m 0644 "$DROPIN_SOURCE" "$DROPIN_TARGET"
systemd-analyze verify "$UNIT_TARGET"
systemctl daemon-reload
systemctl enable systemd-time-wait-sync.service

# The user unit must be stopped before the system unit starts: both would
# claim the camera and port 8000.
if [[ -e $USER_UNIT ]]; then
    printf 'Stopping and removing the user unit %s\n' "$USER_UNIT"
    user_systemctl disable --now "$UNIT_NAME" || true
    install -d -m 0700 "$BACKUP_DIR"
    cp -a -- "$USER_UNIT" "$USER_UNIT_BACKUP"
    backup_file "$USER_UNIT"
    rm -f -- "$USER_UNIT"
    user_systemctl daemon-reload
fi

systemctl enable "$UNIT_NAME"
systemctl restart "$UNIT_NAME"
if healthy; then
    printf 'optic-daemon runs as a system service: User=%s, %s.\n' \
        "$(systemctl show -p User --value "$UNIT_NAME")" \
        "$(systemctl is-active "$UNIT_NAME")"
else
    printf '%s\n' 'The system service did not become healthy (API up with the camera detected).' >&2
    systemctl --no-pager --full status "$UNIT_NAME" >&2 || true
    journalctl -u "$UNIT_NAME" -n 30 --no-pager >&2 || true
    rollback
    exit 1
fi
