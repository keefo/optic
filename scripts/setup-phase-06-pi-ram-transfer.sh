#!/usr/bin/env bash

# Configure the Pi side of Phase 6: RAM capture staging and verified iMac transfer.

set -euo pipefail

export LC_ALL=C
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

ORIGINAL_ARGS=("$@")
DRY_RUN=0
HOST_KEY_FILE=""
CAPTURE_USER="liam"
CAPTURE_GROUP="liam"
CAPTURE_UID=1000
CAPTURE_GID=1000
CAPTURE_DIR="/mnt/capture"
REMOTE_HOST="192.168.0.231"
REMOTE_PORT="2222"
REMOTE_USER="admin"
BACKUP_DIR="/var/backups/optic-hardening"
CONFIG_DIR="/etc/optic"
CONFIG_FILE="$CONFIG_DIR/capture-transfer.conf"
MOUNT_UNIT="/etc/systemd/system/mnt-capture.mount"
SERVICE_UNIT="/etc/systemd/system/optic-capture-transfer.service"
TIMER_UNIT="/etc/systemd/system/optic-capture-transfer.timer"
SENDER_SOURCE="$(cd "$(dirname "$0")" && pwd)/optic-capture-transfer.sh"
SENDER_TARGET="/usr/local/libexec/optic-capture-transfer"
USER_HOME="/home/$CAPTURE_USER"
PRIVATE_KEY="$USER_HOME/.ssh/optic_capture_ed25519"
PUBLIC_KEY="$PRIVATE_KEY.pub"
KNOWN_HOSTS="$USER_HOME/.ssh/optic_capture_known_hosts"
BESZEL_DROPIN="$USER_HOME/.config/systemd/user/beszel-agent.service.d/60-optic-capture-ram.conf"
TEMPORARY=""

usage() {
    cat <<'EOF'
Usage: setup-phase-06-pi-ram-transfer.sh [--dry-run] [--host-key FILE] [--help]

Configures a 256 MiB tmpfs capture stage and a verified SSH push service to
the dedicated iMac receiver at 192.168.0.231:2222.

  --dry-run        Report required changes without modifying the system.
  --host-key FILE  Pin the exported iMac receiver public host key.
  --help           Display this help.
EOF
}

while (($#)); do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        --host-key)
            [[ $# -ge 2 ]] || { printf '%s\n' 'Missing value for --host-key' >&2; exit 64; }
            HOST_KEY_FILE=$2
            shift
            ;;
        --help|-h) usage; exit 0 ;;
        *) printf 'Unknown argument: %s\n' "$1" >&2; usage >&2; exit 64 ;;
    esac
    shift
done

mount_content=$(cat <<EOF
# Managed by Project Optic setup-phase-06-pi-ram-transfer.sh
[Unit]
Description=Project Optic RAM capture staging
Before=optic-capture-transfer.service

[Mount]
What=tmpfs
Where=$CAPTURE_DIR
Type=tmpfs
Options=mode=0750,uid=$CAPTURE_UID,gid=$CAPTURE_GID,size=256M,nosuid,nodev,noexec,noatime

[Install]
WantedBy=local-fs.target
EOF
)

config_content=$(cat <<EOF
# Managed by Project Optic setup-phase-06-pi-ram-transfer.sh
STAGING_DIR=$CAPTURE_DIR
REMOTE_HOST=$REMOTE_HOST
REMOTE_PORT=$REMOTE_PORT
REMOTE_USER=$REMOTE_USER
IDENTITY_FILE=$PRIVATE_KEY
KNOWN_HOSTS_FILE=$KNOWN_HOSTS
EOF
)

service_content=$(cat <<EOF
# Managed by Project Optic setup-phase-06-pi-ram-transfer.sh
[Unit]
Description=Project Optic verified capture transfer
Requires=mnt-capture.mount
After=mnt-capture.mount network-online.target
Wants=network-online.target

[Service]
Type=oneshot
User=$CAPTURE_USER
Group=$CAPTURE_GROUP
ExecStart=$SENDER_TARGET
UMask=0027
Nice=10
IOSchedulingClass=idle
NoNewPrivileges=true
PrivateDevices=true
PrivateTmp=true
ProtectClock=true
ProtectControlGroups=true
ProtectHome=read-only
ProtectHostname=true
ProtectKernelLogs=true
ProtectKernelModules=true
ProtectKernelTunables=true
ProtectSystem=strict
ReadWritePaths=$CAPTURE_DIR
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
RestrictNamespaces=true
RestrictSUIDSGID=true
SystemCallArchitectures=native
EOF
)

timer_content=$(cat <<'EOF'
# Managed by Project Optic setup-phase-06-pi-ram-transfer.sh
[Unit]
Description=Project Optic capture transfer schedule

[Timer]
OnBootSec=30s
OnUnitActiveSec=15s
AccuracySec=1s
RandomizedDelaySec=3s
Unit=optic-capture-transfer.service

[Install]
WantedBy=timers.target
EOF
)

beszel_content=$(cat <<'EOF'
# Managed by Project Optic setup-phase-06-pi-ram-transfer.sh
[Service]
Environment="EXTRA_FILESYSTEMS=/mnt/capture__Capture-RAM"
EOF
)

cleanup() {
    [[ -z "$TEMPORARY" ]] || rm -f -- "$TEMPORARY"
}
trap cleanup EXIT

file_matches() {
    local path=$1 content=$2
    [[ -r "$path" ]] && cmp -s "$path" <(printf '%s\n' "$content")
}

show_file_plan() {
    local path=$1 content=$2
    if file_matches "$path" "$content"; then
        printf '[OK] %s already matches the plan.\n' "$path"
    else
        printf '[CHANGE] Install managed file: %s\n' "$path"
    fi
}

install_managed_file() {
    local path=$1 content=$2 mode=${3:-0644} owner=${4:-root} group=${5:-root}
    local parent backup_name backup
    if file_matches "$path" "$content"; then
        chmod "$mode" "$path"
        chown "$owner:$group" "$path"
        printf '%s already matches the plan.\n' "$path"
        return 0
    fi

    parent=$(dirname "$path")
    install -d -m 0755 "$parent"
    if [[ -e "$path" ]]; then
        install -d -m 0700 "$BACKUP_DIR"
        backup_name=${path#/}
        backup_name=${backup_name//\//_}
        backup="$BACKUP_DIR/$backup_name.$(date +%Y%m%dT%H%M%S).bak"
        cp -a -- "$path" "$backup"
        printf 'Backed up previous file to %s\n' "$backup"
    fi

    TEMPORARY=$(mktemp "$parent/.$(basename "$path").XXXXXX")
    printf '%s\n' "$content" > "$TEMPORARY"
    chmod "$mode" "$TEMPORARY"
    chown "$owner:$group" "$TEMPORARY"
    mv -f -- "$TEMPORARY" "$path"
    TEMPORARY=""
    printf 'Installed %s\n' "$path"
}

if ((DRY_RUN)); then
    printf '%s\n' 'Project Optic Phase 6 Pi dry run'
    [[ -x "$SENDER_SOURCE" ]] || {
        printf '[BLOCKED] Transfer helper is missing or not executable: %s\n' "$SENDER_SOURCE"
        exit 69
    }
    show_file_plan "$MOUNT_UNIT" "$mount_content"
    show_file_plan "$CONFIG_FILE" "$config_content"
    show_file_plan "$SERVICE_UNIT" "$service_content"
    show_file_plan "$TIMER_UNIT" "$timer_content"
    show_file_plan "$BESZEL_DROPIN" "$beszel_content"
    [[ -f "$PRIVATE_KEY" ]] && printf '%s\n' '[OK] Capture transfer key already exists.' ||
        printf '%s\n' '[CHANGE] Generate a dedicated capture transfer key.'
    if [[ -n "$HOST_KEY_FILE" ]]; then
        [[ -r "$HOST_KEY_FILE" ]] || { printf '[BLOCKED] Host key is unreadable: %s\n' "$HOST_KEY_FILE"; exit 69; }
        printf '[CHANGE] Pin receiver host key from %s.\n' "$HOST_KEY_FILE"
    elif [[ -s "$KNOWN_HOSTS" ]]; then
        printf '%s\n' '[OK] Receiver host key is already pinned.'
    else
        printf '%s\n' '[INFO] Receiver host key is not pinned yet; rerun with --host-key after iMac setup.'
    fi
    exit 0
fi

if ((EUID != 0)); then
    if command -v sudo >/dev/null 2>&1; then
        exec sudo -- "$0" "${ORIGINAL_ARGS[@]}"
    fi
    printf '%s\n' 'This setup must run as root.' >&2
    exit 77
fi

[[ -x "$SENDER_SOURCE" ]] || {
    printf 'Transfer helper is missing or not executable: %s\n' "$SENDER_SOURCE" >&2
    exit 69
}
[[ "$(id -u "$CAPTURE_USER")" == "$CAPTURE_UID" ]] || {
    printf 'Unexpected UID for %s.\n' "$CAPTURE_USER" >&2
    exit 69
}
[[ "$(id -g "$CAPTURE_USER")" == "$CAPTURE_GID" ]] || {
    printf 'Unexpected GID for %s.\n' "$CAPTURE_USER" >&2
    exit 69
}

printf '%s\n' 'Configuring Project Optic Phase 6 Pi RAM stage and transfer service'

install -d -m 0755 "$CONFIG_DIR" /usr/local/libexec
install -d -m 0700 -o "$CAPTURE_USER" -g "$CAPTURE_GROUP" "$USER_HOME/.ssh"
install -d -m 0755 -o "$CAPTURE_USER" -g "$CAPTURE_GROUP" \
    "$USER_HOME/.config/systemd/user/beszel-agent.service.d"
install -d -m 0750 -o "$CAPTURE_USER" -g "$CAPTURE_GROUP" "$CAPTURE_DIR"

install -m 0755 -o root -g root "$SENDER_SOURCE" "$SENDER_TARGET"
install_managed_file "$MOUNT_UNIT" "$mount_content"
install_managed_file "$CONFIG_FILE" "$config_content" 0644 root root
install_managed_file "$SERVICE_UNIT" "$service_content"
install_managed_file "$TIMER_UNIT" "$timer_content"
install_managed_file "$BESZEL_DROPIN" "$beszel_content" 0644 "$CAPTURE_USER" "$CAPTURE_GROUP"

if [[ ! -f "$PRIVATE_KEY" ]]; then
    sudo -u "$CAPTURE_USER" ssh-keygen -q -t ed25519 -N '' \
        -C 'Project-Optic-capture-transfer' -f "$PRIVATE_KEY"
    printf 'Generated %s.\n' "$PRIVATE_KEY"
fi
chmod 0600 "$PRIVATE_KEY"
chmod 0644 "$PUBLIC_KEY"
chown "$CAPTURE_USER:$CAPTURE_GROUP" "$PRIVATE_KEY" "$PUBLIC_KEY"

if [[ -n "$HOST_KEY_FILE" ]]; then
    [[ -r "$HOST_KEY_FILE" ]] || { printf 'Host key is unreadable: %s\n' "$HOST_KEY_FILE" >&2; exit 69; }
    read -r host_key_type host_key_data _ < "$HOST_KEY_FILE"
    [[ "$host_key_type" == "ssh-ed25519" && -n "$host_key_data" ]] || {
        printf '%s\n' 'Receiver host key must be an ssh-ed25519 public key.' >&2
        exit 69
    }
    printf '[%s]:%s %s %s\n' "$REMOTE_HOST" "$REMOTE_PORT" \
        "$host_key_type" "$host_key_data" > "$KNOWN_HOSTS"
    chmod 0600 "$KNOWN_HOSTS"
    chown "$CAPTURE_USER:$CAPTURE_GROUP" "$KNOWN_HOSTS"
    printf 'Pinned receiver host key in %s.\n' "$KNOWN_HOSTS"
fi

systemd-analyze verify "$MOUNT_UNIT" "$SERVICE_UNIT" "$TIMER_UNIT"
systemctl daemon-reload
systemctl enable --now mnt-capture.mount
chown "$CAPTURE_USER:$CAPTURE_GROUP" "$CAPTURE_DIR"
chmod 0750 "$CAPTURE_DIR"

sudo -u "$CAPTURE_USER" XDG_RUNTIME_DIR="/run/user/$CAPTURE_UID" \
    systemctl --user daemon-reload
sudo -u "$CAPTURE_USER" XDG_RUNTIME_DIR="/run/user/$CAPTURE_UID" \
    systemctl --user restart beszel-agent.service

if [[ -s "$KNOWN_HOSTS" ]]; then
    systemctl enable --now optic-capture-transfer.timer
    systemctl start optic-capture-transfer.service
    printf '%s\n' 'Capture transfer timer is enabled.'
else
    systemctl disable --now optic-capture-transfer.timer 2>/dev/null || true
    printf '%s\n' 'Pi setup complete; transfer timer awaits the pinned iMac host key.'
fi

printf 'Pi capture public key: %s\n' "$PUBLIC_KEY"