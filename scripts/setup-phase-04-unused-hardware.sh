#!/usr/bin/env bash

# Configure Phase 4 of the Project Optic hardening plan.

set -euo pipefail

export LC_ALL=C
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

DRY_RUN=0
REBOOT=0
BACKUP_DIR="/var/backups/optic-hardening"
FIRMWARE_FILE="/boot/firmware/config.txt"
TEMPORARY=""

usage() {
    cat <<'EOF'
Usage: setup-phase-04-unused-hardware.sh [--dry-run] [--reboot] [--help]

Idempotently disables unused Bluetooth and onboard audio hardware and enables
HDMI blanking while preserving SSH, networking, Avahi, camera, and KMS settings.

  --dry-run   Report required changes without modifying the system.
  --reboot    Reboot when firmware configuration changes require it.
  --help      Display this help.
EOF
}

while (($#)); do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        --reboot) REBOOT=1 ;;
        --help|-h) usage; exit 0 ;;
        *) printf 'Unknown argument: %s\n' "$1" >&2; usage >&2; exit 64 ;;
    esac
    shift
done

if ((DRY_RUN && REBOOT)); then
    printf '%s\n' '--dry-run and --reboot cannot be used together.' >&2
    exit 64
fi

cleanup() {
    [[ -z "$TEMPORARY" ]] || rm -f -- "$TEMPORARY"
}
trap cleanup EXIT

render_firmware_config() {
    awk '
        BEGIN { managed = 0; kept_count = 0 }
        /^# BEGIN Project Optic Phase 4$/ { managed = 1; next }
        /^# END Project Optic Phase 4$/ { managed = 0; next }
        managed { next }
        {
            normalized = $0
            sub(/#.*/, "", normalized)
            gsub(/[[:space:]]/, "", normalized)
            if (normalized ~ /^dtoverlay=disable-bt(,.*)?$/) next
            if (normalized ~ /^dtparam=audio=/) next
            if (normalized ~ /^hdmi_blanking=/) next
            kept[++kept_count] = $0
        }
        END {
            while (kept_count > 0 && kept[kept_count] ~ /^[[:space:]]*$/) kept_count--
            for (line_number = 1; line_number <= kept_count; line_number++) print kept[line_number]
            print ""
            print "# BEGIN Project Optic Phase 4"
            print "[all]"
            print "# Disable the unused Bluetooth controller."
            print "dtoverlay=disable-bt"
            print "# Disable unused onboard audio; KMS HDMI audio is separate."
            print "dtparam=audio=off"
            print "# Permit HDMI output blanking when the display is idle."
            print "hdmi_blanking=2"
            print "# END Project Optic Phase 4"
        }
    ' "$FIRMWARE_FILE"
}

firmware_matches() {
    [[ -r "$FIRMWARE_FILE" ]] && cmp -s "$FIRMWARE_FILE" <(render_firmware_config)
}

bluetooth_matches() {
    local load active enabled
    load=$(systemctl show bluetooth.service -p LoadState --value 2>/dev/null || true)
    active=$(systemctl is-active bluetooth.service 2>/dev/null || true)
    enabled=$(systemctl is-enabled bluetooth.service 2>/dev/null || true)
    [[ -z "$load" || "$load" == "not-found" ]] ||
        [[ "$active" != "active" && ("$enabled" == "disabled" || "$enabled" == "masked") ]]
}

if ((DRY_RUN)); then
    printf '%s\n' 'Project Optic Phase 4 dry run'
    if bluetooth_matches; then
        printf '%s\n' '[OK] bluetooth.service is absent or disabled and inactive.'
    else
        printf '%s\n' '[CHANGE] Disable and stop bluetooth.service.'
    fi
    if firmware_matches; then
        printf '[OK] %s already matches the plan.\n' "$FIRMWARE_FILE"
    else
        printf '[CHANGE] Back up and update target directives in %s.\n' "$FIRMWARE_FILE"
        printf '%s\n' '[CHANGE] A reboot is required to activate firmware changes.'
    fi
    exit 0
fi

if ((EUID != 0)); then
    sudo_arguments=()
    ((REBOOT)) && sudo_arguments+=(--reboot)
    if command -v sudo >/dev/null 2>&1; then
        exec sudo -- "$0" "${sudo_arguments[@]}"
    fi
    printf '%s\n' 'This setup must run as root.' >&2
    exit 77
fi

if [[ ! -f "$FIRMWARE_FILE" ]]; then
    printf 'Firmware configuration is unavailable: %s\n' "$FIRMWARE_FILE" >&2
    exit 69
fi

if [[ "$(systemctl is-active avahi-daemon.service 2>/dev/null || true)" != "active" ]]; then
    printf '%s\n' 'Avahi is not active; refusing to alter hardware configuration.' >&2
    exit 69
fi

printf '%s\n' 'Configuring Project Optic Phase 4: unused services and hardware'

bluetooth_load=$(systemctl show bluetooth.service -p LoadState --value 2>/dev/null || true)
if [[ -n "$bluetooth_load" && "$bluetooth_load" != "not-found" ]]; then
    systemctl disable --now bluetooth.service
    printf '%s\n' 'Disabled bluetooth.service.'
else
    printf '%s\n' 'bluetooth.service is absent; no action required.'
fi

FIRMWARE_CHANGED=0
if firmware_matches; then
    printf '%s already matches the plan.\n' "$FIRMWARE_FILE"
else
    install -d -m 0700 "$BACKUP_DIR"
    backup="$BACKUP_DIR/config.txt.phase-04.$(date +%Y%m%dT%H%M%S).bak"
    cp -a -- "$FIRMWARE_FILE" "$backup"
    printf 'Backed up firmware configuration to %s\n' "$backup"

    TEMPORARY=$(mktemp "$(dirname "$FIRMWARE_FILE")/.config.txt.optic-phase-04.XXXXXX")
    render_firmware_config > "$TEMPORARY"

    for expected in dtoverlay=disable-bt dtparam=audio=off hdmi_blanking=2; do
        count=$(awk -v expected="$expected" '
            {
                line = $0
                sub(/#.*/, "", line)
                gsub(/[[:space:]]/, "", line)
                if (line == expected) count++
            }
            END { print count + 0 }
        ' "$TEMPORARY")
        [[ "$count" == "1" ]] || {
            printf 'Candidate firmware configuration has %s copies of %s.\n' "$count" "$expected" >&2
            exit 1
        }
    done

    grep -Eq '^[[:space:]]*camera_auto_detect=1([[:space:]]*(#.*)?)?$' "$TEMPORARY"
    grep -Eq '^[[:space:]]*dtoverlay=vc4-kms-v3d([[:space:]]*(#.*)?)?$' "$TEMPORARY"

    chmod --reference="$FIRMWARE_FILE" "$TEMPORARY"
    chown --reference="$FIRMWARE_FILE" "$TEMPORARY"
    mv -f -- "$TEMPORARY" "$FIRMWARE_FILE"
    TEMPORARY=""
    FIRMWARE_CHANGED=1
    printf 'Updated %s.\n' "$FIRMWARE_FILE"
fi

bluetooth_matches
[[ "$(systemctl is-active avahi-daemon.service 2>/dev/null || true)" == "active" ]]
[[ "$(systemctl is-active ssh.service 2>/dev/null || true)" == "active" ]]

if ((FIRMWARE_CHANGED)); then
    printf '%s\n' 'Phase 4 configuration is installed; reboot is required for firmware changes.'
    if ((REBOOT)); then
        printf '%s\n' 'Rebooting now.'
        systemctl reboot
    else
        printf '%s\n' 'Rerun this script with --reboot to activate the firmware configuration.'
    fi
else
    printf '%s\n' 'Phase 4 setup complete; persistent state matches the plan.'
fi