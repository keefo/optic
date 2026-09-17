#!/usr/bin/env bash

# Configure Phase 2 of the Project Optic hardening plan.

set -euo pipefail

export LC_ALL=C
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

DRY_RUN=0
REBOOT=0
BACKUP_DIR="/var/backups/optic-hardening"
RPI_SWAP_FILE="/etc/rpi/swap.conf.d/90-optic.conf"
ZRAM_GENERATOR_FILE="/etc/systemd/zram-generator.conf.d/90-optic.conf"
SYSCTL_FILE="/etc/sysctl.d/99-optic-memory.conf"
GENERATOR="/usr/lib/systemd/system-generators/rpi-swap-generator"
TEMPORARY=""
VALIDATION_DIR=""

usage() {
    cat <<'EOF'
Usage: setup-phase-02-memory.sh [--dry-run] [--reboot] [--help]

Idempotently configures native Raspberry Pi OS pure zram swap at 50% of RAM,
zstd compression, priority 100, and Project Optic virtual-memory sysctls.

  --dry-run   Report required changes without modifying the system.
  --reboot    Reboot when required to activate zram configuration changes.
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

MEMORY_KIB=$(awk '/^MemTotal:/ {print $2}' /proc/meminfo)
if [[ ! "$MEMORY_KIB" =~ ^[0-9]+$ ]] || ((MEMORY_KIB <= 0)); then
    printf '%s\n' 'Unable to determine physical memory.' >&2
    exit 1
fi
TARGET_ZRAM_MIB=$((MEMORY_KIB / 1024 / 2))

rpi_swap_content=$(cat <<EOF
# Managed by Project Optic setup-phase-02-memory.sh
[Main]
Mechanism=zram

[Zram]
RamMultiplier=0.5
FixedSizeMiB=$TARGET_ZRAM_MIB
EOF
)

zram_generator_content=$(cat <<'EOF'
# Managed by Project Optic setup-phase-02-memory.sh
[zram0]
compression-algorithm=zstd
swap-priority=100
EOF
)

sysctl_content=$(cat <<'EOF'
# Managed by Project Optic setup-phase-02-memory.sh
# Favor reclaiming page cache over aggressive swapping.
vm.swappiness=10
# Reserve 64 MiB for high-order CSI frame allocations.
vm.min_free_kbytes=65536
# Preserve filesystem metadata cache.
vm.vfs_cache_pressure=50
EOF
)

cleanup() {
    [[ -z "$TEMPORARY" ]] || rm -f -- "$TEMPORARY"
    [[ -z "$VALIDATION_DIR" ]] || rm -rf -- "$VALIDATION_DIR"
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
        return 1
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
    return 0
}

runtime_zram_matches() {
    local memory_kib zram_bytes zram_percent algorithm priority backing non_zram
    memory_kib=$(awk '/^MemTotal:/ {print $2}' /proc/meminfo)
    [[ -r /sys/block/zram0/disksize ]] || return 1
    zram_bytes=$(cat /sys/block/zram0/disksize)
    zram_percent=$((zram_bytes / 1024 * 100 / memory_kib))
    algorithm=$(sed -n 's/.*\[\([^]]*\)\].*/\1/p' /sys/block/zram0/comp_algorithm)
    priority=$(awk '$1 == "/dev/zram0" {print $5}' /proc/swaps)
    backing=$(cat /sys/block/zram0/backing_dev 2>/dev/null || true)
    non_zram=$(awk 'NR > 1 && $1 !~ /^\/dev\/zram[0-9]+$/ {print; exit}' /proc/swaps)
    [[ "$zram_percent" -ge 48 && "$zram_percent" -le 52 &&
       "$algorithm" == "zstd" && "$priority" == "100" &&
       "$backing" == "none" && -z "$non_zram" ]]
}

if [[ ! -x "$GENERATOR" ]] || ! command -v rpi-systemd-config >/dev/null 2>&1; then
    printf '%s\n' 'Required native rpi-swap components are not installed.' >&2
    exit 69
fi

if ((DRY_RUN)); then
    printf '%s\n' 'Project Optic Phase 2 dry run'
    show_file_plan "$RPI_SWAP_FILE" "$rpi_swap_content"
    show_file_plan "$ZRAM_GENERATOR_FILE" "$zram_generator_content"
    show_file_plan "$SYSCTL_FILE" "$sysctl_content"
    if runtime_zram_matches; then
        printf '%s\n' '[OK] Runtime zram state already matches the plan.'
    else
        printf '%s\n' '[CHANGE] A reboot is required to activate the planned zram state.'
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

printf '%s\n' 'Configuring Project Optic Phase 2: zram and virtual memory'

ZRAM_CONFIGURATION_CHANGED=0
if install_managed_file "$RPI_SWAP_FILE" "$rpi_swap_content"; then
    ZRAM_CONFIGURATION_CHANGED=1
fi
if install_managed_file "$ZRAM_GENERATOR_FILE" "$zram_generator_content"; then
    ZRAM_CONFIGURATION_CHANGED=1
fi
install_managed_file "$SYSCTL_FILE" "$sysctl_content" || true

VALIDATION_DIR=$(mktemp -d)
mkdir -p "$VALIDATION_DIR/normal" "$VALIDATION_DIR/early" "$VALIDATION_DIR/late"
validation_generator="$VALIDATION_DIR/rpi-swap-generator"
sed \
    -e 's|generator_dir="/run/systemd/${base_path}"|generator_dir="${VALIDATION_ROOT}/systemd/${base_path}"|' \
    -e 's|local sysctl_dir="/run/sysctl.d"|local sysctl_dir="${VALIDATION_ROOT}/sysctl.d"|' \
    "$GENERATOR" > "$validation_generator"
chmod 0755 "$validation_generator"
VALIDATION_ROOT="$VALIDATION_DIR/runtime" \
    "$validation_generator" "$VALIDATION_DIR/normal" "$VALIDATION_DIR/early" "$VALIDATION_DIR/late"
generated_zram="$VALIDATION_DIR/runtime/systemd/zram-generator.conf.d/20-rpi-swap-zram0-ctrl.conf"
generated_swap="$VALIDATION_DIR/normal/dev-zram0.swap"
[[ -r "$generated_zram" && -r "$generated_swap" ]]
grep -q '^host-memory-limit=none$' "$generated_zram"
grep -q "^zram-size=$TARGET_ZRAM_MIB$" "$generated_zram"
if grep -q '^writeback-device=' "$generated_zram"; then
    printf '%s\n' 'Generated zram configuration unexpectedly enables disk writeback.' >&2
    exit 1
fi
grep -q '^Priority=100$' "$generated_swap"
printf '%s\n' 'Validated native rpi-swap output: pure zram, no disk writeback.'

sysctl --load "$SYSCTL_FILE"
for specification in vm.swappiness:10 vm.min_free_kbytes:65536 vm.vfs_cache_pressure:50; do
    key=${specification%%:*}
    expected=${specification#*:}
    actual=$(sysctl -n "$key")
    [[ "$actual" == "$expected" ]] || {
        printf '%s runtime value is %s; expected %s.\n' "$key" "$actual" "$expected" >&2
        exit 1
    }
done

if ((ZRAM_CONFIGURATION_CHANGED)) || ! runtime_zram_matches; then
    printf '%s\n' 'Phase 2 configuration is installed; reboot is required for zram changes.'
    if ((REBOOT)); then
        printf '%s\n' 'Rebooting now.'
        systemctl reboot
    else
        printf '%s\n' 'Rerun this script with --reboot to activate the zram configuration.'
    fi
else
    printf '%s\n' 'Phase 2 setup complete; runtime and persistent state match the plan.'
fi