#!/usr/bin/env bash

# Configure Phase 5 of the Project Optic hardening plan.

set -euo pipefail

export LC_ALL=C
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

DRY_RUN=0
REBOOT=0
BACKUP_DIR="/var/backups/optic-hardening"
FIRMWARE_FILE="/boot/firmware/config.txt"
TEMPORARY=""
TEST_TEMPORARY=""

FAN_SETTINGS=(
    fan_temp0=45000 fan_temp0_hyst=5000 fan_temp0_speed=75
    fan_temp1=55000 fan_temp1_hyst=5000 fan_temp1_speed=125
    fan_temp2=65000 fan_temp2_hyst=5000 fan_temp2_speed=175
    fan_temp3=70000 fan_temp3_hyst=5000 fan_temp3_speed=250
)

usage() {
    cat <<'EOF'
Usage: setup-phase-05-active-cooler.sh [--dry-run] [--reboot] [--help]

Idempotently configures the official Raspberry Pi 5 Active Cooler policy.

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
    [[ -z "$TEST_TEMPORARY" ]] || rm -f -- "$TEST_TEMPORARY"
}
trap cleanup EXIT

render_firmware_config() {
    local source=${1:-$FIRMWARE_FILE}
    awk '
        BEGIN { managed = 0; kept_count = 0 }
        /^# BEGIN Project Optic Phase 5$/ { managed = 1; next }
        /^# END Project Optic Phase 5$/ { managed = 0; next }
        managed { next }
        {
            normalized = $0
            sub(/#.*/, "", normalized)
            gsub(/[[:space:]]/, "", normalized)
            if (normalized ~ /^dtparam=fan_temp[0-3](|_hyst|_speed)=/) next
            kept[++kept_count] = $0
        }
        END {
            while (kept_count > 0 && kept[kept_count] ~ /^[[:space:]]*$/) kept_count--
            for (line_number = 1; line_number <= kept_count; line_number++) print kept[line_number]
            print ""
            print "# BEGIN Project Optic Phase 5"
            print "[all]"
            print "# Raspberry Pi 5 Active Cooler profile: threshold, hysteresis, PWM."
            print "dtparam=fan_temp0=45000"
            print "dtparam=fan_temp0_hyst=5000"
            print "dtparam=fan_temp0_speed=75"
            print "dtparam=fan_temp1=55000"
            print "dtparam=fan_temp1_hyst=5000"
            print "dtparam=fan_temp1_speed=125"
            print "dtparam=fan_temp2=65000"
            print "dtparam=fan_temp2_hyst=5000"
            print "dtparam=fan_temp2_speed=175"
            print "dtparam=fan_temp3=70000"
            print "dtparam=fan_temp3_hyst=5000"
            print "dtparam=fan_temp3_speed=250"
            print "# END Project Optic Phase 5"
        }
    ' "$source"
}

validate_candidate() {
    local candidate=$1 setting expected count preserved
    for setting in "${FAN_SETTINGS[@]}"; do
        expected="dtparam=$setting"
        count=$(awk -v expected="$expected" '
            {
                line = $0
                sub(/#.*/, "", line)
                gsub(/[[:space:]]/, "", line)
                if (line == expected) count++
            }
            END { print count + 0 }
        ' "$candidate")
        [[ "$count" == "1" ]] || {
            printf 'Candidate firmware configuration has %s copies of %s.\n' "$count" "$expected" >&2
            return 1
        }
    done

    for preserved in camera_auto_detect=1 dtoverlay=vc4-kms-v3d \
        dtoverlay=disable-bt dtparam=audio=off hdmi_blanking=2; do
        grep -Eq "^[[:space:]]*$preserved([[:space:]]*(#.*)?)?$" "$candidate"
    done
}

firmware_matches() {
    [[ -r "$FIRMWARE_FILE" ]] && cmp -s "$FIRMWARE_FILE" <(render_firmware_config)
}

active_cooler_available() {
    local device
    for device in /sys/class/thermal/cooling_device*; do
        [[ -r "$device/type" ]] || continue
        [[ "$(cat "$device/type")" == "pwm-fan" &&
           "$(cat "$device/max_state" 2>/dev/null || true)" == "4" ]] && return 0
    done
    return 1
}

conflicting_overlay() {
    awk '
        {
            line = $0
            sub(/#.*/, "", line)
            gsub(/[[:space:]]/, "", line)
            if (line ~ /^dtoverlay=(gpio-fan|pwm-fan)(,|$)/) { found = 1; exit }
        }
        END { exit !found }
    ' "$FIRMWARE_FILE"
}

if ((DRY_RUN)); then
    printf '%s\n' 'Project Optic Phase 5 dry run'
    if active_cooler_available; then
        printf '%s\n' '[OK] Official Active Cooler pwm-fan device is available.'
    else
        printf '%s\n' '[BLOCKED] Official Active Cooler pwm-fan device is unavailable.'
    fi
    if conflicting_overlay; then
        printf '%s\n' '[BLOCKED] A conflicting gpio-fan or pwm-fan overlay is configured.'
    else
        printf '%s\n' '[OK] No conflicting fan overlay is configured.'
    fi
    if firmware_matches; then
        printf '[OK] %s already matches the fan policy.\n' "$FIRMWARE_FILE"
    else
        TEST_TEMPORARY=$(mktemp)
        render_firmware_config > "$TEST_TEMPORARY"
        validate_candidate "$TEST_TEMPORARY"
        render_firmware_config "$TEST_TEMPORARY" | cmp -s "$TEST_TEMPORARY" -
        printf '%s\n' '[OK] Candidate firmware configuration is valid and idempotent.'
        printf '[CHANGE] Back up and update the fan policy in %s.\n' "$FIRMWARE_FILE"
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
if ! active_cooler_available; then
    printf '%s\n' 'Official Active Cooler pwm-fan device is unavailable; no changes were made.' >&2
    exit 69
fi
if conflicting_overlay; then
    printf '%s\n' 'A conflicting gpio-fan or pwm-fan overlay is configured; no changes were made.' >&2
    exit 69
fi

printf '%s\n' 'Configuring Project Optic Phase 5: Active Cooler policy'

FIRMWARE_CHANGED=0
if firmware_matches; then
    printf '%s already matches the fan policy.\n' "$FIRMWARE_FILE"
else
    install -d -m 0700 "$BACKUP_DIR"
    backup="$BACKUP_DIR/config.txt.phase-05.$(date +%Y%m%dT%H%M%S).bak"
    cp -a -- "$FIRMWARE_FILE" "$backup"
    printf 'Backed up firmware configuration to %s\n' "$backup"

    TEMPORARY=$(mktemp "$(dirname "$FIRMWARE_FILE")/.config.txt.optic-phase-05.XXXXXX")
    render_firmware_config > "$TEMPORARY"
    validate_candidate "$TEMPORARY"
    render_firmware_config "$TEMPORARY" | cmp -s "$TEMPORARY" -

    chmod --reference="$FIRMWARE_FILE" "$TEMPORARY"
    chown --reference="$FIRMWARE_FILE" "$TEMPORARY"
    mv -f -- "$TEMPORARY" "$FIRMWARE_FILE"
    TEMPORARY=""
    FIRMWARE_CHANGED=1
    printf 'Updated %s.\n' "$FIRMWARE_FILE"
fi

if ((FIRMWARE_CHANGED)); then
    printf '%s\n' 'Phase 5 configuration is installed; reboot is required for fan policy changes.'
    if ((REBOOT)); then
        printf '%s\n' 'Rebooting now.'
        systemctl reboot
    else
        printf '%s\n' 'Rerun this script with --reboot to activate the fan policy.'
    fi
else
    printf '%s\n' 'Phase 5 setup complete; persistent state matches the plan.'
fi