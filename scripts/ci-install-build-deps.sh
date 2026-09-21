#!/usr/bin/env bash

# Install the pinned native build dependencies for optic-daemon inside a
# Debian 13 (trixie) arm64 CI container. Run as root from the project root.
#
# The package pins are parsed from scripts/build-deploy-optic-daemon.sh
# (`required_packages`), which is the single source of truth shared with the
# on-Pi build. Unlike the Pi, which extracts them into a user-local sysroot,
# CI installs them system-wide because the container has root.
#
# Usage: scripts/ci-install-build-deps.sh [--print-packages]
#   --print-packages  Print the parsed pins and exit without installing.

set -euo pipefail

export LC_ALL=C
export DEBIAN_FRONTEND=noninteractive

PROJECT_ROOT=$(cd "$(dirname "$0")/.." && pwd)
DEPLOY_SCRIPT="$PROJECT_ROOT/scripts/build-deploy-optic-daemon.sh"
# The archive key comes from the raspberrypi-archive-keyring package, pinned
# by SHA-256. The loose raspberrypi.gpg.key download carries SHA-1 binding
# signatures, which Debian 13's apt verifier (sqv) rejects since 2026-02-01;
# the package ships the same key re-signed with SHA-512.
RPI_KEYRING_DEB_URL=https://archive.raspberrypi.com/debian/pool/main/r/raspberrypi-archive-keyring/raspberrypi-archive-keyring_2025.1+rpt1_all.deb
RPI_KEYRING_DEB_SHA256=2e727149d7acb8cc7f604e66d0049161039c8aa1eaf1175e54f9e69d963d60e4
RPI_KEY_FINGERPRINT=CF8A1AF502A2AA2D763BAE7E82B129927FA3303E
RPI_KEYRING=/usr/share/keyrings/raspberrypi-archive-keyring.pgp

fail() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

# A read loop rather than mapfile keeps --print-packages usable under the
# macOS system bash 3.2.
packages=()
while IFS= read -r specification; do
    packages+=("$specification")
done < <(sed -n '/^required_packages=($/,/^)$/p' "$DEPLOY_SCRIPT" | grep -o "'[^']*'" | tr -d "'")
((${#packages[@]} > 0)) || fail "no required_packages found in $DEPLOY_SCRIPT"
for specification in "${packages[@]}"; do
    [[ $specification =~ ^[a-z0-9.+-]+=[0-9A-Za-z.+:~-]+$ ]] || \
        fail "unexpected package pin: $specification"
done

if [[ ${1:-} == --print-packages ]]; then
    printf '%s\n' "${packages[@]}"
    exit 0
fi
(($# == 0)) || fail "unknown argument: $1"

[[ $(id -u) == 0 ]] || fail 'must run as root (CI container)'
[[ $(dpkg --print-architecture) == arm64 ]] || fail 'requires an arm64 Debian container'
# shellcheck source=/dev/null
. /etc/os-release
[[ ${VERSION_CODENAME:-} == trixie ]] || fail "requires Debian trixie, found ${VERSION_CODENAME:-unknown}"

printf '%s\n' '==> Installing base build tools'
apt-get update
apt-get install -y --no-install-recommends \
    ca-certificates curl gcc g++ git gpg libc6-dev pkg-config xz-utils

printf '%s\n' '==> Adding the Raspberry Pi archive (keyring package and fingerprint pinned)'
work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT
curl --proto '=https' --tlsv1.2 --fail --silent --show-error "$RPI_KEYRING_DEB_URL" \
    -o "$work_dir/keyring.deb"
printf '%s  %s\n' "$RPI_KEYRING_DEB_SHA256" "$work_dir/keyring.deb" | sha256sum --check --quiet || \
    fail 'raspberrypi-archive-keyring package SHA-256 mismatch'
dpkg-deb -x "$work_dir/keyring.deb" "$work_dir/keyring"
key_file="$work_dir/keyring/usr/share/keyrings/raspberrypi-archive-keyring.pgp"
[[ -r $key_file ]] || fail 'keyring package does not contain raspberrypi-archive-keyring.pgp'
actual_fingerprint=$(gpg --show-keys --with-colons "$key_file" | awk -F: '$1 == "fpr" { print $10; exit }')
[[ $actual_fingerprint == "$RPI_KEY_FINGERPRINT" ]] || \
    fail "Raspberry Pi archive key fingerprint mismatch: got ${actual_fingerprint:-none}"
install -m 0644 "$key_file" "$RPI_KEYRING"
cat > /etc/apt/sources.list.d/raspi.sources <<EOF
Types: deb
URIs: http://archive.raspberrypi.com/debian/
Suites: trixie
Components: main
Signed-By: $RPI_KEYRING
EOF

printf '%s\n' '==> Installing pinned native packages'
printf '  %s\n' "${packages[@]}"
apt-get update
apt-get install -y --no-install-recommends "${packages[@]}"

printf '%s\n' '==> Installed native library versions'
pkg-config --modversion libcamera libcamera-base libturbojpeg
