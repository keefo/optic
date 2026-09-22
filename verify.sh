#!/usr/bin/env bash

# Read-only verification for the Project Optic hardening plan.

set -uo pipefail

export LC_ALL=C
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

EXPECTED_BESZEL_VERSION="0.19.0"
# Must stay an IP, never a hostname — confirmed directly (2026-09-20):
# beszel-agent is a statically linked Go binary whose resolver only issues
# plain unicast DNS queries to the router, never mDNS multicast, so
# "imacpro.local" fails outright ("no such host") regardless of network
# state. This is not a transient/fixable-later issue; it structurally
# cannot resolve for this binary. See
# worklogs/2026-09-20-stale-iMac-ip-beszel-and-sync.md. `EXPECTED_CAPTURE_HOST`
# below is different and correctly a hostname: it feeds `optic_sync`'s SSH
# connection, which shells out to the real `ssh` binary and resolves
# `.local` names fine via the Pi's normal system resolver.
EXPECTED_HUB_URL="http://192.168.0.202:8090"
EXPECTED_CAPTURE_HOST="imacpro.local"
EXPECTED_CAPTURE_USER="admin"
EXPECTED_CAPTURE_PORT="2222"
EXPECTED_CAPTURE_SIZE_BYTES=268435456
STRICT=0
COLOR_MODE="auto"
SELECTED_PHASE="all"
SECTION_ENABLED=1

usage() {
    cat <<'EOF'
Usage: verify.sh [--phase PHASE] [--strict] [--color|--no-color] [--help]

Runs read-only checks for the Project Optic hardening plan.

  --phase      Report only: 1-9, baseline, monitoring, or all (default).
  --strict     Return exit code 2 when warnings exist and no checks fail.
  --color      Always use ANSI colors (useful when streaming through SSH).
  --no-color   Disable ANSI colors.
  --help       Display this help.

Exit codes: 0 = checks passed, 1 = failed checks, 2 = warnings in strict mode,
64 = invalid arguments.
EOF
}

while (($#)); do
    case "$1" in
        --phase)
            [[ $# -ge 2 ]] || { printf '%s\n' 'Missing value for --phase' >&2; exit 64; }
            SELECTED_PHASE=$2
            shift
            ;;
        --phase=*) SELECTED_PHASE=${1#*=} ;;
        --strict) STRICT=1 ;;
        --color) COLOR_MODE="always" ;;
        --no-color) COLOR_MODE="never" ;;
        --help|-h) usage; exit 0 ;;
        *) printf 'Unknown argument: %s\n' "$1" >&2; usage >&2; exit 64 ;;
    esac
    shift
done

case "$SELECTED_PHASE" in
    all|baseline|monitoring|1|2|3|4|5|6|7|8|9) ;;
    *)
        printf 'Invalid phase: %s\n' "$SELECTED_PHASE" >&2
        usage >&2
        exit 64
        ;;
esac

USE_COLOR=0
if [[ "$COLOR_MODE" == "always" ]]; then
    USE_COLOR=1
elif [[ "$COLOR_MODE" == "auto" && -t 1 && -z "${NO_COLOR:-}" ]]; then
    USE_COLOR=1
fi

if ((USE_COLOR)); then
    C_PASS=$'\033[32m'
    C_WARN=$'\033[33m'
    C_FAIL=$'\033[31m'
    C_INFO=$'\033[36m'
    C_BOLD=$'\033[1m'
    C_RESET=$'\033[0m'
else
    C_PASS=""
    C_WARN=""
    C_FAIL=""
    C_INFO=""
    C_BOLD=""
    C_RESET=""
fi

PASS_COUNT=0
WARN_COUNT=0
FAIL_COUNT=0
INFO_COUNT=0

report() {
    local level=$1 color=$2 message=$3 detail=${4:-}
    ((SECTION_ENABLED)) || return 0
    printf '%s[%s]%s %s' "$color" "$level" "$C_RESET" "$message"
    [[ -n "$detail" ]] && printf ' — %s' "$detail"
    printf '\n'
}

pass() { ((SECTION_ENABLED)) || return 0; ((PASS_COUNT += 1)); report PASS "$C_PASS" "$1" "${2:-}"; }
warn() { ((SECTION_ENABLED)) || return 0; ((WARN_COUNT += 1)); report WARN "$C_WARN" "$1" "${2:-}"; }
fail() { ((SECTION_ENABLED)) || return 0; ((FAIL_COUNT += 1)); report FAIL "$C_FAIL" "$1" "${2:-}"; }
info() { ((SECTION_ENABLED)) || return 0; ((INFO_COUNT += 1)); report INFO "$C_INFO" "$1" "${2:-}"; }

section() {
    local title=$1 phase
    case "$title" in
        "Prerequisites and hardware baseline") phase="baseline" ;;
        "Monitoring baseline: Beszel") phase="monitoring" ;;
        [1-9].*) phase=${title%%.*} ;;
        *) phase="unknown" ;;
    esac
    SECTION_ENABLED=0
    if [[ "$SELECTED_PHASE" == "all" || "$SELECTED_PHASE" == "$phase" ]]; then
        SECTION_ENABLED=1
        printf '\n%s== %s ==%s\n' "$C_BOLD" "$title" "$C_RESET"
    fi
}

assignment_value() {
    local key=$1
    awk -v wanted="$key" '
        /^[[:space:]]*#/ { next }
        {
            line = $0
            sub(/^[[:space:]]+/, "", line)
            position = index(line, "=")
            if (!position) next
            name = substr(line, 1, position - 1)
            gsub(/[[:space:]]/, "", name)
            if (name != wanted) next
            value = substr(line, position + 1)
            sub(/[[:space:]]+#.*$/, "", value)
            sub(/^[[:space:]]+/, "", value)
            sub(/[[:space:]]+$/, "", value)
            print value
        }
    ' | tail -n 1
}

file_value() {
    local file=$1 key=$2
    [[ -r "$file" ]] || return 0
    assignment_value "$key" < "$file"
}

systemd_config_value() {
    local config=$1 key=$2
    systemd-analyze cat-config "$config" 2>/dev/null | assignment_value "$key"
}

unquote() {
    local value=$1
    value=${value#\"}
    value=${value%\"}
    value=${value#\'}
    value=${value%\'}
    printf '%s' "$value"
}

check_runtime_and_file_value() {
    local label=$1 key=$2 expected=$3 file=$4
    local runtime configured
    runtime=$(sysctl -n "$key" 2>/dev/null || true)
    configured=$(file_value "$file" "$key")
    configured=$(unquote "$configured")
    if [[ "$runtime" == "$expected" && "$configured" == "$expected" ]]; then
        pass "$label" "$key=$expected (runtime and persistent)"
    else
        [[ -n "$configured" ]] || configured="missing"
        [[ -n "$runtime" ]] || runtime="unavailable"
        fail "$label" "expected=$expected, runtime=$runtime, config=$configured"
    fi
}

unit_disabled() {
    local unit=$1
    local load active enabled
    load=$(systemctl show "$unit" -p LoadState --value 2>/dev/null || true)
    active=$(systemctl is-active "$unit" 2>/dev/null || true)
    enabled=$(systemctl is-enabled "$unit" 2>/dev/null || true)
    if [[ -z "$load" || "$load" == "not-found" ]]; then
        pass "$unit is absent"
    elif [[ "$active" == "inactive" || "$active" == "failed" ]] &&
         [[ "$enabled" == "disabled" || "$enabled" == "masked" ||
            "$enabled" == "static" || "$enabled" == "indirect" ]]; then
        pass "$unit is not running" "active=$active, enabled=$enabled"
    else
        fail "$unit must be stopped and disabled" "active=${active:-unknown}, enabled=${enabled:-unknown}"
    fi
}

global_user_unit_masked() {
    local unit=$1 state
    state=$(systemctl --global is-enabled "$unit" 2>/dev/null || true)
    if [[ "$state" == "masked" || "$state" == "masked-runtime" ]]; then
        pass "$unit is globally masked"
    elif [[ -z "$state" || "$state" == "not-found" ]]; then
        pass "$unit is absent"
    else
        fail "$unit must be globally masked or absent" "state=$state"
    fi
}

firmware_line_count() {
    local expected=$1
    awk -v expected="$expected" '
        {
            line = $0
            sub(/#.*/, "", line)
            gsub(/[[:space:]]/, "", line)
            if (line == expected) count++
        }
        END { print count + 0 }
    ' /boot/firmware/config.txt 2>/dev/null
}

check_firmware_line() {
    local label=$1 expected=$2 count
    count=$(firmware_line_count "$expected")
    if [[ "$count" == "1" ]]; then
        pass "$label" "$expected"
    else
        fail "$label" "expected exactly one active '$expected' entry; found $count"
    fi
}

option_present() {
    local options=$1 option=$2
    [[ ",$options," == *",$option,"* ]]
}

environment_value() {
    local environment=$1 key=$2
    printf '%s\n' "$environment" | tr ' ' '\n' |
        awk -F= -v wanted="$key" '$1 == wanted {sub(/^[^=]*=/, ""); print; exit}'
}

printf '%sProject Optic hardening verification%s\n' "$C_BOLD" "$C_RESET"
printf 'Host: %s | Time: %s | Mode: read-only | Scope: %s\n' \
    "$(hostname 2>/dev/null || printf unknown)" \
    "$(date --iso-8601=seconds 2>/dev/null || date)" \
    "$SELECTED_PHASE"

section "Prerequisites and hardware baseline"

for command in awk cat curl find findmnt grep rpicam-still sed sort stat \
    systemctl systemd-analyze sysctl timedatectl timeout tr vcgencmd; do
    if command -v "$command" >/dev/null 2>&1; then
        pass "Command available: $command"
    else
        fail "Required command is missing: $command"
    fi
done

model=$(tr -d '\0' < /proc/device-tree/model 2>/dev/null || true)
if [[ "$model" == *"Raspberry Pi 5"* ]]; then
    pass "Hardware model" "$model"
else
    fail "Hardware model is not Raspberry Pi 5" "detected=${model:-unknown}"
fi

architecture=$(uname -m 2>/dev/null || true)
if [[ "$architecture" == "aarch64" ]]; then
    pass "64-bit ARM operating system" "$architecture"
else
    fail "Expected aarch64 operating system" "detected=${architecture:-unknown}"
fi

memory_kib=$(awk '/^MemTotal:/ {print $2}' /proc/meminfo 2>/dev/null)
if [[ "$memory_kib" =~ ^[0-9]+$ ]] && ((memory_kib >= 850000 && memory_kib <= 1200000)); then
    pass "Physical memory matches the 1 GB target" "MemTotal=${memory_kib} KiB"
else
    fail "Physical memory is outside the expected 1 GB range" "MemTotal=${memory_kib:-unknown} KiB"
fi

throttled=$(vcgencmd get_throttled 2>/dev/null || true)
if [[ "$throttled" == "throttled=0x0" ]]; then
    pass "No current or historical power/thermal throttling" "$throttled"
elif [[ -n "$throttled" ]]; then
    fail "Firmware reports a power or thermal event" "$throttled"
else
    fail "Unable to read firmware throttling status"
fi

failed_units=$(systemctl --failed --no-legend --plain 2>/dev/null |
    awk 'NF {count++} END {print count + 0}')
if [[ "$failed_units" == "0" ]]; then
    pass "No failed system services"
else
    fail "Systemd has failed services" "count=$failed_units"
fi

section "Monitoring baseline: Beszel"

agent_active=$(systemctl --user is-active beszel-agent.service 2>/dev/null || true)
agent_enabled=$(systemctl --user is-enabled beszel-agent.service 2>/dev/null || true)
if [[ "$agent_active" == "active" && "$agent_enabled" == "enabled" ]]; then
    pass "Beszel Agent service is active and enabled"
else
    fail "Beszel Agent service is not persistent and healthy" \
        "active=${agent_active:-unknown}, enabled=${agent_enabled:-unknown}"
fi

linger=$(loginctl show-user "$(id -un)" -p Linger --value 2>/dev/null || true)
if [[ "$linger" == "yes" ]]; then
    pass "User lingering keeps the Agent running after logout"
else
    fail "User lingering is disabled" "Linger=${linger:-unknown}"
fi

agent_environment=$(systemctl --user show beszel-agent.service -p Environment --value 2>/dev/null || true)
hub_url=$(environment_value "$agent_environment" HUB_URL)
listen_address=$(environment_value "$agent_environment" LISTEN)
primary_sensor=$(environment_value "$agent_environment" PRIMARY_SENSOR)
extra_filesystems=$(environment_value "$agent_environment" EXTRA_FILESYSTEMS)

if [[ "$hub_url" == "$EXPECTED_HUB_URL" ]]; then
    pass "Beszel Hub URL" "$hub_url"
else
    fail "Beszel Hub URL differs from the deployment plan" \
        "expected=$EXPECTED_HUB_URL, actual=${hub_url:-missing}"
fi

if [[ "$listen_address" == 127.0.0.1:* ]]; then
    pass "Beszel fallback SSH listener is loopback-only" "$listen_address"
else
    fail "Beszel fallback SSH listener is not loopback-only" "LISTEN=${listen_address:-missing}"
fi

if [[ "$primary_sensor" == "cpu_thermal" ]]; then
    pass "Beszel primary temperature sensor" "$primary_sensor"
else
    fail "Beszel primary temperature sensor differs from the plan" \
        "PRIMARY_SENSOR=${primary_sensor:-missing}"
fi

agent_health=$("$HOME/.local/bin/beszel-agent" health 2>/dev/null || true)
if [[ "$agent_health" == "ok" ]]; then
    pass "Beszel Agent health check"
else
    fail "Beszel Agent health check failed" "result=${agent_health:-unavailable}"
fi

agent_version=$("$HOME/.local/bin/beszel-agent" --version 2>/dev/null | awk '{print $NF}' || true)
if [[ "$agent_version" == "$EXPECTED_BESZEL_VERSION" ]]; then
    pass "Beszel Agent version" "$agent_version"
else
    fail "Beszel Agent version differs from the plan" \
        "expected=$EXPECTED_BESZEL_VERSION, actual=${agent_version:-unavailable}"
fi

if [[ -n "$hub_url" ]] && curl -fsS --max-time 5 "$hub_url/api/health" 2>/dev/null |
    grep -q '"code":200'; then
    pass "Beszel Hub is reachable from optic" "$hub_url"
else
    fail "Beszel Hub is not reachable from optic" "HUB_URL=${hub_url:-missing}"
fi

for secret_file in "$HOME/.config/beszel/key" "$HOME/.config/beszel/token" \
    "$HOME/.config/systemd/user/beszel-agent.service"; do
    if [[ -f "$secret_file" ]]; then
        mode=$(stat -c '%a' "$secret_file" 2>/dev/null || true)
        if [[ "$mode" == "600" ]]; then
            pass "Protected file permissions" "$secret_file mode=600"
        else
            fail "Protected file permissions are too broad" "$secret_file mode=${mode:-unknown}"
        fi
    else
        fail "Required protected file is missing" "$secret_file"
    fi
done

section "1. Flash wear and RAM journaling"

# Persistent but small: keeps crash evidence across reboots at bounded SD
# wear (scripts/setup-phase-01-journaling.sh,
# worklogs/2026-09-19-persistent-journal-crash-evidence.md).
journal_storage=$(systemd_config_value systemd/journald.conf Storage)
if [[ "$journal_storage" == "persistent" ]]; then
    pass "Journal storage is persistent"
else
    fail "Journal storage must be persistent" "Storage=${journal_storage:-default}"
fi

for journal_limit_key in SystemMaxUse RuntimeMaxUse; do
    journal_limit=$(systemd_config_value systemd/journald.conf "$journal_limit_key")
    if [[ "$journal_limit" == "16M" ]]; then
        pass "Journal size is constrained" "$journal_limit_key=16M"
    else
        fail "Journal size must be constrained" \
            "expected $journal_limit_key=16M, actual=${journal_limit:-unset}"
    fi
done

if [[ "$(systemctl is-active systemd-journald.service 2>/dev/null || true)" == "active" ]]; then
    pass "systemd-journald is active"
else
    fail "systemd-journald is not active"
fi

machine_id=$(cat /etc/machine-id 2>/dev/null || true)
if [[ -n "$machine_id" && -e "/var/log/journal/$machine_id/system.journal" ]]; then
    pass "Persistent system journal is being written" "/var/log/journal/$machine_id"
else
    fail "Persistent system journal is missing" "/var/log/journal/${machine_id:-unknown}/system.journal"
fi

unit_disabled rsyslog.service
journal_usage=$(journalctl --disk-usage 2>/dev/null | tr '\n' ' ' || true)
[[ -n "$journal_usage" ]] && info "Current journal usage" "$journal_usage"

section "2. zram and virtual memory"

swap_count=$(awk 'NR > 1 {count++} END {print count + 0}' /proc/swaps 2>/dev/null)
non_zram_swap=$(awk 'NR > 1 && $1 !~ /^\/dev\/zram[0-9]+$/ {print $1}' /proc/swaps 2>/dev/null |
    paste -sd, -)
if [[ "$swap_count" =~ ^[0-9]+$ ]] && ((swap_count > 0)) && [[ -z "$non_zram_swap" ]]; then
    pass "All active swap devices use zram" "count=$swap_count"
elif [[ -n "$non_zram_swap" ]]; then
    fail "On-disk or unexpected swap is active" "$non_zram_swap"
else
    fail "No zram swap device is active"
fi

if [[ -r /sys/block/zram0/comp_algorithm ]]; then
    zram_algorithm=$(sed -n 's/.*\[\([^]]*\)\].*/\1/p' /sys/block/zram0/comp_algorithm)
    if [[ "$zram_algorithm" == "zstd" ]]; then
        pass "zram compression algorithm" "zstd"
    else
        fail "zram compression algorithm must be zstd" "active=${zram_algorithm:-unknown}"
    fi
else
    fail "zram compression status is unavailable"
fi

if [[ -r /sys/block/zram0/disksize && "$memory_kib" =~ ^[0-9]+$ ]]; then
    zram_bytes=$(cat /sys/block/zram0/disksize)
    zram_percent=$((zram_bytes / 1024 * 100 / memory_kib))
    if ((zram_percent >= 48 && zram_percent <= 52)); then
        pass "zram capacity is 50% of physical memory" "calculated=${zram_percent}%"
    else
        fail "zram capacity differs from the 50% plan" "calculated=${zram_percent}%"
    fi
else
    fail "Unable to determine zram capacity"
fi

zram_priorities=$(awk 'NR > 1 && $1 ~ /^\/dev\/zram[0-9]+$/ {print $5}' /proc/swaps 2>/dev/null |
    sort -u | paste -sd, -)
if [[ "$zram_priorities" == "100" ]]; then
    pass "zram swap priority" "100"
else
    fail "zram swap priority differs from the plan" "expected=100, actual=${zram_priorities:-missing}"
fi

zram_backing_device=$(cat /sys/block/zram0/backing_dev 2>/dev/null || true)
if [[ "$zram_backing_device" == "none" ]]; then
    pass "zram has no disk writeback device"
else
    fail "zram must not use disk writeback" "backing_device=${zram_backing_device:-unavailable}"
fi

rpi_swap_status=$(dpkg-query -W -f='${db:Status-Status}' rpi-swap 2>/dev/null || true)
if [[ "$rpi_swap_status" == "installed" ]]; then
    pass "Native Raspberry Pi swap manager is installed" "rpi-swap"
else
    fail "Native Raspberry Pi swap manager is unavailable" "status=${rpi_swap_status:-missing}"
fi

rpi_swap_config=/etc/rpi/swap.conf.d/90-optic.conf
if [[ -r "$rpi_swap_config" ]]; then
    for specification in "Mechanism:zram" "RamMultiplier:0.5"; do
        key=${specification%%:*}
        expected=${specification#*:}
        actual=$(unquote "$(file_value "$rpi_swap_config" "$key")")
        if [[ "$actual" == "$expected" ]]; then
            pass "Persistent rpi-swap setting" "$key=$expected"
        else
            fail "Persistent rpi-swap setting differs from the plan" \
                "$key expected=$expected, actual=${actual:-missing}"
        fi
    done
    expected_zram_mib=$((memory_kib / 1024 / 2))
    fixed_zram_mib=$(unquote "$(file_value "$rpi_swap_config" FixedSizeMiB)")
    if [[ "$fixed_zram_mib" == "$expected_zram_mib" ]]; then
        pass "Persistent zram size workaround" "FixedSizeMiB=$expected_zram_mib"
    else
        fail "Persistent zram size workaround differs from the plan" \
            "expected=$expected_zram_mib, actual=${fixed_zram_mib:-missing}"
    fi
else
    fail "Project Optic rpi-swap configuration is missing" "$rpi_swap_config"
fi

zram_generator_config=/etc/systemd/zram-generator.conf.d/90-optic.conf
if [[ -r "$zram_generator_config" ]]; then
    for specification in "compression-algorithm:zstd" "swap-priority:100"; do
        key=${specification%%:*}
        expected=${specification#*:}
        actual=$(unquote "$(file_value "$zram_generator_config" "$key")")
        if [[ "$actual" == "$expected" ]]; then
            pass "Persistent zram-generator setting" "$key=$expected"
        else
            fail "Persistent zram-generator setting differs from the plan" \
                "$key expected=$expected, actual=${actual:-missing}"
        fi
    done
else
    fail "Project Optic zram-generator configuration is missing" "$zram_generator_config"
fi

zram_setup_active=$(systemctl is-active systemd-zram-setup@zram0.service 2>/dev/null || true)
if [[ "$zram_setup_active" == "active" ]]; then
    pass "Native zram setup service is active"
else
    fail "Native zram setup service is not active" "active=${zram_setup_active:-unknown}"
fi

unit_disabled dphys-swapfile.service

for specification in \
    "Swappiness:vm.swappiness:10" \
    "Minimum free memory:vm.min_free_kbytes:65536" \
    "VFS cache pressure:vm.vfs_cache_pressure:50"; do
    label=${specification%%:*}
    remainder=${specification#*:}
    key=${remainder%%:*}
    expected=${remainder#*:}
    check_runtime_and_file_value "$label" "$key" "$expected" /etc/sysctl.d/99-optic-memory.conf
done

section "3. Watchdog and panic recovery"

check_runtime_and_file_value "Kernel panic reboot delay" kernel.panic 10 /etc/sysctl.d/99-optic-recovery.conf
check_runtime_and_file_value "Panic on kernel oops" kernel.panic_on_oops 1 /etc/sysctl.d/99-optic-recovery.conf

runtime_watchdog=$(systemctl show -p RuntimeWatchdogUSec --value 2>/dev/null || true)
if [[ "$runtime_watchdog" == "15s" ]]; then
    pass "Runtime hardware watchdog interval" "$runtime_watchdog"
else
    fail "Runtime hardware watchdog interval differs from the plan" \
        "expected=15s, actual=${runtime_watchdog:-unknown}"
fi

reboot_watchdog=$(systemctl show -p RebootWatchdogUSec --value 2>/dev/null || true)
if [[ "$reboot_watchdog" == "2min" ]]; then
    pass "Reboot watchdog interval" "$reboot_watchdog"
else
    fail "Reboot watchdog interval differs from the plan" \
        "expected=2min, actual=${reboot_watchdog:-unknown}"
fi

watchdog_config=$(systemd_config_value systemd/system.conf RuntimeWatchdogSec)
reboot_watchdog_config=$(systemd_config_value systemd/system.conf RebootWatchdogSec)
if [[ "$watchdog_config" == "15s" && "$reboot_watchdog_config" == "2min" ]]; then
    pass "Watchdog settings are persistent in systemd configuration"
else
    fail "Persistent systemd watchdog settings differ from the plan" \
        "RuntimeWatchdogSec=${watchdog_config:-unset}, RebootWatchdogSec=${reboot_watchdog_config:-unset}"
fi

if [[ -c /dev/watchdog0 && -r /sys/class/watchdog/watchdog0/identity ]]; then
    pass "Hardware watchdog device is available" "$(cat /sys/class/watchdog/watchdog0/identity)"
else
    fail "Hardware watchdog device is unavailable"
fi

section "4. Disabled services and firmware hardware"

for unit in bluetooth.service hciuart.service ModemManager.service cups.service; do
    unit_disabled "$unit"
done

for unit in pulseaudio.socket pulseaudio.service pipewire.socket pipewire.service; do
    global_user_unit_masked "$unit"
done

avahi_active=$(systemctl is-active avahi-daemon.service 2>/dev/null || true)
avahi_enabled=$(systemctl is-enabled avahi-daemon.service 2>/dev/null || true)
if [[ "$avahi_active" == "active" && "$avahi_enabled" != "disabled" && "$avahi_enabled" != "masked" ]]; then
    pass "Avahi remains available for optic.local" "active=$avahi_active, enabled=$avahi_enabled"
else
    fail "Avahi must remain available for optic.local" \
        "active=${avahi_active:-unknown}, enabled=${avahi_enabled:-unknown}"
fi

check_firmware_line "Bluetooth firmware controller is disabled" "dtoverlay=disable-bt"
check_firmware_line "Onboard audio firmware is disabled" "dtparam=audio=off"
check_firmware_line "HDMI blanking is configured" "hdmi_blanking=2"

section "5. Active Cooler policy"

for specification in \
    "fan_temp0:45000" "fan_temp0_hyst:5000" "fan_temp0_speed:75" \
    "fan_temp1:55000" "fan_temp1_hyst:5000" "fan_temp1_speed:125" \
    "fan_temp2:65000" "fan_temp2_hyst:5000" "fan_temp2_speed:175" \
    "fan_temp3:70000" "fan_temp3_hyst:5000" "fan_temp3_speed:250"; do
    key=${specification%%:*}
    value=${specification#*:}
    check_firmware_line "Active Cooler firmware setting: $key" "dtparam=$key=$value"
done

prohibited_fan_overlays=$(awk '
    {
        line = $0
        sub(/#.*/, "", line)
        gsub(/[[:space:]]/, "", line)
        if (line ~ /^dtoverlay=(gpio-fan|pwm-fan)(,|$)/) print line
    }
' /boot/firmware/config.txt 2>/dev/null | paste -sd, -)
if [[ -z "$prohibited_fan_overlays" ]]; then
    pass "No conflicting fan-control overlay is configured"
else
    fail "Conflicting fan-control overlay is configured" "$prohibited_fan_overlays"
fi

cooling_device=""
for candidate in /sys/class/thermal/cooling_device*; do
    [[ -r "$candidate/type" ]] || continue
    if [[ "$(cat "$candidate/type")" == "pwm-fan" ]]; then
        cooling_device=$candidate
        break
    fi
done

if [[ -n "$cooling_device" ]]; then
    max_state=$(cat "$cooling_device/max_state" 2>/dev/null || true)
    if [[ "$max_state" == "4" ]]; then
        pass "Active Cooler thermal device detected" "type=pwm-fan, max_state=4"
    else
        fail "Active Cooler has an unexpected maximum state" "max_state=${max_state:-unknown}"
    fi
else
    fail "Active Cooler pwm-fan thermal device was not detected"
fi

thermal_zone=""
for candidate in /sys/class/thermal/thermal_zone*; do
    [[ -r "$candidate/type" ]] || continue
    if [[ "$(cat "$candidate/type")" == *"cpu"* ]]; then
        thermal_zone=$candidate
        break
    fi
done
[[ -n "$thermal_zone" ]] || thermal_zone=/sys/class/thermal/thermal_zone0

trip_data=$(
    for type_file in "$thermal_zone"/trip_point_*_type; do
        [[ -r "$type_file" && "$(cat "$type_file")" == "active" ]] || continue
        base=${type_file%_type}
        temperature=$(cat "${base}_temp" 2>/dev/null || true)
        hysteresis=$(cat "${base}_hyst" 2>/dev/null || true)
        [[ -n "$temperature" ]] && printf '%s:%s\n' "$temperature" "$hysteresis"
    done | sort -n
)
trip_temperatures=$(printf '%s\n' "$trip_data" | awk -F: 'NF {print $1}' | paste -sd, -)
trip_hysteresis=$(printf '%s\n' "$trip_data" | awk -F: 'NF {print $2}' | paste -sd, -)
if [[ "$trip_temperatures" == "45000,55000,65000,70000" &&
      "$trip_hysteresis" == "5000,5000,5000,5000" ]]; then
    pass "Runtime fan thresholds and hysteresis match the plan"
else
    fail "Runtime fan thresholds differ from the plan" \
        "temperatures=${trip_temperatures:-missing}, hysteresis=${trip_hysteresis:-missing}"
fi

fan_name_file=$(grep -l '^pwmfan$' /sys/class/hwmon/hwmon*/name 2>/dev/null | head -n 1 || true)
if [[ -n "$fan_name_file" ]]; then
    fan_hwmon=${fan_name_file%/name}
    pwm=$(cat "$fan_hwmon/pwm1" 2>/dev/null || true)
    rpm=$(cat "$fan_hwmon/fan1_input" 2>/dev/null || true)
    cooling_state="unknown"
    [[ -n "$cooling_device" ]] && cooling_state=$(cat "$cooling_device/cur_state" 2>/dev/null || true)
    if [[ "$pwm" =~ ^[0-9]+$ && "$rpm" =~ ^[0-9]+$ ]]; then
        if ((pwm > 0 && rpm == 0)); then
            for _ in 1 2 3 4 5; do
                sleep 1
                rpm=$(cat "$fan_hwmon/fan1_input" 2>/dev/null || true)
                [[ "$rpm" =~ ^[0-9]+$ ]] && ((rpm > 0)) && break
            done
        fi
        if ((pwm > 0 && rpm == 0)); then
            fail "Fan PWM is active but no RPM is reported after 5 seconds" \
                "state=$cooling_state, pwm=$pwm, rpm=$rpm"
        else
            pass "Fan telemetry is internally consistent" "state=$cooling_state, pwm=$pwm, rpm=$rpm"
        fi
    else
        fail "Fan PWM or RPM telemetry is unavailable"
    fi
else
    fail "pwmfan hardware-monitor interface was not found"
fi

cpu_temp=$(cat "$thermal_zone/temp" 2>/dev/null || true)
if [[ "$cpu_temp" =~ ^[0-9]+$ ]]; then
    info "Current CPU temperature" "$(awk -v value="$cpu_temp" 'BEGIN {printf "%.1f C", value / 1000}')"
fi

section "6. RAM capture staging and iMac transfer"

capture_record=$(findmnt -n -o SOURCE,FSTYPE,OPTIONS /mnt/capture 2>/dev/null || true)
if [[ -n "$capture_record" ]]; then
    read -r capture_source capture_type capture_options <<< "$capture_record"
    if [[ "$capture_source" == "tmpfs" && "$capture_type" == "tmpfs" ]]; then
        pass "Capture staging uses RAM-backed tmpfs"
    else
        fail "Capture staging must use tmpfs" \
            "source=${capture_source:-unknown}, type=${capture_type:-unknown}"
    fi

    capture_size=$(findmnt -b -n -o SIZE /mnt/capture 2>/dev/null || true)
    if [[ "$capture_size" == "$EXPECTED_CAPTURE_SIZE_BYTES" ]]; then
        pass "Capture RAM limit" "256 MiB"
    else
        fail "Capture RAM limit differs from the plan" \
            "expected=$EXPECTED_CAPTURE_SIZE_BYTES, actual=${capture_size:-unknown}"
    fi

    missing_mount_options=""
    for option in rw nosuid nodev noexec noatime; do
        option_present "$capture_options" "$option" || missing_mount_options+=" $option"
    done
    if [[ -z "$missing_mount_options" ]]; then
        pass "Capture tmpfs mount options are hardened"
    else
        fail "Capture tmpfs mount options are incomplete" "missing:$missing_mount_options"
    fi
else
    fail "/mnt/capture RAM staging is not mounted"
fi

capture_owner=$(stat -c '%U:%G' /mnt/capture 2>/dev/null || true)
capture_mode=$(stat -c '%a' /mnt/capture 2>/dev/null || true)
if [[ "$capture_owner" == "$(id -un):$(id -gn)" && "$capture_mode" == "750" ]]; then
    pass "Capture staging ownership and permissions" "$capture_owner mode=$capture_mode"
else
    fail "Capture staging ownership or permissions differ from the plan" \
        "owner=${capture_owner:-unknown}, mode=${capture_mode:-unknown}"
fi

capture_mount_active=$(systemctl is-active mnt-capture.mount 2>/dev/null || true)
capture_mount_enabled=$(systemctl is-enabled mnt-capture.mount 2>/dev/null || true)
if [[ "$capture_mount_active" == "active" && "$capture_mount_enabled" == "enabled" ]]; then
    pass "Capture tmpfs mount is active and persistent"
else
    fail "Capture tmpfs mount is not active and persistent" \
        "active=${capture_mount_active:-unknown}, enabled=${capture_mount_enabled:-unknown}"
fi

# Capture transfer is optic_sync inside optic-daemon (docs/optic-daemon.md
# §6), configured by OPTIC_SYNC_* in the user unit; unset port/user fall
# back to the daemon defaults (src/main.rs DEFAULT_SYNC_REMOTE_*).
daemon_environment=$(systemctl --user show optic-daemon.service -p Environment --value 2>/dev/null || true)
sync_env_value() {
    local key=$1 entry
    for entry in $daemon_environment; do
        [[ "$entry" == "$key="* ]] && { printf '%s' "${entry#*=}"; return 0; }
    done
    return 1
}
sync_host=$(sync_env_value OPTIC_SYNC_REMOTE_HOST || true)
sync_user=$(sync_env_value OPTIC_SYNC_REMOTE_USER || echo admin)
sync_port=$(sync_env_value OPTIC_SYNC_REMOTE_PORT || echo 2222)
sync_enabled_env=$(sync_env_value OPTIC_SYNC_ENABLED || true)
if [[ "$sync_enabled_env" == "false" ]]; then
    fail "Capture sync is force-disabled" "OPTIC_SYNC_ENABLED=false in optic-daemon.service"
elif [[ "$sync_host" == "$EXPECTED_CAPTURE_HOST" &&
        "$sync_user" == "$EXPECTED_CAPTURE_USER" &&
        "$sync_port" == "$EXPECTED_CAPTURE_PORT" ]]; then
    pass "Capture sync target" "$sync_user@$sync_host:$sync_port"
else
    fail "Capture sync target differs from the plan" \
        "expected=$EXPECTED_CAPTURE_USER@$EXPECTED_CAPTURE_HOST:$EXPECTED_CAPTURE_PORT, actual=${sync_user:-missing}@${sync_host:-missing}:${sync_port:-missing}"
fi

for secret_file in "$HOME/.ssh/optic_capture_ed25519" "$HOME/.ssh/optic_capture_known_hosts"; do
    mode=$(stat -c '%a' "$secret_file" 2>/dev/null || true)
    if [[ "$mode" == "600" ]]; then
        pass "Protected capture-transfer file" "$secret_file mode=600"
    else
        fail "Capture-transfer file is missing or too broad" "$secret_file mode=${mode:-missing}"
    fi
done

if ssh-keygen -F "[$EXPECTED_CAPTURE_HOST]:$EXPECTED_CAPTURE_PORT" \
    -f "$HOME/.ssh/optic_capture_known_hosts" \
    >/dev/null 2>&1; then
    pass "iMac SSH host key is pinned" "$EXPECTED_CAPTURE_HOST"
else
    fail "iMac SSH host key is not pinned" "$EXPECTED_CAPTURE_HOST"
fi

# The retired Phase 6 shell transfer ships and then deletes every
# non-hidden file in /mnt/capture (including the daemon's
# preview_config.json) and races optic_sync, so it must not run.
legacy_timer_active=$(systemctl is-active optic-capture-transfer.timer 2>/dev/null || true)
legacy_timer_enabled=$(systemctl is-enabled optic-capture-transfer.timer 2>/dev/null || true)
if [[ "$legacy_timer_active" == "active" || "$legacy_timer_enabled" == "enabled" ]]; then
    fail "Retired shell capture transfer timer is still running" \
        "optic-capture-transfer.timer active=$legacy_timer_active, enabled=$legacy_timer_enabled; optic_sync replaces it"
else
    pass "Retired shell capture transfer timer is not running"
fi

# /api/status serializes SyncStatus as a flat JSON object (src/optic_sync.rs).
sync_status_json=$(curl -fsS --max-time 5 http://127.0.0.1:8000/api/status 2>/dev/null |
    sed -n 's/.*"sync":{\([^}]*\)}.*/\1/p' || true)
if [[ -z "$sync_status_json" ]]; then
    fail "Capture sync status unavailable" "GET http://127.0.0.1:8000/api/status"
elif [[ "$sync_status_json" != *'"enabled":true'* ]]; then
    fail "Capture sync is disabled in the running daemon" "$sync_status_json"
elif [[ "$sync_status_json" != *'"last_error":null'* ]]; then
    fail "Capture sync reports an error" \
        "$(sed -n 's/.*"last_error":\("[^"]*"\).*/\1/p' <<< "$sync_status_json")"
else
    pass "Capture sync is enabled with no error" \
        "$(sed -n 's/.*"connectivity":"\([^"]*\)".*/connectivity=\1/p' <<< "$sync_status_json")"
fi

receiver_probe=$(timeout 10 ssh -n -T \
    -p "$EXPECTED_CAPTURE_PORT" \
    -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes \
    -o UserKnownHostsFile="$HOME/.ssh/optic_capture_known_hosts" \
    -o ConnectTimeout=5 -i "$HOME/.ssh/optic_capture_ed25519" \
    "$EXPECTED_CAPTURE_USER@$EXPECTED_CAPTURE_HOST" ping 2>/dev/null || true)
if [[ "$receiver_probe" == "OK receiver" ]]; then
    pass "Authenticated iMac capture receiver probe"
else
    fail "Authenticated iMac capture receiver probe failed" \
        "response=${receiver_probe:-unavailable}"
fi

if [[ "$extra_filesystems" == *"/mnt/capture__Capture-RAM"* ]]; then
    pass "Beszel exports the capture RAM stage"
else
    fail "Beszel does not export the capture RAM stage" \
        "EXTRA_FILESYSTEMS=${extra_filesystems:-missing}"
fi

queued_files=$(find /mnt/capture -maxdepth 1 -type f ! -name '.*' -printf '.' 2>/dev/null |
    wc -c | tr -d ' ')
queued_bytes=$(find /mnt/capture -maxdepth 1 -type f ! -name '.*' -printf '%s\n' 2>/dev/null |
    awk '{total += $1} END {print total + 0}')
info "Capture transfer queue" "files=$queued_files, bytes=$queued_bytes"

section "7. RTC and time synchronization"

ntp_enabled=$(timedatectl show -p NTP --value 2>/dev/null || true)
ntp_synchronized=$(timedatectl show -p NTPSynchronized --value 2>/dev/null || true)
if [[ "$ntp_enabled" == "yes" ]]; then
    pass "Network time synchronization is enabled"
else
    fail "Network time synchronization is disabled" "NTP=${ntp_enabled:-unknown}"
fi
if [[ "$ntp_synchronized" == "yes" ]]; then
    pass "System clock is synchronized"
else
    fail "System clock is not synchronized" "NTPSynchronized=${ntp_synchronized:-unknown}"
fi

rtc_name=$(cat /sys/class/rtc/rtc0/name 2>/dev/null || true)
if [[ -n "$rtc_name" ]]; then
    pass "RTC device is available" "$rtc_name"
else
    fail "RTC device is unavailable"
fi

rtc_charge_count=$(firmware_line_count "dtparam=rtc_bbat_vcharge=3000000")
if [[ "$rtc_charge_count" == "1" ]]; then
    pass "RTC rechargeable-battery trickle charging is configured"
elif [[ "$rtc_charge_count" == "0" ]]; then
    info "RTC trickle charging is not configured" "optional; enable only for a supported rechargeable battery"
else
    warn "RTC trickle-charging setting is duplicated" "count=$rtc_charge_count"
fi

timezone=$(timedatectl show -p Timezone --value 2>/dev/null || true)
[[ -n "$timezone" ]] && info "Configured timezone" "$timezone"

section "8. Raspberry Pi HQ camera commissioning"

check_firmware_line "Camera firmware auto-detection is enabled" "camera_auto_detect=1"

user_groups=$(id -nG 2>/dev/null || true)
for required_group in video render; do
    if printf '%s\n' "$user_groups" | tr ' ' '\n' | grep -qx "$required_group"; then
        pass "Camera device group membership" "$required_group"
    else
        fail "Camera device group membership is missing" "$required_group"
    fi
done

if command -v rpicam-still >/dev/null 2>&1; then
    pass "HQ Camera capture tool is available" "rpicam-still"
else
    fail "HQ Camera capture tool is missing" "rpicam-still"
fi

camera_output=$(timeout 15 rpicam-still --list-cameras 2>&1 || true)
if [[ "$camera_output" == *"imx477"* && "$camera_output" == *"4056x3040"* ]]; then
    pass "Raspberry Pi HQ camera detected" "IMX477, 4056x3040"
else
    fail "Raspberry Pi HQ camera was not detected" "expected IMX477 at 4056x3040"
fi

info "Optical validation remains manual" \
    "confirm native-resolution framing and focus on the transferred iMac image"

# Host access for the daemon's dashboard system actions
# (scripts/setup-phase-08-daemon-host-access.sh). The user service needs
# lingering to start at boot without a login.
daemon_user=liam
if [[ -e "/var/lib/systemd/linger/$daemon_user" ]]; then
    pass "User manager lingering is enabled" "$daemon_user"
else
    fail "User manager lingering is disabled" "optic-daemon will not start at boot for $daemon_user"
fi

# pkcheck only asks polkitd for a decision; it performs no action. Any user
# may check its own process without details, but passing --detail (needed to
# scope the NTP rule to one unit and verb) requires a trusted root caller,
# and the rule files are readable only by root and polkitd. Those checks use
# non-interactive sudo when it is available and are skipped otherwise.
policy_subject_uid=$(id -u "$daemon_user" 2>/dev/null || true)
policy_subject_gid=$(id -g "$daemon_user" 2>/dev/null || true)
policy_root=()
if ((SECTION_ENABLED)); then
    if ((EUID == 0)); then
        policy_root=(env)
    elif sudo -n true 2>/dev/null; then
        policy_root=(sudo -n)
    fi
fi

if ! ((SECTION_ENABLED)); then
    :
elif ! command -v pkcheck >/dev/null 2>&1; then
    fail "PolicyKit checks are unavailable" "pkcheck is not installed"
elif [[ -z "$policy_subject_uid" ]]; then
    fail "PolicyKit rules were not checked" "user $daemon_user does not exist"
elif [[ "$(id -u)" != "$policy_subject_uid" ]] && ((${#policy_root[@]} == 0)); then
    warn "PolicyKit rules were not checked" "run verify.sh as $daemon_user or root"
else
    policy_subject=$$
    policy_sleeper=""
    if [[ "$(id -u)" != "$policy_subject_uid" ]]; then
        # Running as root: evaluate a short-lived process owned by the daemon
        # user, and wait until it has dropped root so polkitd sees that user.
        setpriv --reuid="$policy_subject_uid" --regid="$policy_subject_gid" --init-groups -- sleep 30 &
        policy_sleeper=$!
        policy_subject=$policy_sleeper
        for _ in {1..20}; do
            [[ "$(stat -c %u "/proc/$policy_sleeper" 2>/dev/null || true)" == "$policy_subject_uid" ]] && break
            sleep 0.1
        done
    fi

    for policy_action in \
        "org.freedesktop.login1.reboot|Reboot Pi" \
        "org.freedesktop.timedate1.set-timezone|Station timezone save"; do
        if pkcheck --process "$policy_subject" --action-id "${policy_action%%|*}" >/dev/null 2>&1; then
            pass "PolicyKit authorizes $daemon_user for ${policy_action#*|}" "${policy_action%%|*}"
        else
            fail "PolicyKit does not authorize $daemon_user for ${policy_action#*|}" \
                "${policy_action%%|*}; see scripts/setup-phase-08-daemon-host-access.sh"
        fi
    done

    if ((${#policy_root[@]} == 0)); then
        warn "NTP sync PolicyKit rule was not checked" "unit-scoped checks need root; rerun with sudo"
    else
        if "${policy_root[@]}" pkcheck --process "$policy_subject" \
            --action-id org.freedesktop.systemd1.manage-units \
            --detail unit systemd-timesyncd.service --detail verb restart >/dev/null 2>&1; then
            pass "PolicyKit authorizes $daemon_user for NTP Sync now" "restart systemd-timesyncd.service"
        else
            fail "PolicyKit does not authorize $daemon_user for NTP Sync now" \
                "restart systemd-timesyncd.service; see scripts/setup-phase-08-daemon-host-access.sh"
        fi
        # pkcheck exits 1 (not authorized) or 2 (authentication required)
        # for a denial; anything else is an error, not proof of scoping.
        "${policy_root[@]}" pkcheck --process "$policy_subject" \
            --action-id org.freedesktop.systemd1.manage-units \
            --detail unit ssh.service --detail verb restart >/dev/null 2>&1
        policy_status=$?
        if ((policy_status == 0)); then
            fail "NTP sync PolicyKit rule is too broad" "$daemon_user may restart ssh.service without authentication"
        elif ((policy_status == 1 || policy_status == 2)); then
            pass "NTP sync PolicyKit rule stays scoped to one unit" "ssh.service restart still requires authentication"
        else
            fail "NTP sync PolicyKit scope check failed" "pkcheck exit status $policy_status"
        fi

        for policy_rule in 60-optic-daemon-reboot.rules 61-optic-daemon-ntp-sync.rules 62-optic-daemon-set-timezone.rules; do
            policy_rule_meta=$("${policy_root[@]}" stat -c '%a %U:%G' "/etc/polkit-1/rules.d/$policy_rule" 2>/dev/null || true)
            if [[ "$policy_rule_meta" == "644 root:root" ]]; then
                pass "PolicyKit rule is installed" "$policy_rule ($policy_rule_meta)"
            elif [[ -z "$policy_rule_meta" ]]; then
                fail "PolicyKit rule is missing" "/etc/polkit-1/rules.d/$policy_rule"
            else
                warn "PolicyKit rule has unexpected ownership or mode" "$policy_rule ($policy_rule_meta)"
            fi
        done
    fi

    [[ -z "$policy_sleeper" ]] || kill "$policy_sleeper" 2>/dev/null || true
fi

section "9. Immutable root protection"

root_type=$(findmnt -n -o FSTYPE / 2>/dev/null || true)
root_options=$(findmnt -n -o OPTIONS / 2>/dev/null || true)
if [[ "$root_type" == "overlay" ]]; then
    pass "Root filesystem uses OverlayFS" "$root_options"
else
    warn "Root filesystem does not yet use OverlayFS" \
        "type=${root_type:-unknown}; enable only after all services are proven"
fi

boot_options=$(findmnt -n -o OPTIONS /boot/firmware 2>/dev/null || true)
if option_present "$boot_options" ro; then
    pass "Boot filesystem is read-only"
else
    warn "Boot filesystem is still writable" "enable read-only boot with the final OverlayFS step"
fi

printf '\n%sSummary%s: %sPASS=%d%s %sWARN=%d%s %sFAIL=%d%s %sINFO=%d%s\n' \
    "$C_BOLD" "$C_RESET" \
    "$C_PASS" "$PASS_COUNT" "$C_RESET" \
    "$C_WARN" "$WARN_COUNT" "$C_RESET" \
    "$C_FAIL" "$FAIL_COUNT" "$C_RESET" \
    "$C_INFO" "$INFO_COUNT" "$C_RESET"

if ((FAIL_COUNT > 0)); then
    exit 1
fi
if ((STRICT && WARN_COUNT > 0)); then
    exit 2
fi
exit 0
