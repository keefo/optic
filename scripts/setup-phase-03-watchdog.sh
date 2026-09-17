#!/usr/bin/env bash

# Configure Phase 3 of the Project Optic hardening plan.

set -euo pipefail

export LC_ALL=C
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

DRY_RUN=0
BACKUP_DIR="/var/backups/optic-hardening"
SYSCTL_FILE="/etc/sysctl.d/99-optic-recovery.conf"
WATCHDOG_FILE="/etc/systemd/system.conf.d/90-optic-watchdog.conf"
TEMPORARY=""

usage() {
    cat <<'EOF'
Usage: setup-phase-03-watchdog.sh [--dry-run] [--help]

Idempotently configures kernel panic recovery and systemd's hardware watchdog.

  --dry-run   Report required changes without modifying the system.
  --help      Display this help.
EOF
}

while (($#)); do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        --help|-h) usage; exit 0 ;;
        *) printf 'Unknown argument: %s\n' "$1" >&2; usage >&2; exit 64 ;;
    esac
    shift
done

sysctl_content=$(cat <<'EOF'
# Managed by Project Optic setup-phase-03-watchdog.sh
# Reboot ten seconds after a kernel panic.
kernel.panic=10
# Turn a kernel oops into a panic so recovery is automatic.
kernel.panic_on_oops=1
EOF
)

watchdog_content=$(cat <<'EOF'
# Managed by Project Optic setup-phase-03-watchdog.sh
[Manager]
RuntimeWatchdogSec=15s
RebootWatchdogSec=2min
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
        printf '[CHANGE] Install managed configuration: %s\n' "$path"
    fi
}

install_managed_file() {
    local path=$1 content=$2 parent backup_name backup
    if file_matches "$path" "$content"; then
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
        printf 'Backed up previous configuration to %s\n' "$backup"
    fi

    TEMPORARY=$(mktemp "$parent/.$(basename "$path").XXXXXX")
    printf '%s\n' "$content" > "$TEMPORARY"
    chmod 0644 "$TEMPORARY"
    chown root:root "$TEMPORARY"
    mv -f -- "$TEMPORARY" "$path"
    TEMPORARY=""
    printf 'Installed %s\n' "$path"
}

if ((DRY_RUN)); then
    printf '%s\n' 'Project Optic Phase 3 dry run'
    show_file_plan "$SYSCTL_FILE" "$sysctl_content"
    show_file_plan "$WATCHDOG_FILE" "$watchdog_content"
    if [[ "$(sysctl -n kernel.panic 2>/dev/null || true)" == "10" &&
          "$(sysctl -n kernel.panic_on_oops 2>/dev/null || true)" == "1" ]]; then
        printf '%s\n' '[OK] Runtime panic recovery settings match the plan.'
    else
        printf '%s\n' '[CHANGE] Apply kernel panic recovery settings.'
    fi
    if [[ "$(systemctl show -p RuntimeWatchdogUSec --value 2>/dev/null || true)" == "15s" &&
          "$(systemctl show -p RebootWatchdogUSec --value 2>/dev/null || true)" == "2min" ]]; then
        printf '%s\n' '[OK] Runtime watchdog settings match the plan.'
    else
        printf '%s\n' '[CHANGE] Re-execute systemd to apply watchdog settings.'
    fi
    exit 0
fi

if ((EUID != 0)); then
    if command -v sudo >/dev/null 2>&1; then
        exec sudo -- "$0"
    fi
    printf '%s\n' 'This setup must run as root.' >&2
    exit 77
fi

if [[ ! -c /dev/watchdog0 || ! -r /sys/class/watchdog/watchdog0/identity ]]; then
    printf '%s\n' 'The hardware watchdog device is unavailable; no changes were made.' >&2
    exit 69
fi

printf 'Configuring Project Optic Phase 3 with %s\n' \
    "$(cat /sys/class/watchdog/watchdog0/identity)"

install_managed_file "$SYSCTL_FILE" "$sysctl_content"
install_managed_file "$WATCHDOG_FILE" "$watchdog_content"

effective=$(systemd-analyze cat-config systemd/system.conf 2>/dev/null)
runtime_setting=$(printf '%s\n' "$effective" |
    awk -F= '/^[[:space:]]*RuntimeWatchdogSec=/{value=$2} END{gsub(/[[:space:]]/, "", value); print value}')
reboot_setting=$(printf '%s\n' "$effective" |
    awk -F= '/^[[:space:]]*RebootWatchdogSec=/{value=$2} END{gsub(/[[:space:]]/, "", value); print value}')
if [[ "$runtime_setting" != "15s" || "$reboot_setting" != "2min" ]]; then
    printf 'Effective watchdog configuration is unexpected: runtime=%s reboot=%s\n' \
        "${runtime_setting:-unset}" "${reboot_setting:-unset}" >&2
    exit 1
fi

sysctl --load "$SYSCTL_FILE"
systemctl daemon-reexec

for attempt in {1..10}; do
    runtime_watchdog=$(systemctl show -p RuntimeWatchdogUSec --value 2>/dev/null || true)
    reboot_watchdog=$(systemctl show -p RebootWatchdogUSec --value 2>/dev/null || true)
    [[ "$runtime_watchdog" == "15s" && "$reboot_watchdog" == "2min" ]] && break
    sleep 1
done

[[ "$(sysctl -n kernel.panic)" == "10" ]]
[[ "$(sysctl -n kernel.panic_on_oops)" == "1" ]]
[[ "$runtime_watchdog" == "15s" ]]
[[ "$reboot_watchdog" == "2min" ]]
[[ "$(cat /sys/class/watchdog/watchdog0/state)" == "active" ]]

printf '%s\n' 'Phase 3 setup complete: panic recovery and hardware watchdog are active.'