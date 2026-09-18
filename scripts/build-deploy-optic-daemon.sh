#!/usr/bin/env bash

# Bootstrap the native Pi build environment, build optic-daemon, and deploy it.
# Run from the macOS project checkout. No arguments are required.

set -euo pipefail

export LC_ALL=C

PROJECT_ROOT=$(cd "$(dirname "$0")/.." && pwd)
DEPLOY_TARGET=${OPTIC_DEPLOY_TARGET:-liam@optic.local}
EXPECTED_VERSION=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$PROJECT_ROOT/Cargo.toml" | head -n 1)
SSH_OPTIONS=(-o BatchMode=yes -o ConnectTimeout=10)

usage() {
    cat <<'EOF'
Usage: build-deploy-optic-daemon.sh [--help]

Uploads the current source to the Raspberry Pi, bootstraps its user-local
native build environment, validates and builds the release, installs it, and
verifies the service and web asset.

Optional environment override:
  OPTIC_DEPLOY_TARGET=user@host  SSH destination (default: liam@optic.local)
EOF
}

if (($#)); then
    case "$1" in
        --help|-h)
            usage
            exit 0
            ;;
        *)
            printf 'Unknown argument: %s\n' "$1" >&2
            usage >&2
            exit 64
            ;;
    esac
fi

[[ -n $EXPECTED_VERSION ]] || {
    printf '%s\n' 'Could not determine the package version from Cargo.toml.' >&2
    exit 69
}
[[ $EXPECTED_VERSION =~ ^[0-9A-Za-z.+-]+$ ]] || {
    printf 'Package version is unsafe for a directory name: %s\n' "$EXPECTED_VERSION" >&2
    exit 69
}

for command in ssh tar; do
    command -v "$command" >/dev/null || {
        printf 'Required local command is missing: %s\n' "$command" >&2
        exit 69
    }
# 4. Invoke high-performance Biome lint checks natively in macOS workspace before committing
for command in npx; do
    command -v "$command" >/dev/null || {
        printf 'Required web validation command is missing: %s\n' "$command" >&2
        exit 69
    }
done
npx @biomejs/biome check src/web/app.js src/web/index.html || {
    printf 'ERROR: Web assets syntax validation failed! Aborting deployment.\n' >&2
    exit 1
}
done

for path in Cargo.toml Cargo.lock src systemd scripts/setup-optic-daemon-phase-01.sh; do
    [[ -e $PROJECT_ROOT/$path ]] || {
        printf 'Required project path is missing: %s\n' "$PROJECT_ROOT/$path" >&2
        exit 69
    }
done

ARCHIVE=$(mktemp -t optic-daemon-source.XXXXXX)
trap 'rm -f "$ARCHIVE"' EXIT

printf 'Packaging optic-daemon %s from %s\n' "$EXPECTED_VERSION" "$PROJECT_ROOT"
COPYFILE_DISABLE=1 tar -czf "$ARCHIVE" -C "$PROJECT_ROOT" \
    Cargo.toml \
    Cargo.lock \
    README.md \
    optic-daemon-build-environment.md \
    optic-daemon-camera.md \
    optic-daemon.md \
    setup.md \
    scripts \
    src \
    systemd

printf 'Checking SSH access to %s\n' "$DEPLOY_TARGET"
ssh "${SSH_OPTIONS[@]}" "$DEPLOY_TARGET" 'test "$(id -un)" = liam'

printf 'Uploading the versioned source tree\n'
cat "$ARCHIVE" | ssh "${SSH_OPTIONS[@]}" "$DEPLOY_TARGET" \
    "set -eu; incoming=\"\$HOME/.local/src/.optic-daemon-$EXPECTED_VERSION.incoming\"; release=\"\$HOME/.local/src/optic-daemon-$EXPECTED_VERSION\"; rm -rf \"\$incoming\"; mkdir -p \"\$incoming\"; tar -xzf - -C \"\$incoming\"; rm -rf \"\$release\"; mv \"\$incoming\" \"\$release\""

printf 'Bootstrapping, building, and deploying on %s\n' "$DEPLOY_TARGET"
ssh "${SSH_OPTIONS[@]}" "$DEPLOY_TARGET" bash -s -- "$EXPECTED_VERSION" <<'REMOTE_SCRIPT'
set -euo pipefail

export LC_ALL=C
export PATH="$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

VERSION=$1
SOURCE_DIR="$HOME/.local/src/optic-daemon-$VERSION"
OPTIC_SYSROOT="$HOME/.local/optic-sysroot"
OPTIC_NATIVE_LIB="$OPTIC_SYSROOT/usr/lib/aarch64-linux-gnu"
DEB_CACHE="$HOME/.cache/optic-debs"
CARGO_TARGET_DIR="$HOME/.cache/optic-daemon-target"
RUST_VERSION=1.98.1

required_packages=(
    'libcamera0.7=0.7.2+rpt20260817-1'
    'libcamera-dev=0.7.2+rpt20260817-1'
    'libclang1-19=1:19.1.7-3+b1'
    'libclang-common-19-dev=1:19.1.7-3+b1'
    'libllvm19=1:19.1.7-3+b1'
    'libz3-4=4.13.3-1'
    'libjpeg62-turbo-dev=1:2.1.5-4'
    'libturbojpeg0=1:2.1.5-4'
    'libturbojpeg0-dev=1:2.1.5-4'
)

fail() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

printf '%s\n' '==> Checking Raspberry Pi prerequisites'
[[ $(id -un) == liam ]] || fail 'deployment must run as liam'
[[ $(uname -m) == aarch64 ]] || fail 'deployment requires an AArch64 Raspberry Pi'
[[ -d $SOURCE_DIR ]] || fail "staged source is missing: $SOURCE_DIR"
[[ $(findmnt -n -o FSTYPE /mnt/capture 2>/dev/null || true) == tmpfs ]] || \
    fail '/mnt/capture is not the Phase 6 tmpfs'
for group in video render; do
    id -nG | tr ' ' '\n' | grep -qx "$group" || fail "liam is not in group $group"
done
[[ $(loginctl show-user liam -p Linger --value 2>/dev/null || true) == yes ]] || \
    fail 'systemd lingering is not enabled for liam'

for command in cmp curl date dpkg-deb gcc g++ install ldd pkg-config python3 systemctl systemd-analyze; do
    command -v "$command" >/dev/null || fail "required Pi command is missing: $command"
done
[[ -r /lib/aarch64-linux-gnu/libcamera.so.0.7 ]] || \
    fail 'system libcamera.so.0.7 is missing; install the Raspberry Pi libcamera runtime first'
source_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$SOURCE_DIR/Cargo.toml" | head -n 1)
[[ $source_version == "$VERSION" ]] || \
    fail "staged source version $source_version does not match requested version $VERSION"

printf '%s\n' '==> Bootstrapping the pinned Rust toolchain'
if ! command -v rustup >/dev/null; then
    curl --proto '=https' --tlsv1.2 --fail --silent --show-error \
        https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
    export PATH="$HOME/.cargo/bin:$PATH"
fi

if rustup run stable rustc --version 2>/dev/null | grep -q "^rustc $RUST_VERSION "; then
    RUST_TOOLCHAIN=stable
else
    RUST_TOOLCHAIN=$RUST_VERSION
    rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal
fi
rustup component add --toolchain "$RUST_TOOLCHAIN" rustfmt clippy
rustup run "$RUST_TOOLCHAIN" rustc --version
rustup run "$RUST_TOOLCHAIN" cargo --version
rustup run "$RUST_TOOLCHAIN" cargo clippy --version

expected_manifest=$(printf '%s\n' "${required_packages[@]}")
sysroot_ready=false
if [[ -r $OPTIC_SYSROOT/.optic-package-versions ]] &&
   [[ $(cat "$OPTIC_SYSROOT/.optic-package-versions") == "$expected_manifest" ]] &&
   [[ -r $OPTIC_NATIVE_LIB/libclang-19.so.1 ]] &&
   [[ -r $OPTIC_NATIVE_LIB/libLLVM.so.19.1 ]] &&
   [[ -r $OPTIC_NATIVE_LIB/libcamera.so ]] &&
   [[ -r $OPTIC_NATIVE_LIB/libcamera-base.so ]] &&
   [[ -r $OPTIC_NATIVE_LIB/libturbojpeg.so.0 ]] &&
   [[ -r $OPTIC_NATIVE_LIB/pkgconfig/libcamera.pc ]] &&
   [[ -r $OPTIC_NATIVE_LIB/pkgconfig/libturbojpeg.pc ]]; then
    sysroot_ready=true
fi

cached_package() {
    local wanted_name=$1
    local wanted_version=$2
    local package name package_version architecture

    for package in "$DEB_CACHE"/*.deb; do
        [[ -e $package ]] || continue
        name=$(dpkg-deb -f "$package" Package 2>/dev/null || true)
        package_version=$(dpkg-deb -f "$package" Version 2>/dev/null || true)
        architecture=$(dpkg-deb -f "$package" Architecture 2>/dev/null || true)
        if [[ $name == "$wanted_name" && $package_version == "$wanted_version" && $architecture == arm64 ]]; then
            printf '%s\n' "$package"
            return 0
        fi
    done
    return 1
}

if [[ $sysroot_ready != true ]]; then
    printf '%s\n' '==> Rebuilding the pinned user-local native sysroot'
    command -v apt-get >/dev/null || fail 'apt-get is required to download native packages'
    mkdir -p "$DEB_CACHE" "$HOME/.local"

    for specification in "${required_packages[@]}"; do
        name=${specification%%=*}
        package_version=${specification#*=}
        if ! cached_package "$name" "$package_version" >/dev/null; then
            printf 'Downloading %s\n' "$specification"
            (cd "$DEB_CACHE" && apt-get download "$specification")
        fi
        cached_package "$name" "$package_version" >/dev/null || \
            fail "the AArch64 package was not cached: $specification"
    done

    new_sysroot=$(mktemp -d "$HOME/.local/.optic-sysroot.XXXXXX")
    for specification in "${required_packages[@]}"; do
        name=${specification%%=*}
        package_version=${specification#*=}
        package=$(cached_package "$name" "$package_version")
        dpkg-deb -x "$package" "$new_sysroot"
    done
    printf '%s\n' "${required_packages[@]}" > "$new_sysroot/.optic-package-versions"

    new_native_lib="$new_sysroot/usr/lib/aarch64-linux-gnu"
    [[ -r $new_native_lib/libclang-19.so.1 ]] || fail 'staged libclang is missing'
    [[ -r $new_native_lib/libLLVM.so.19.1 ]] || fail 'staged LLVM is missing'
    [[ -r $new_native_lib/libcamera.so ]] || fail 'staged libcamera linker target is missing'
    [[ -r $new_native_lib/libcamera-base.so ]] || fail 'staged libcamera-base linker target is missing'
    [[ -r $new_native_lib/libturbojpeg.so.0 ]] || fail 'staged TurboJPEG is missing'
    [[ -r $new_native_lib/pkgconfig/libcamera.pc ]] || fail 'staged libcamera.pc is missing'

    old_sysroot="$OPTIC_SYSROOT.previous"
    rm -rf "$old_sysroot"
    if [[ -e $OPTIC_SYSROOT ]]; then
        mv "$OPTIC_SYSROOT" "$old_sysroot"
    fi
    if ! mv "$new_sysroot" "$OPTIC_SYSROOT"; then
        [[ ! -e $old_sysroot ]] || mv "$old_sysroot" "$OPTIC_SYSROOT"
        fail 'could not activate the rebuilt native sysroot'
    fi
    rm -rf "$old_sysroot"
else
    printf '%s\n' '==> Pinned native sysroot is already ready'
fi

export OPTIC_SYSROOT OPTIC_NATIVE_LIB CARGO_TARGET_DIR
export PKG_CONFIG_PATH="$OPTIC_NATIVE_LIB/pkgconfig"
export PKG_CONFIG_SYSROOT_DIR="$OPTIC_SYSROOT"
export LIBCLANG_PATH="$OPTIC_NATIVE_LIB"
GCC_INCLUDE=$(gcc -print-file-name=include)
[[ -r $GCC_INCLUDE/stddef.h ]] || fail "GCC builtin headers are missing: $GCC_INCLUDE"
export BINDGEN_EXTRA_CLANG_ARGS="-I$GCC_INCLUDE"
unset SYSROOT LD_LIBRARY_PATH

pkg-config --modversion libcamera libcamera-base libturbojpeg

printf '%s\n' '==> Formatting and testing'
cd "$SOURCE_DIR"
rustup run "$RUST_TOOLCHAIN" cargo fmt --all -- --check
LD_LIBRARY_PATH="$OPTIC_NATIVE_LIB" \
    rustup run "$RUST_TOOLCHAIN" cargo test --locked --all-targets

printf '%s\n' '==> Running strict Clippy with isolated compiler libraries'
CLIPPY_WRAPPER=$(mktemp "$HOME/.cache/optic-clippy-wrapper.XXXXXX")
trap 'rm -f "$CLIPPY_WRAPPER"' EXIT
cat > "$CLIPPY_WRAPPER" <<'CLIPPY_WRAPPER_SCRIPT'
#!/usr/bin/env bash
set -euo pipefail

sanitized=
old_ifs=$IFS
IFS=:
for directory in ${LD_LIBRARY_PATH:-}; do
    if [[ -n $directory && $directory != "${OPTIC_NATIVE_LIB:?}" ]]; then
        if [[ -n $sanitized ]]; then
            sanitized="$sanitized:$directory"
        else
            sanitized=$directory
        fi
    fi
done
IFS=$old_ifs

if [[ -n $sanitized ]]; then
    export LD_LIBRARY_PATH=$sanitized
else
    unset LD_LIBRARY_PATH
fi
exec "$@"
CLIPPY_WRAPPER_SCRIPT
chmod 0700 "$CLIPPY_WRAPPER"
LD_LIBRARY_PATH="$OPTIC_NATIVE_LIB" RUSTC_WRAPPER="$CLIPPY_WRAPPER" \
    rustup run "$RUST_TOOLCHAIN" cargo clippy --locked --all-targets -- -D warnings
rm -f "$CLIPPY_WRAPPER"
trap - EXIT

printf '%s\n' '==> Building the optimized release with staged libclang'
LD_LIBRARY_PATH="$OPTIC_NATIVE_LIB" \
    rustup run "$RUST_TOOLCHAIN" cargo build --locked --release

BINARY="$CARGO_TARGET_DIR/release/optic-daemon"
[[ -x $BINARY ]] || fail "release binary is missing: $BINARY"
LD_LIBRARY_PATH="$OPTIC_NATIVE_LIB" ldd "$BINARY" | tee /tmp/optic-daemon.ldd
if grep -q 'not found' /tmp/optic-daemon.ldd; then
    fail 'the release binary has unresolved shared libraries'
fi
grep -q 'libcamera.so.0.7' /tmp/optic-daemon.ldd || fail 'release binary is not linked to libcamera.so.0.7'
grep -q 'libturbojpeg.so.0' /tmp/optic-daemon.ldd || fail 'release binary is not linked to libturbojpeg.so.0'
grep -aF "$VERSION" "$BINARY" >/dev/null || fail 'release binary does not contain the expected version'
grep -aF '${profile.previewFps} FPS' "$BINARY" >/dev/null || fail 'release binary does not contain the FPS web asset'

printf '%s\n' '==> Installing with rollback protection'
backup_dir=$(mktemp -d "$HOME/.cache/optic-daemon-deploy.XXXXXX")
had_binary=false
had_unit=false
if [[ -f $HOME/.local/bin/optic-daemon ]]; then
    cp -p "$HOME/.local/bin/optic-daemon" "$backup_dir/optic-daemon"
    had_binary=true
fi
if [[ -f $HOME/.config/systemd/user/optic-daemon.service ]]; then
    cp -p "$HOME/.config/systemd/user/optic-daemon.service" "$backup_dir/optic-daemon.service"
    had_unit=true
fi

rollback() {
    printf '%s\n' 'Deployment verification failed; restoring the previous service.' >&2
    set +e
    if [[ $had_binary == true ]]; then
        install -m 0755 "$backup_dir/optic-daemon" "$HOME/.local/bin/optic-daemon"
    else
        rm -f "$HOME/.local/bin/optic-daemon"
    fi
    if [[ $had_unit == true ]]; then
        install -m 0644 "$backup_dir/optic-daemon.service" \
            "$HOME/.config/systemd/user/optic-daemon.service"
        systemctl --user daemon-reload
        systemctl --user restart optic-daemon.service
    else
        systemctl --user disable --now optic-daemon.service
        rm -f "$HOME/.config/systemd/user/optic-daemon.service"
        systemctl --user daemon-reload
    fi
    systemctl --user --no-pager --full status optic-daemon.service >&2
    rm -rf "$backup_dir"
    exit 1
}

deploy_started=$(date --iso-8601=seconds)
if ! ./scripts/setup-optic-daemon-phase-01.sh --binary "$BINARY"; then
    rollback
fi

if ! cmp -s "$BINARY" "$HOME/.local/bin/optic-daemon"; then
    printf '%s\n' 'Installed binary does not match the release artifact.' >&2
    rollback
fi
if ! systemctl --user is-enabled --quiet optic-daemon.service; then
    printf '%s\n' 'optic-daemon.service is not enabled.' >&2
    rollback
fi
if ! systemctl --user is-active --quiet optic-daemon.service; then
    printf '%s\n' 'optic-daemon.service is not active.' >&2
    rollback
fi
if ! curl --fail --silent --show-error http://127.0.0.1:8000/healthz >/dev/null; then
    printf '%s\n' 'The deployed health endpoint failed.' >&2
    rollback
fi

status=$(curl --fail --silent --show-error http://127.0.0.1:8000/api/status) || rollback
deployed_version=$(python3 -c 'import json, sys; print(json.load(sys.stdin)["version"])' <<<"$status") || rollback
if [[ $deployed_version != "$VERSION" ]]; then
    printf 'Expected API version %s, received %s.\n' "$VERSION" "$deployed_version" >&2
    rollback
fi

app_js=$(curl --fail --silent --show-error http://127.0.0.1:8000/app.js) || rollback
if ! grep -F 'previewFps' <<<"$app_js" >/dev/null ||
   ! grep -F 'recordRenderedFrame(headers, paintedAt)' <<<"$app_js" >/dev/null ||
   ! grep -F 'previewFps: 2' <<<"$app_js" >/dev/null ||
   [[ $(grep -o 'previewFps: 8' <<<"$app_js" | wc -l | tr -d ' ') -lt 2 ]]; then
    printf '%s\n' 'The served web asset does not contain all profile FPS values.' >&2
    rollback
fi
index_html=$(curl --fail --silent --show-error http://127.0.0.1:8000/) || rollback
if ! grep -F 'Preview responsiveness' <<<"$index_html" >/dev/null ||
   ! grep -F 'First visible' <<<"$index_html" >/dev/null; then
    printf '%s\n' 'The served dashboard does not contain responsiveness measurements.' >&2
    rollback
fi

error_logs=$(journalctl --user -u optic-daemon.service --since "$deploy_started" \
    --priority err --quiet --no-pager -o cat || true)
if [[ -n $error_logs ]]; then
    printf '%s\n' "$error_logs" >&2
    rollback
fi

rm -rf "$HOME/.local/src/optic-daemon"
ln -s "optic-daemon-$VERSION" "$HOME/.local/src/optic-daemon"
rm -rf "$backup_dir"

printf '%s\n' '==> Recent service log'
journalctl --user -u optic-daemon.service -n 20 --no-pager
printf 'SUCCESS: optic-daemon %s is active at http://optic.local:8000/\n' "$VERSION"
REMOTE_SCRIPT