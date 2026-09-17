#!/usr/bin/env bash

# Configure Phase 1 of the Project Optic hardening plan.

set -euo pipefail

export LC_ALL=C
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

DRY_RUN=0
DROP_IN_DIR="/etc/systemd/journald.conf.d"
DROP_IN_FILE="$DROP_IN_DIR/60-optic-volatile.conf"
BACKUP_DIR="/var/backups/optic-hardening"

usage() {
    cat <<'EOF'
Usage: setup-phase-01-journaling.sh [--dry-run] [--help]

Idempotently configures volatile systemd journal storage with a 32 MiB limit
and disables rsyslog when it is installed.

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

desired_content() {
    cat <<'EOF'
# Managed by Project Optic setup-phase-01-journaling.sh
[Journal]
Storage=volatile
RuntimeMaxUse=32M
EOF
}

drop_in_matches() {
    [[ -r "$DROP_IN_FILE" ]] && cmp -s "$DROP_IN_FILE" <(desired_content)
}

rsyslog_load=$(systemctl show rsyslog.service -p LoadState --value 2>/dev/null || true)
rsyslog_active=$(systemctl is-active rsyslog.service 2>/dev/null || true)
rsyslog_enabled=$(systemctl is-enabled rsyslog.service 2>/dev/null || true)

if ((DRY_RUN)); then
    printf '%s\n' 'Project Optic Phase 1 dry run'
    if drop_in_matches; then
        printf '[OK] %s already matches the plan.\n' "$DROP_IN_FILE"
    else
        printf '[CHANGE] Write %s with Storage=volatile and RuntimeMaxUse=32M.\n' "$DROP_IN_FILE"
    fi
    if [[ -z "$rsyslog_load" || "$rsyslog_load" == "not-found" ]]; then
        printf '%s\n' '[OK] rsyslog is not installed.'
    elif [[ "$rsyslog_active" == "inactive" &&
            ("$rsyslog_enabled" == "disabled" || "$rsyslog_enabled" == "masked") ]]; then
        printf '[OK] rsyslog is inactive and %s.\n' "$rsyslog_enabled"
    else
        printf '[CHANGE] Disable rsyslog (active=%s, enabled=%s).\n' \
            "${rsyslog_active:-unknown}" "${rsyslog_enabled:-unknown}"
    fi
    printf '%s\n' '[CHANGE] Restart systemd-journald after applying configuration.'
    exit 0
fi

if ((EUID != 0)); then
    if command -v sudo >/dev/null 2>&1; then
        exec sudo -- "$0" "$@"
    fi
    printf '%s\n' 'This setup must run as root.' >&2
    exit 77
fi

printf '%s\n' 'Configuring Project Optic Phase 1: RAM journaling'

install -d -m 0755 "$DROP_IN_DIR"
if ! drop_in_matches; then
    if [[ -e "$DROP_IN_FILE" ]]; then
        install -d -m 0700 "$BACKUP_DIR"
        backup="$BACKUP_DIR/$(basename "$DROP_IN_FILE").$(date +%Y%m%dT%H%M%S).bak"
        cp -a -- "$DROP_IN_FILE" "$backup"
        printf 'Backed up previous configuration to %s\n' "$backup"
    fi

    temporary=$(mktemp "$DROP_IN_DIR/.60-optic-volatile.conf.XXXXXX")
    trap 'rm -f -- "${temporary:-}"' EXIT
    desired_content > "$temporary"
    chmod 0644 "$temporary"
    chown root:root "$temporary"
    mv -f -- "$temporary" "$DROP_IN_FILE"
    temporary=""
    trap - EXIT
    printf 'Installed %s\n' "$DROP_IN_FILE"
else
    printf '%s already matches the plan.\n' "$DROP_IN_FILE"
fi

if [[ -n "$rsyslog_load" && "$rsyslog_load" != "not-found" ]]; then
    systemctl disable --now rsyslog.service
    printf '%s\n' 'Disabled rsyslog.service.'
else
    printf '%s\n' 'rsyslog.service is absent; no action required.'
fi

systemctl restart systemd-journald.service
systemctl is-active --quiet systemd-journald.service

effective=$(systemd-analyze cat-config systemd/journald.conf 2>/dev/null)
journal_storage=$(printf '%s\n' "$effective" |
    awk -F= '/^[[:space:]]*Storage=/{value=$2} END{gsub(/[[:space:]]/, "", value); print value}')
journal_limit=$(printf '%s\n' "$effective" |
    awk -F= '/^[[:space:]]*RuntimeMaxUse=/{value=$2} END{gsub(/[[:space:]]/, "", value); print value}')

if [[ "$journal_storage" != "volatile" || "$journal_limit" != "32M" ]]; then
    printf 'Effective journald configuration is unexpected: Storage=%s RuntimeMaxUse=%s\n' \
        "${journal_storage:-unset}" "${journal_limit:-unset}" >&2
    exit 1
fi

printf '%s\n' 'Phase 1 setup complete: Storage=volatile, RuntimeMaxUse=32M.'