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

# Unused services found by the 2026-09-21 audit (docs/pi-services-audit.md).
SERVICE_USER="liam"
PLAIN_UNITS=(man-db.timer udisks2.service)
APT_UNITS=(apt-daily.timer apt-daily-upgrade.timer)
E2SCRUB_UNITS=(e2scrub_all.timer e2scrub_reap.service)
GLOBAL_USER_UNITS=(mpris-proxy.service)
CRON_SPOOL="/var/spool/cron/crontabs"
CLOUD_INIT_MARKER="/etc/cloud/cloud-init.disabled"
AUTOLOGIN_DIR="/etc/systemd/system/getty@tty1.service.d"

usage() {
    cat <<'EOF'
Usage: setup-phase-04-unused-hardware.sh [--dry-run] [--reboot] [--help]

Idempotently disables unused Bluetooth and onboard audio hardware and enables
HDMI blanking while preserving SSH, networking, Avahi, camera, and KMS settings.

It also disables services this station does not use (docs/pi-services-audit.md):
  - man-db.timer, udisks2.service, and the user unit mpris-proxy.service
    (globally);
  - apt-daily.timer and apt-daily-upgrade.timer, unless APT::Periodic is set;
  - e2scrub_all.timer and e2scrub_reap.service, unless / is on LVM;
  - cron.service, only if every cron job defers to systemd and no crontab
    exists;
  - cloud-init (creates /etc/cloud/cloud-init.disabled), only after
    cloud-init reports status "done";
  - console autologin on tty1 (the drop-in is moved to the backup
    directory), only if liam has a usable password.
Guarded items that fail their check are skipped and reported.

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

# An existing managed block is re-rendered in place, so a later phase's
# block after it (Phase 5) does not make this file look out of date.
render_firmware_config() {
    awk '
        function print_block() {
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
        BEGIN { managed = 0; kept_count = 0; block_at = 0 }
        /^# BEGIN Project Optic Phase 4$/ {
            managed = 1
            if (!block_at) { kept[++kept_count] = ""; block_at = kept_count }
            next
        }
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
            if (!block_at)
                while (kept_count > 0 && kept[kept_count] ~ /^[[:space:]]*$/) kept_count--
            for (line_number = 1; line_number <= kept_count; line_number++) {
                if (line_number == block_at) print_block()
                else print kept[line_number]
            }
            if (!block_at) {
                print ""
                print_block()
            }
        }
    ' "$FIRMWARE_FILE"
}

firmware_matches() {
    [[ -r "$FIRMWARE_FILE" ]] && cmp -s "$FIRMWARE_FILE" <(render_firmware_config)
}

unit_loaded() {
    local load
    load=$(systemctl show "$1" -p LoadState --value 2>/dev/null || true)
    [[ -n "$load" && "$load" != "not-found" ]]
}

# Absent, or stopped and disabled/masked.
unit_off() {
    local active enabled
    unit_loaded "$1" || return 0
    active=$(systemctl is-active "$1" 2>/dev/null || true)
    enabled=$(systemctl is-enabled "$1" 2>/dev/null || true)
    [[ "$active" != "active" && ("$enabled" == "disabled" || "$enabled" == "masked") ]]
}

bluetooth_matches() {
    unit_off bluetooth.service
}

global_user_unit_off() {
    local state
    state=$(systemctl --global is-enabled "$1" 2>/dev/null || true)
    [[ -z "$state" || "$state" == "disabled" || "$state" == "masked" || "$state" == "not-found" ]]
}

apt_periodic_configured() {
    apt-config dump 2>/dev/null | grep -q '^APT::Periodic::'
}

root_on_lvm() {
    local source
    source=$(findmnt -n -o SOURCE / 2>/dev/null || true)
    [[ "$source" == /dev/mapper/* || "$(lsblk -no TYPE "$source" 2>/dev/null)" == "lvm" ]]
}

# Prints the cron jobs that would stop running without cron.service, or
# "unreadable" when the crontab spool needs root. Jobs that exit when
# systemd is running (they test /run/systemd/system) do not count.
cron_jobs_needing_cron() {
    local file jobs=()
    if [[ -d "$CRON_SPOOL" ]]; then
        if [[ ! -r "$CRON_SPOOL" || ! -x "$CRON_SPOOL" ]]; then
            printf '%s\n' unreadable
            return 0
        fi
        for file in "$CRON_SPOOL"/*; do
            [[ -f "$file" ]] && jobs+=("crontab:$(basename "$file")")
        done
    fi
    for file in /etc/cron.d/* /etc/cron.hourly/* /etc/cron.daily/* \
        /etc/cron.weekly/* /etc/cron.monthly/*; do
        [[ -f "$file" ]] || continue
        grep -q 'run/systemd/system' "$file" 2>/dev/null || jobs+=("$file")
    done
    if grep -Ev '^[[:space:]]*(#|$|[A-Za-z_]+=)' /etc/crontab 2>/dev/null |
        grep -v 'run-parts' | grep -q .; then
        jobs+=(/etc/crontab)
    fi
    ((${#jobs[@]} == 0)) || printf '%s\n' "${jobs[*]}"
}

cloud_init_off() {
    ! command -v cloud-init >/dev/null 2>&1 || [[ -e "$CLOUD_INIT_MARKER" ]]
}

cloud_init_status() {
    cloud-init status 2>/dev/null | awk -F': *' '/^status:/ { print $2; exit }'
}

autologin_dropins() {
    grep -l -e '--autologin' "$AUTOLOGIN_DIR"/*.conf 2>/dev/null || true
}

# passwd -S state of the service user: P (usable password), L (locked),
# NP (none), or empty when it cannot be read.
service_user_password_state() {
    passwd -S "$SERVICE_USER" 2>/dev/null | awk '{ print $2 }'
}

if ((DRY_RUN)); then
    printf '%s\n' 'Project Optic Phase 4 dry run'
    if bluetooth_matches; then
        printf '%s\n' '[OK] bluetooth.service is absent or disabled and inactive.'
    else
        printf '%s\n' '[CHANGE] Disable and stop bluetooth.service.'
    fi
    plan_units() {
        local unit
        for unit in "$@"; do
            if unit_off "$unit"; then
                printf '[OK] %s is absent or disabled and inactive.\n' "$unit"
            else
                printf '[CHANGE] Disable and stop %s.\n' "$unit"
            fi
        done
    }
    plan_units "${PLAIN_UNITS[@]}"
    if apt_periodic_configured; then
        printf '[SKIP] APT::Periodic is configured; keeping %s.\n' "${APT_UNITS[*]}"
    else
        plan_units "${APT_UNITS[@]}"
    fi
    if root_on_lvm; then
        printf '[SKIP] / is on LVM; keeping %s.\n' "${E2SCRUB_UNITS[*]}"
    else
        plan_units "${E2SCRUB_UNITS[@]}"
    fi
    for unit in "${GLOBAL_USER_UNITS[@]}"; do
        if global_user_unit_off "$unit"; then
            printf '[OK] User unit %s is not enabled globally.\n' "$unit"
        else
            printf '[CHANGE] Disable user unit %s globally and stop it for %s.\n' "$unit" "$SERVICE_USER"
        fi
    done
    if unit_off cron.service; then
        printf '%s\n' '[OK] cron.service is absent or disabled and inactive.'
    else
        cron_jobs=$(cron_jobs_needing_cron)
        if [[ "$cron_jobs" == "unreadable" ]]; then
            printf '%s %s\n' '[CHECK] cron.service: the crontab spool is readable only by root;' \
                'the real run disables cron only if no job needs it.'
        elif [[ -n "$cron_jobs" ]]; then
            printf '[SKIP] Keeping cron.service; these jobs need it: %s\n' "$cron_jobs"
        else
            printf '%s\n' '[CHANGE] Disable and stop cron.service (every job defers to systemd).'
        fi
    fi
    if cloud_init_off; then
        printf '%s\n' '[OK] cloud-init is absent or disabled.'
    elif [[ "$(cloud_init_status)" == "done" ]]; then
        printf '[CHANGE] Disable cloud-init: create %s.\n' "$CLOUD_INIT_MARKER"
    else
        printf '[SKIP] cloud-init has not finished (status=%s); keeping it.\n' "$(cloud_init_status)"
    fi
    dropins=$(autologin_dropins)
    if [[ -z "$dropins" ]]; then
        printf '%s\n' '[OK] No console autologin on tty1.'
    else
        password_state=$(service_user_password_state)
        if [[ "$password_state" == "P" ]]; then
            printf '[CHANGE] Remove tty1 autologin: move %s to %s.\n' "$(paste -sd ' ' - <<< "$dropins")" "$BACKUP_DIR"
        elif [[ -z "$password_state" ]]; then
            printf '%s %s\n' '[CHECK] tty1 autologin: the password state is readable only by root;' \
                "the real run removes autologin only if $SERVICE_USER has a usable password."
        else
            printf '[SKIP] Keeping tty1 autologin: %s has no usable password (state=%s).\n' \
                "$SERVICE_USER" "$password_state"
        fi
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

SKIPPED=()
disable_units() {
    local unit
    for unit in "$@"; do
        if unit_off "$unit"; then
            printf '%s is absent or already disabled.\n' "$unit"
        else
            systemctl disable --now "$unit"
            printf 'Disabled %s.\n' "$unit"
        fi
    done
}
EXPECTED_OFF=("${PLAIN_UNITS[@]}")
disable_units "${PLAIN_UNITS[@]}"
if apt_periodic_configured; then
    SKIPPED+=("${APT_UNITS[*]} (APT::Periodic is configured)")
else
    disable_units "${APT_UNITS[@]}"
    EXPECTED_OFF+=("${APT_UNITS[@]}")
fi
if root_on_lvm; then
    SKIPPED+=("${E2SCRUB_UNITS[*]} (/ is on LVM)")
else
    disable_units "${E2SCRUB_UNITS[@]}"
    EXPECTED_OFF+=("${E2SCRUB_UNITS[@]}")
fi

service_uid=$(id -u "$SERVICE_USER")
for unit in "${GLOBAL_USER_UNITS[@]}"; do
    if global_user_unit_off "$unit"; then
        printf 'User unit %s is not enabled globally.\n' "$unit"
    else
        systemctl --global disable "$unit"
        printf 'Disabled user unit %s globally.\n' "$unit"
    fi
    if [[ -d "/run/user/$service_uid" ]]; then
        sudo -u "$SERVICE_USER" XDG_RUNTIME_DIR="/run/user/$service_uid" \
            systemctl --user stop "$unit" 2>/dev/null || true
    fi
done

if unit_off cron.service; then
    printf '%s\n' 'cron.service is absent or already disabled.'
else
    cron_jobs=$(cron_jobs_needing_cron)
    if [[ -n "$cron_jobs" ]]; then
        SKIPPED+=("cron.service (jobs need it: $cron_jobs)")
    else
        systemctl disable --now cron.service
        EXPECTED_OFF+=(cron.service)
        printf '%s\n' 'Disabled cron.service; every cron job defers to systemd.'
    fi
fi

if cloud_init_off; then
    printf '%s\n' 'cloud-init is absent or already disabled.'
elif [[ "$(cloud_init_status)" == "done" ]]; then
    # Takes effect at the next boot; the Wi-Fi profile NetworkManager
    # stores under /etc/netplan is independent of cloud-init.
    install -m 0644 /dev/null "$CLOUD_INIT_MARKER"
    printf 'Disabled cloud-init from the next boot (%s).\n' "$CLOUD_INIT_MARKER"
else
    SKIPPED+=("cloud-init (status=$(cloud_init_status), not done)")
fi

dropins=$(autologin_dropins)
if [[ -z "$dropins" ]]; then
    printf '%s\n' 'No console autologin on tty1.'
elif [[ "$(service_user_password_state)" != "P" ]]; then
    SKIPPED+=("tty1 autologin ($SERVICE_USER has no usable password; set one with passwd, then rerun)")
else
    install -d -m 0700 "$BACKUP_DIR"
    while IFS= read -r dropin; do
        backup="$BACKUP_DIR/getty-tty1-$(basename "$dropin").phase-04.$(date +%Y%m%dT%H%M%S).bak"
        mv -- "$dropin" "$backup"
        printf 'Removed tty1 autologin; moved %s to %s\n' "$dropin" "$backup"
    done <<< "$dropins"
    rmdir --ignore-fail-on-non-empty "$AUTOLOGIN_DIR"
    systemctl daemon-reload
    # Restarting the getty ends the autologin session, so skip it when this
    # script itself runs on tty1; the change then applies at the next boot.
    if [[ "$(tty 2>/dev/null || true)" == "/dev/tty1" ]]; then
        printf '%s\n' 'Running on tty1: the login prompt returns at the next boot.'
    else
        systemctl restart getty@tty1.service
    fi
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
for unit in "${EXPECTED_OFF[@]}"; do
    unit_off "$unit"
done
for unit in "${GLOBAL_USER_UNITS[@]}"; do
    global_user_unit_off "$unit"
done
[[ "$(systemctl is-active avahi-daemon.service 2>/dev/null || true)" == "active" ]]
[[ "$(systemctl is-active ssh.service 2>/dev/null || true)" == "active" ]]

for item in "${SKIPPED[@]}"; do
    printf 'WARNING: skipped %s.\n' "$item" >&2
done

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