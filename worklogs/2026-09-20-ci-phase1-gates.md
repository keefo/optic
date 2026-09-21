# Dated Worklog: 2026-09-20 - CI Phase 1: GitHub Actions Gates

Status: **implemented and statically validated locally**.

- **Not yet run on GitHub.** AC5 and AC6 need this branch to be committed
  and pushed, which is waiting on user approval.
- Nothing was deployed. The Pi was not touched.

Plan: [`docs/optic-daemon-ci-cd.md`](../docs/optic-daemon-ci-cd.md) §7 phase 1.
Research: [`2026-09-20-github-ci-cd-plan.md`](2026-09-20-github-ci-cd-plan.md).
User decisions (2026-09-20): implement phase 1; D2 = add `rust-toolchain.toml`.

## Objective

Run the deploy script's gates on every push and PR, on GitHub-hosted runners:

- fmt;
- test;
- strict Clippy;
- Biome;
- the release build, with `ldd` and version checks.

The release binary is kept as a downloadable CI artifact. There is no
release or deploy step in this phase.

## Scope

1. `rust-toolchain.toml` (`channel = "1.98.1"`, with rustfmt and Clippy).
   The deploy script reads `RUST_VERSION` from it instead of hard-coding it.
2. The Biome file list moves into `biome.json` `files.includes`. The deploy
   script and CI both run Biome without explicit paths.
3. `scripts/ci-install-build-deps.sh` installs the native packages in a
   `debian:trixie` container:
   - it adds the Raspberry Pi archive with the key fingerprint pinned to
     `CF8A1AF502A2AA2D763BAE7E82B129927FA3303E`;
   - it installs the exact `required_packages` list parsed from
     `scripts/build-deploy-optic-daemon.sh`, so the pins exist in one place.
4. `.github/workflows/ci.yml` with two jobs, `web-lint` and `rust-arm64`.
5. `.github/dependabot.yml` for `github-actions` only.

Out of scope: `release.yml`, the `--release` deploy mode, any Pi changes,
and branch protection (D1).

## Acceptance Criteria

- AC1: `actionlint` reports no errors on `ci.yml`, and `bash -n` passes on
  the changed scripts.
- AC2: Biome 2.5.14 with the new `files.includes` checks exactly the nine
  files the script checked before (`app.js`, `index.html`, `scheduler.js`,
  `scheduler.html`, `capture-history.js`, `capture-history.html`,
  `config.js`, `config.html`, `footer.js`). It passes on the current tree
  and fails on a deliberately mis-formatted file.
- AC3: The `required_packages` parse yields the same nine pins the script
  uses.
- AC4: Parsing `rust-toolchain.toml` yields `1.98.1`. Local cargo still
  passes fmt, test and Clippy.
- AC5: **(requires a push, gated on user approval)**
  - A GitHub Actions run on this branch is green in both jobs.
  - The Linux test count is recorded.
  - The `ldd` and version checks pass and the artifact is downloadable.
  - Cold and warm durations are recorded.
- AC6 (negative, requires a push): a deliberate fmt violation turns the run
  red. Run it once, then revert.

## Failure Cases to Watch

- The RPi repo may pull in newer packages than the pins, causing an apt
  resolution conflict. The script fails loudly; it does not float versions.
- Clippy may fail with LLVM 19 vs 22 in the container. If so, port the
  deploy script's wrapper and record that.
- bindgen may be unable to find headers or libclang. `LIBCLANG_PATH` and
  `BINDGEN_EXTRA_CLANG_ARGS` mirror the Pi script.
- Tests may assume a host-specific environment such as tmpfs or `/proc`.

## Target Environments

- Local macOS: static checks, Biome, and cargo on the non-Linux code path.
- GitHub-hosted `ubuntu-24.04-arm` with a `debian:trixie` container: the
  real CI (AC5 and AC6).
- The Pi is **not** touched. Deploy-script edits are checked with `bash -n`
  and local extraction tests only. A real deploy is needed to hardware-verify
  them.

## Implementation Summary

Files changed:

- `rust-toolchain.toml` (new): `channel = "1.98.1"`, with the rustfmt and
  Clippy components and the minimal profile.
- `biome.json`:
  - adds `files.includes` with the nine web files;
  - adds a trailing newline. The file had none before, and Biome now checks
    its own config once no paths are passed, so the missing newline was a
    new failure.
- `scripts/build-deploy-optic-daemon.sh`:
  - uploads `rust-toolchain.toml` and requires it to be present;
  - on the Pi, reads `RUST_VERSION` from it and validates the format;
  - runs `"$HOME/biome" check` with no paths;
  - adds a comment documenting that CI parses `required_packages`.
- `scripts/ci-install-build-deps.sh` (new): the root-only installer for the
  arm64 trixie container. It:
  - pins the Raspberry Pi archive key fingerprint;
  - writes a deb822 source with `Signed-By`;
  - installs the parsed pins with `--print-packages` available for dry runs;
  - uses a read loop instead of `mapfile`, because macOS bash 3.2 has no
    `mapfile`.
- `.github/workflows/ci.yml` (new) has two jobs, `web-lint` and
  `rust-arm64`:
  - every action is pinned to a commit SHA: checkout v7.0.1
    `3d3c42e5…`, setup-biome v2.7.1 `4c91541e…`, rust-cache v2.9.2
    `6323deb1…`, upload-artifact v7.0.1 `043fb46d…`;
  - SHAs were resolved with `git ls-remote`, using the peeled `^{}` commit
    for annotated tags.
- `.github/dependabot.yml` (new): weekly grouped `github-actions` updates
  only.
- Docs: `docs/optic-daemon-ci-cd.md` (status, §2, §5.1, D2 resolved) and
  `docs/optic-daemon-build-environment.md` (the new single sources).

Design deviation from the plan: CI installs the **same** nine
`required_packages` pins as the Pi, with
`LIBCLANG_PATH=/usr/lib/aarch64-linux-gnu`, instead of the separately
listed `libclang-19-dev`. This keeps the Pi and CI on one pin list. The plan
doc was updated to match.

## Validation

Environment: the macOS development host. No Linux or arm64 container was
available locally: no Docker, and the `vmpi-builder` Lima VM is x86_64
without qemu-user.

| AC | Command | Observed |
| --- | --- | --- |
| AC1 | `actionlint` 1.7.12 with shellcheck 0.11.0 on `PATH` | First run: 2× SC2094 (info). The checksum step read and wrote `SHA256SUMS` in the same pipeline; fixed by writing to a temp file outside the hashed directory and then `mv`. Rerun: exit 0 |
| AC1 | `shellcheck scripts/ci-install-build-deps.sh` | SC1091 (info) on `. /etc/os-release`; added a `source=/dev/null` directive. Rerun: exit 0 |
| AC1 | `bash -n` on both scripts | OK. The deploy script's shellcheck finding count is unchanged vs `HEAD` (1 existing) |
| AC2 | Biome 2.5.14 (`npx @biomejs/biome@2.5.14`) `check --verbose` | The processed set is exactly the nine listed files, plus `biome.json` |
| AC2 | Baseline: `HEAD` `biome.json` with the old explicit nine paths | `Checked 9 files … No fixes applied` |
| AC2 | New `biome check` / `biome ci` before the newline fix | 1 error: `biome.json` format (missing trailing newline) |
| AC2 | New `biome check` / `biome ci` after the fix | `Checked 10 files`, exit 0 |
| AC2 | Negative test: a copy with a mis-formatted line appended to `footer.js` | `biome ci` exit 1, `biome check` exit 1 |
| AC3 | `scripts/ci-install-build-deps.sh --print-packages` | The nine pins, identical to `required_packages`. `--bogus` gives exit 1; running as non-root gives exit 1 "must run as root" |
| AC3 | Each pin is present in the Debian trixie and RPi trixie arm64 `Packages` indexes | 9/9 OK |
| AC4 | `sed` parse of `rust-toolchain.toml` | `1.98.1` |
| AC4 | Local `cargo fmt --check`, `cargo test --locked --all-targets`, `cargo clippy … -D warnings` (Homebrew cargo 1.98.1, which ignores `rust-toolchain.toml`) | fmt OK; `109 passed; 0 failed`; Clippy clean |
| AC5 | GitHub Actions run | **Not run.** Needs commit and push approval |
| AC6 | Negative CI run | **Not run** |

The RPi archive key fingerprint was computed from
`raspberrypi.gpg.key` (sha256 `76603890…6d56`) with a Python OpenPGP v4
fingerprint calculation, because no `gpg` was available on the Mac or the
VM. Primary key: `CF8A1AF502A2AA2D763BAE7E82B129927FA3303E`, uid
"Raspberry Pi Archive Signing Key".

## Failures Encountered

- `mapfile: command not found` under the macOS bash 3.2. Replaced with a
  `while read` loop. `CLAUDE.md` asks for such findings to be recorded in
  `AGENTS.md`, but that file does not exist in this checkout (plan O4), so
  the finding is recorded here instead.
- Biome's config-file check exposed the missing trailing newline in
  `biome.json`. Fixed as described above.

## Limitations and Next Steps

- **AC5 and AC6 are unverified.** The whole Linux path is exercised only by
  a real GitHub run:
  - installing the container dependencies;
  - bindgen;
  - whether the Clippy LLVM wrapper is needed;
  - the Linux test count;
  - `ldd`.
  Next: commit and push this branch (user approval), watch the run, record
  the durations, and fix any failures.
- The deploy-script edits (reading `RUST_VERSION` from the file, Biome with
  no paths) have **not** run on the Pi. The next real `build-deploy` run,
  full or `--assets`, will exercise them. Plan item O1: check that the Pi's
  `~/biome` is a 2.x version that supports `files.includes`.
- Nothing was deployed; phase 2 (a hardware test of the CI artifact) needs
  explicit approval.
