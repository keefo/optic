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
Usage: build-deploy-optic-daemon.sh [--assets] [--help]

Uploads the current source to the Raspberry Pi, bootstraps its user-local
native build environment, validates and builds the release, installs it, and
verifies the service and web asset.

  --assets   Fast path: deploy only src/web/* (HTML/CSS/JS). Skips the Rust
             toolchain bootstrap and cargo fmt/test/clippy/build entirely,
             and never stops or restarts optic-daemon.service, since static
             assets are served straight from disk on every request — no
             rebuild or restart is needed to pick up a change. Still lints
             with Biome, still installs through a backup-and-rollback path,
             and still verifies the served content over real HTTP requests.
             Use this for CSS/HTML/JS-only changes; use the full (no-flag)
             path for anything touching src/*.rs, Cargo.toml, or systemd/.
  --help     Display this help.

Optional environment override:
  OPTIC_DEPLOY_TARGET=user@host  SSH destination (default: liam@optic.local)
EOF
}

MODE=full
if (($#)); then
    case "$1" in
        --help|-h)
            usage
            exit 0
            ;;
        --assets)
            MODE=assets
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
done

if [[ $MODE == full ]]; then
    for path in Cargo.toml Cargo.lock README.md biome.json docs/optic-daemon-build-environment.md \
        docs/optic-daemon-camera.md docs/optic-daemon.md setup.md src systemd \
        scripts/setup-optic-daemon-phase-01.sh; do
        [[ -e $PROJECT_ROOT/$path ]] || {
            printf 'Required project path is missing: %s\n' "$PROJECT_ROOT/$path" >&2
            exit 69
        }
    done
else
    for path in biome.json src/web; do
        [[ -e $PROJECT_ROOT/$path ]] || {
            printf 'Required project path is missing: %s\n' "$PROJECT_ROOT/$path" >&2
            exit 69
        }
    done
fi

ARCHIVE=$(mktemp -t optic-daemon-source.XXXXXX)
trap 'rm -f "$ARCHIVE"' EXIT

if [[ $MODE == full ]]; then
    printf 'Packaging optic-daemon %s from %s\n' "$EXPECTED_VERSION" "$PROJECT_ROOT"
    COPYFILE_DISABLE=1 tar -czf "$ARCHIVE" -C "$PROJECT_ROOT" \
        Cargo.toml \
        Cargo.lock \
        README.md \
        biome.json \
        docs/optic-daemon-build-environment.md \
        docs/optic-daemon-camera.md \
        docs/optic-daemon.md \
        setup.md \
        scripts \
        src \
        systemd
else
    printf 'Packaging static web assets from %s/src/web\n' "$PROJECT_ROOT"
    # biome.json must travel with src/web, not just src/web itself: Biome
    # discovers its config by walking up from the current directory, and
    # without it here it silently falls back to its own defaults (tabs)
    # instead of this project's `indentStyle: space`, flagging the entire
    # file as needing reformatting. Found by actually running this against
    # the Pi, not by reasoning about it — see the worklog for this feature.
    COPYFILE_DISABLE=1 tar -czf "$ARCHIVE" -C "$PROJECT_ROOT" biome.json src/web
fi

printf 'Checking SSH access to %s\n' "$DEPLOY_TARGET"
ssh "${SSH_OPTIONS[@]}" "$DEPLOY_TARGET" 'test "$(id -un)" = liam'

if [[ $MODE == full ]]; then
    printf 'Uploading the versioned source tree\n'
    cat "$ARCHIVE" | ssh "${SSH_OPTIONS[@]}" "$DEPLOY_TARGET" \
        "set -eu; incoming=\"\$HOME/.local/src/.optic-daemon-$EXPECTED_VERSION.incoming\"; release=\"\$HOME/.local/src/optic-daemon-$EXPECTED_VERSION\"; rm -rf \"\$incoming\"; mkdir -p \"\$incoming\"; tar -xzf - -C \"\$incoming\"; rm -rf \"\$release\"; mv \"\$incoming\" \"\$release\""
    printf 'Bootstrapping, building, and deploying on %s\n' "$DEPLOY_TARGET"
else
    printf 'Uploading static web assets\n'
    cat "$ARCHIVE" | ssh "${SSH_OPTIONS[@]}" "$DEPLOY_TARGET" \
        "set -eu; incoming=\"\$HOME/.cache/.optic-assets-deploy.incoming\"; release=\"\$HOME/.cache/optic-assets-deploy\"; rm -rf \"\$incoming\"; mkdir -p \"\$incoming\"; tar -xzf - -C \"\$incoming\"; rm -rf \"\$release\"; mv \"\$incoming\" \"\$release\""
    printf 'Deploying static web assets on %s\n' "$DEPLOY_TARGET"
fi

ssh "${SSH_OPTIONS[@]}" "$DEPLOY_TARGET" bash -s -- "$MODE" "$EXPECTED_VERSION" <<'REMOTE_SCRIPT'
set -euo pipefail

export LC_ALL=C
export PATH="$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

MODE=$1
VERSION=$2
if [[ $MODE == full ]]; then
    SOURCE_DIR="$HOME/.local/src/optic-daemon-$VERSION"
else
    SOURCE_DIR="$HOME/.cache/optic-assets-deploy"
fi
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
[[ -d $SOURCE_DIR ]] || fail "staged source is missing: $SOURCE_DIR"

if [[ $MODE == full ]]; then
    [[ $(uname -m) == aarch64 ]] || fail 'deployment requires an AArch64 Raspberry Pi'
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
else
    for command in curl install systemctl; do
        command -v "$command" >/dev/null || fail "required Pi command is missing: $command"
    done
fi

CLIPPY_WRAPPER=""
if [[ $MODE == full ]]; then
    # The build (cargo test/clippy/build --release with LTO) is memory-heavy
    # enough on this 990MB-RAM Pi to have tripped the hardware watchdog before
    # under parallel compilation (see worklogs/2026-09-18-system-control-panel.md,
    # "Incidental Finding"). Stopping the daemon frees its RAM for the build and
    # avoids it holding the camera device while cargo test exercises it. This
    # on_exit handler is the single place responsible for getting it running
    # again — on success (via setup-optic-daemon-phase-01.sh's own restart), on
    # a verification-triggered rollback (which restarts it itself), and on any
    # other failure exit in between (the case this handler actually exists for).
    # None of this applies to the --assets fast path: it never touches the
    # binary or the service, so there's no RAM pressure to relieve and nothing
    # to restart.
    on_exit() {
        local status=$?
        [[ -z $CLIPPY_WRAPPER ]] || rm -f "$CLIPPY_WRAPPER"
        systemctl --user is-active --quiet optic-daemon.service || \
            systemctl --user start optic-daemon.service || true
        exit "$status"
    }
    trap on_exit EXIT

    printf '%s\n' '==> Stopping optic-daemon.service to free RAM for the build (dashboard/camera offline until reinstalled)'
    systemctl --user stop optic-daemon.service || true

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

    # Cargo suppresses its crate-count progress bar when stdout isn't a TTY,
    # which this script's piped/logged SSH output never is — leaving a bare
    # scroll of "Compiling X" lines with no sense of how far along a build is.
    # Forcing it on prints periodic "Building [===>] N/Total: crate" snapshot
    # lines instead, even over a non-interactive pipe.
    export CARGO_TERM_PROGRESS_WHEN=always
    export CARGO_TERM_PROGRESS_WIDTH=80

    pkg-config --modversion libcamera libcamera-base libturbojpeg
fi

printf '%s\n' '==> Linting web assets with Biome'
[[ -x $HOME/biome ]] || fail "Biome binary is missing: $HOME/biome"
cd "$SOURCE_DIR"
"$HOME/biome" check src/web/app.js src/web/index.html src/web/scheduler.js src/web/scheduler.html || \
    fail 'Web assets failed Biome validation'

if [[ $MODE == full ]]; then
    printf '%s\n' '==> Formatting and testing'
    rustup run "$RUST_TOOLCHAIN" cargo fmt --all -- --check
    LD_LIBRARY_PATH="$OPTIC_NATIVE_LIB" \
        rustup run "$RUST_TOOLCHAIN" cargo test --locked --all-targets

    printf '%s\n' '==> Running strict Clippy with isolated compiler libraries'
    CLIPPY_WRAPPER=$(mktemp "$HOME/.cache/optic-clippy-wrapper.XXXXXX")
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
    CLIPPY_WRAPPER=""

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
fi

printf '%s\n' '==> Installing with rollback protection'
backup_dir=$(mktemp -d "$HOME/.cache/optic-daemon-deploy.XXXXXX")
had_binary=false
had_unit=false
had_web_assets=false
if [[ $MODE == full ]]; then
    if [[ -f $HOME/.local/bin/optic-daemon ]]; then
        cp -p "$HOME/.local/bin/optic-daemon" "$backup_dir/optic-daemon"
        had_binary=true
    fi
    if [[ -f $HOME/.config/systemd/user/optic-daemon.service ]]; then
        cp -p "$HOME/.config/systemd/user/optic-daemon.service" "$backup_dir/optic-daemon.service"
        had_unit=true
    fi
fi
if [[ -d $HOME/.local/bin/web ]]; then
    cp -pr "$HOME/.local/bin/web" "$backup_dir/web"
    had_web_assets=true
fi

rollback() {
    printf '%s\n' 'Deployment verification failed; restoring the previous state.' >&2
    set +e
    if [[ $MODE == full ]]; then
        if [[ $had_binary == true ]]; then
            install -m 0755 "$backup_dir/optic-daemon" "$HOME/.local/bin/optic-daemon"
        else
            rm -f "$HOME/.local/bin/optic-daemon"
        fi
    fi
    rm -rf "$HOME/.local/bin/web"
    if [[ $had_web_assets == true ]]; then
        cp -pr "$backup_dir/web" "$HOME/.local/bin/web"
    fi
    if [[ $MODE == full ]]; then
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
    fi
    rm -rf "$backup_dir"
    exit 1
}

deploy_started=$(date --iso-8601=seconds)
if [[ $MODE == full ]]; then
    if ! ./scripts/setup-optic-daemon-phase-01.sh --binary "$BINARY"; then
        rollback
    fi

    if ! cmp -s "$BINARY" "$HOME/.local/bin/optic-daemon"; then
        printf '%s\n' 'Installed binary does not match the release artifact.' >&2
        rollback
    fi
else
    rm -rf "$HOME/.local/bin/web"
    install -d -m 0755 "$HOME/.local/bin/web"
    cp -r "$SOURCE_DIR/src/web/." "$HOME/.local/bin/web/"
    find "$HOME/.local/bin/web" -type d -exec chmod 0755 {} +
    find "$HOME/.local/bin/web" -type f -exec chmod 0644 {} +
fi

web_asset_diff=$(mktemp)
if ! diff -rq "$SOURCE_DIR/src/web" "$HOME/.local/bin/web" >"$web_asset_diff" 2>&1; then
    printf 'Installed web assets do not match the release artifact:\n' >&2
    cat "$web_asset_diff" >&2
    rm -f "$web_asset_diff"
    rollback
fi
rm -f "$web_asset_diff"

if [[ $MODE == full ]]; then
    if ! systemctl --user is-enabled --quiet optic-daemon.service; then
        printf '%s\n' 'optic-daemon.service is not enabled.' >&2
        rollback
    fi
fi
if ! systemctl --user is-active --quiet optic-daemon.service; then
    printf '%s\n' 'optic-daemon.service is not active.' >&2
    rollback
fi
if ! curl --fail --silent --show-error http://127.0.0.1:8000/healthz >/dev/null; then
    printf '%s\n' 'The deployed health endpoint failed.' >&2
    rollback
fi

if [[ $MODE == full ]]; then
    status=$(curl --fail --silent --show-error http://127.0.0.1:8000/api/status) || rollback
    deployed_version=$(python3 -c 'import json, sys; print(json.load(sys.stdin)["version"])' <<<"$status") || rollback
    if [[ $deployed_version != "$VERSION" ]]; then
        printf 'Expected API version %s, received %s.\n' "$VERSION" "$deployed_version" >&2
        rollback
    fi
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
if ! grep -F 'Manage rules' <<<"$index_html" >/dev/null; then
    printf '%s\n' 'The served dashboard is missing the scheduler summary link.' >&2
    rollback
fi

scheduler_html=$(curl --fail --silent --show-error http://127.0.0.1:8000/scheduler.html) || rollback
if ! grep -F 'Shot forecaster' <<<"$scheduler_html" >/dev/null; then
    printf '%s\n' 'The served scheduler page is missing expected content.' >&2
    rollback
fi
scheduler_js=$(curl --fail --silent --show-error http://127.0.0.1:8000/scheduler.js) || rollback
if ! grep -F 'schedule/forecast' <<<"$scheduler_js" >/dev/null; then
    printf '%s\n' 'The served scheduler script is missing expected content.' >&2
    rollback
fi

error_logs=$(journalctl --user -u optic-daemon.service --since "$deploy_started" \
    --priority err --quiet --no-pager -o cat || true)
if [[ -n $error_logs ]]; then
    printf '%s\n' "$error_logs" >&2
    rollback
fi

if [[ $MODE == full ]]; then
    rm -rf "$HOME/.local/src/optic-daemon"
    ln -s "optic-daemon-$VERSION" "$HOME/.local/src/optic-daemon"
fi
rm -rf "$backup_dir"

printf '%s\n' '==> Recent service log'
journalctl --user -u optic-daemon.service -n 20 --no-pager
if [[ $MODE == full ]]; then
    printf 'SUCCESS: optic-daemon %s is active at http://optic.local:8000/\n' "$VERSION"
else
    printf 'SUCCESS: static web assets deployed and verified at http://optic.local:8000/\n'
fi
REMOTE_SCRIPT
