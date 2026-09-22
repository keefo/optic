# `optic-daemon` Raspberry Pi Build Environment

This is the canonical native build and deployment runbook for `optic-daemon`.
Run these commands on `liam@optic.local`, not on the macOS development host.

The environment was last verified on 2026-09-16. The deploy script stages each
release's source under `/home/liam/.local/src/optic-daemon-<version>`.

## Validated Baseline

| Component | Validated value |
| --- | --- |
| Board and architecture | Raspberry Pi 5, `aarch64` |
| Operating system | Debian GNU/Linux 13 (`trixie`), Raspberry Pi repositories enabled |
| Rust toolchain | `stable-aarch64-unknown-linux-gnu` |
| `rustc` | `1.98.1` (`48a229cea`, LLVM `22.1.8`) |
| `cargo` | `1.98.1` |
| Clippy | `0.1.98` |
| GCC/G++ | Debian `14.2.0-19` |
| `pkg-config` | `1.8.1` |
| System camera runtime | `libcamera0.7` `0.7.2+rpt20260817-1` |
| Staged libcamera build files | `libcamera0.7` and `libcamera-dev` `0.7.2+rpt20260817-1` |
| Staged Clang/LLVM | `19.1.7-3+b1` |
| Staged TurboJPEG | `2.1.5-4` |

Rust is installed for `liam` under `/home/liam/.rustup` and
`/home/liam/.cargo`. Native development files that could not be installed
system-wide are extracted into this user-owned sysroot:

```text
/home/liam/.local/optic-sysroot
├── usr/include
└── usr/lib/aarch64-linux-gnu
    ├── pkgconfig
    ├── libclang-19.so.1
    ├── libLLVM.so.19.1
    ├── libcamera.so.0.7
    └── libturbojpeg.so.0
```

The installed daemon also needs the staged native runtime libraries. The
systemd unit supplies their directory through `LD_LIBRARY_PATH`; staged
libcamera is version-identical to the Raspberry Pi OS runtime.

## Why the Variables Must Be Separated

Two names caused misleading failures during the `0.1.1` build:

1. **Never export `SYSROOT`.** Clippy interprets that generic variable as the
   Rust compiler sysroot. Pointing it at `optic-sysroot` produces `E0463:
   can't find crate for std`. Use `OPTIC_SYSROOT` only.
2. **Do not expose the staged directory to the Clippy compiler process.** The
   staged Clang uses LLVM 19, while this Rust/Clippy toolchain uses LLVM 22.
   Clippy's build scripts still need LLVM 19 when regenerating bindings, so the
   automation script supplies it to Cargo but removes it in a compiler wrapper.

`bindgen`, which is used by `libcamera-sys`, has the opposite requirement: its
build script needs staged `libclang-19.so.1` and `libLLVM.so.19.1`. Therefore,
tests and releases run with the staged library path. For Clippy, use the
automation script's selective compiler wrapper; a plain `env -u
LD_LIBRARY_PATH cargo clippy` is suitable only when native build-script
artifacts are already current.

## Standard Build

The normal path is the zero-argument automation script, run from the macOS
project checkout. It uploads the source and performs every bootstrap, build,
installation, rollback, and verification step in this document:

```bash
cd /Users/admin/Documents/projects/optic
./scripts/build-deploy-optic-daemon.sh
```

Set `OPTIC_DEPLOY_TARGET=user@host` only when the Pi cannot be reached as
`liam@optic.local`. The manual commands below remain available for diagnosis.

The pinned Rust version comes from `rust-toolchain.toml` (`channel`), which
the script uploads and reads on the Pi. The Biome file list comes from
`biome.json` `files.includes`. GitHub Actions CI reads both files too, and
installs the same `required_packages` pins; see
[`optic-daemon-ci-cd.md`](optic-daemon-ci-cd.md). To change the toolchain or
the linted files, edit those files, not the script.

For a change touching only `src/web/*` (HTML/CSS/JS), use the fast path
instead — it skips the entire Rust bootstrap/test/clippy/build and never
stops or restarts `optic-daemon.service` (the daemon serves these files
straight from disk on every request, so nothing needs to be told a file
changed). It still lints with Biome, installs through the same
backup-and-rollback path, and verifies the served content over real HTTP
requests — just without the multi-minute build in between:

```bash
./scripts/build-deploy-optic-daemon.sh --assets
```

Use the full (no-flag) path for anything touching `src/*.rs`, `Cargo.toml`,
or `systemd/`.

Set the source directory to the versioned release staged on the Pi:

```bash
ssh liam@optic.local
cd /home/liam/.local/src/optic-daemon-<version>

export PATH="$HOME/.cargo/bin:$PATH"
export OPTIC_SYSROOT="$HOME/.local/optic-sysroot"
export OPTIC_NATIVE_LIB="$OPTIC_SYSROOT/usr/lib/aarch64-linux-gnu"
export PKG_CONFIG_PATH="$OPTIC_NATIVE_LIB/pkgconfig"
export PKG_CONFIG_SYSROOT_DIR="$OPTIC_SYSROOT"
export LIBCLANG_PATH="$OPTIC_NATIVE_LIB"
export BINDGEN_EXTRA_CLANG_ARGS="-I/usr/lib/gcc/aarch64-linux-gnu/14/include"
unset SYSROOT LD_LIBRARY_PATH

rustup component add rustfmt clippy
cargo fmt --all -- --check

LD_LIBRARY_PATH="$OPTIC_NATIVE_LIB" cargo test --locked --all-targets

# Let Clippy build scripts load LLVM 19, but keep it out of LLVM 22 compiler processes.
clippy_wrapper=$(mktemp)
cat >"$clippy_wrapper" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
sanitized=
old_ifs=$IFS
IFS=:
for directory in ${LD_LIBRARY_PATH:-}; do
  if [[ -n $directory && $directory != "${OPTIC_NATIVE_LIB:?}" ]]; then
    sanitized=${sanitized:+"$sanitized:"}$directory
  fi
done
IFS=$old_ifs
if [[ -n $sanitized ]]; then
  export LD_LIBRARY_PATH=$sanitized
else
  unset LD_LIBRARY_PATH
fi
exec "$@"
EOF
chmod 0700 "$clippy_wrapper"
LD_LIBRARY_PATH="$OPTIC_NATIVE_LIB" RUSTC_WRAPPER="$clippy_wrapper" \
  cargo clippy --locked --all-targets -- -D warnings
rm -f "$clippy_wrapper"

# bindgen needs staged libclang and LLVM while compiling native dependencies.
LD_LIBRARY_PATH="$OPTIC_NATIVE_LIB" cargo build --locked --release
```

The wrapper is required even after tests pass because Clippy can rerun native
build scripts with its own build fingerprint.

## Pre-deployment Checks

Keep the old service running until all checks below pass:

```bash
expected_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
test -n "$expected_version"
test -x target/release/optic-daemon
test -r "$OPTIC_NATIVE_LIB/libcamera.so"
test -r "$OPTIC_NATIVE_LIB/libturbojpeg.so.0"

LD_LIBRARY_PATH="$OPTIC_NATIVE_LIB" \
  ldd target/release/optic-daemon | tee /tmp/optic-daemon.ldd
! grep -q 'not found' /tmp/optic-daemon.ldd
grep -q 'libcamera.so.0.7' /tmp/optic-daemon.ldd
grep -q 'libturbojpeg.so.0' /tmp/optic-daemon.ldd

grep -aF "$expected_version" target/release/optic-daemon >/dev/null
grep -aF '${profile.previewFps} FPS' target/release/optic-daemon >/dev/null
```

The binary resolves the validated `libcamera.so.0.7` and `libturbojpeg.so.0`
copies from `OPTIC_NATIVE_LIB` under the service's `LD_LIBRARY_PATH`.

## Install and Verify

The installer copies the already-built release binary, verifies the required
runtime library and capture-stage prerequisites, checks that the installed
system unit matches `systemd/` (see `docs/optic-daemon-system-service.md`),
restarts the system service through PolicyKit (no sudo),
and waits until `/api/status` reports the version from `Cargo.toml`:

```bash
./scripts/setup-optic-daemon-phase-01.sh
```

After installation, verify both the API and the disk-served web asset:

```bash
expected_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
status=$(curl --fail --silent --show-error http://127.0.0.1:8000/api/status)
python3 -c 'import json, sys; print(json.load(sys.stdin)["version"])' <<<"$status" |
  grep -Fx "$expected_version"

curl --fail --silent --show-error http://127.0.0.1:8000/app.js |
  grep -F 'previewFps' >/dev/null
systemctl is-enabled optic-daemon.service
systemctl is-active optic-daemon.service
journalctl -u optic-daemon.service --since '-2 minutes' --no-pager
```

Finally, open [http://optic.local:8000/](http://optic.local:8000/) and confirm
the selected profile displays the expected FPS: Master Archive `2 FPS`, 4K DCI
`8 FPS`, and 2K Binning `8 FPS`.

## Rebuilding the User-local Native Sysroot

The validated package archives are cached in `/home/liam/.cache/optic-debs`.
To reconstruct the sysroot from that cache without root privileges:

```bash
export OPTIC_SYSROOT="$HOME/.local/optic-sysroot"
rm -rf "$OPTIC_SYSROOT"
mkdir -p "$OPTIC_SYSROOT"
for package in "$HOME/.cache/optic-debs"/*.deb; do
  dpkg-deb -x "$package" "$OPTIC_SYSROOT"
done
```

The cache contains the following AArch64 packages:

```text
libcamera0.7=0.7.2+rpt20260817-1
libcamera-dev=0.7.2+rpt20260817-1
libclang1-19=1:19.1.7-3+b1
libclang-common-19-dev=1:19.1.7-3+b1
libllvm19=1:19.1.7-3+b1
libz3-4=4.13.3-1
libjpeg62-turbo-dev=1:2.1.5-4
libturbojpeg0=1:2.1.5-4
libturbojpeg0-dev=1:2.1.5-4
```

If the cache is lost, download those packages from the configured Debian and
Raspberry Pi repositories, then extract them with the same `dpkg-deb -x`
loop. Keep related library and development-package versions together. If the
repository no longer carries these exact versions, update the baseline table
and rerun every validation command before deployment.

## Fast Failure Diagnosis

| Symptom | Cause | Fix |
| --- | --- | --- |
| Clippy reports `can't find crate for std` | `SYSROOT` points to the native package tree | `unset SYSROOT`; retain only `PKG_CONFIG_SYSROOT_DIR` |
| Clippy fails while staged LLVM is visible | LLVM 19 conflicts with Rust/Clippy LLVM 22 | Use the selective compiler wrapper above; do not remove the path from Clippy build scripts |
| `bindgen` cannot open `libclang` or reports missing `libLLVM.so.19.1` | The native build lacks its runtime search path | Run test/build with `LD_LIBRARY_PATH="$OPTIC_NATIVE_LIB"` |
| `bindgen` cannot find standard C headers | GCC builtin headers are absent from Clang's search | Set `BINDGEN_EXTRA_CLANG_ARGS` exactly as above |
| `pkg-config` cannot find libcamera or TurboJPEG | Staged `.pc` files are not visible | Check `PKG_CONFIG_PATH` and `PKG_CONFIG_SYSROOT_DIR` |
| Linker cannot find `-lcamera` or `-lcamera-base` | `libcamera-dev` symlinks have no runtime targets in the staged sysroot | Stage matching `libcamera0.7`; rerunning the automation script repairs this automatically |
| Built binary reports `libturbojpeg.so.0 => not found` | `ldd` was run without the staged runtime path | Repeat with `LD_LIBRARY_PATH="$OPTIC_NATIVE_LIB"`; the service unit sets this at runtime |
| Installer reports the previous API version | Service did not start the expected artifact | Inspect installer output and `journalctl`; do not declare deployment complete |

Useful environment checks:

```bash
rustup show active-toolchain
rustc --version --verbose
cargo --version
cargo clippy --version
gcc --version | head -n 1
pkg-config --version
pkg-config --modversion libcamera libcamera-base libturbojpeg
```