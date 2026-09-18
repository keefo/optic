#!/usr/bin/env bash

# Install the Phase 1 optic-daemon binary as liam's persistent systemd user service.

set -euo pipefail

export LC_ALL=C
export PATH="$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

PROJECT_ROOT=$(cd "$(dirname "$0")/.." && pwd)
BINARY="$PROJECT_ROOT/target/release/optic-daemon"
UNIT_SOURCE="$PROJECT_ROOT/systemd/optic-daemon.service"
WEB_ASSETS_DIR="$PROJECT_ROOT/src/web"
INSTALL_DIR="$HOME/.local/bin"
UNIT_DIR="$HOME/.config/systemd/user"
RUNTIME_LIBRARY_DIR="$HOME/.local/optic-sysroot/usr/lib/aarch64-linux-gnu"
EXPECTED_VERSION=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$PROJECT_ROOT/Cargo.toml" | head -n 1)

usage() {
    cat <<'EOF'
Usage: setup-optic-daemon-phase-01.sh [--binary FILE] [--help]

Installs and starts optic-daemon for the current user. Run this script as liam,
not with sudo. The systemd user manager must have lingering enabled.
EOF
}

while (($#)); do
    case "$1" in
        --binary)
            [[ $# -ge 2 ]] || { printf '%s\n' 'Missing value for --binary.' >&2; exit 64; }
            BINARY=$2
            shift
            ;;
        --help|-h) usage; exit 0 ;;
        *) printf 'Unknown argument: %s\n' "$1" >&2; usage >&2; exit 64 ;;
    esac
    shift
done

[[ $EUID -ne 0 ]] || { printf '%s\n' 'Run as liam, not root.' >&2; exit 77; }
[[ $(id -un) == liam ]] || { printf '%s\n' 'This deployment expects user liam.' >&2; exit 77; }
[[ -x $BINARY ]] || { printf 'Release binary is missing: %s\n' "$BINARY" >&2; exit 69; }
[[ -r $UNIT_SOURCE ]] || { printf 'Service unit is missing: %s\n' "$UNIT_SOURCE" >&2; exit 69; }
[[ -n $EXPECTED_VERSION ]] || { printf '%s\n' 'Could not determine the expected daemon version.' >&2; exit 69; }
[[ -d $WEB_ASSETS_DIR ]] || { printf 'Web asset directory is missing: %s\n' "$WEB_ASSETS_DIR" >&2; exit 69; }
[[ -r $WEB_ASSETS_DIR/index.html ]] || {
    printf 'Web asset is missing: %s\n' "$WEB_ASSETS_DIR/index.html" >&2
    exit 69
}
[[ -r $RUNTIME_LIBRARY_DIR/libturbojpeg.so.0 ]] || {
    printf 'Runtime library is missing: %s\n' "$RUNTIME_LIBRARY_DIR/libturbojpeg.so.0" >&2
    exit 69
}
[[ $(findmnt -n -o FSTYPE /mnt/capture 2>/dev/null || true) == tmpfs ]] || {
    printf '%s\n' '/mnt/capture is not the Phase 6 tmpfs; refusing installation.' >&2
    exit 69
}
for group in video render; do
    id -nG | tr ' ' '\n' | grep -qx "$group" || {
        printf 'User liam is not in required group: %s\n' "$group" >&2
        exit 69
    }
done

install -d -m 0755 "$INSTALL_DIR" "$UNIT_DIR"
install -m 0755 "$BINARY" "$INSTALL_DIR/optic-daemon"

# Mirror the whole asset directory (not a fixed file list) so any file
# dropped into src/web/, including new subdirectories, is deployed without
# editing this script. Removing the old copy first drops assets that were
# deleted from the source tree since the last deployment.
rm -rf "$INSTALL_DIR/web"
install -d -m 0755 "$INSTALL_DIR/web"
cp -r "$WEB_ASSETS_DIR/." "$INSTALL_DIR/web/"
find "$INSTALL_DIR/web" -type d -exec chmod 0755 {} +
find "$INSTALL_DIR/web" -type f -exec chmod 0644 {} +

install -m 0644 "$UNIT_SOURCE" "$UNIT_DIR/optic-daemon.service"

systemd-analyze --user verify "$UNIT_DIR/optic-daemon.service"
systemctl --user daemon-reload
systemctl --user enable optic-daemon.service
systemctl --user restart optic-daemon.service

for _ in {1..20}; do
    if status=$(curl -fsS --max-time 2 http://127.0.0.1:8000/api/status 2>/dev/null); then
        deployed_version=$(python3 -c 'import json, sys; print(json.load(sys.stdin)["version"])' <<<"$status" 2>/dev/null || true)
        if [[ $deployed_version == "$EXPECTED_VERSION" ]]; then
            printf 'optic-daemon %s is running at http://optic.local:8000/\n' "$deployed_version"
            exit 0
        fi
    fi
    sleep 0.5
done

systemctl --user --no-pager --full status optic-daemon.service >&2 || true
printf 'optic-daemon did not become healthy at expected version %s.\n' "$EXPECTED_VERSION" >&2
exit 1