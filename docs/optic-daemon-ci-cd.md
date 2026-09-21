# `optic-daemon` GitHub CI/CD Pipeline — Design Plan

Status (2026-09-20):

- Phase 1 (CI gates) is **implemented and tested on GitHub Actions**.
  The cold run took 4m35s and a warm run 1m45s; 116 Linux tests pass. The
  CI binary has not been hardware-validated yet (phase 2).
- Phases 2–4 are planned.
- 2026-09-21: added a macOS job for the separate `tools/timelapse`
  workspace (§5.1, `timelapse-macos`).

Worklogs:

- research: [`2026-09-20-github-ci-cd-plan.md`](../worklogs/2026-09-20-github-ci-cd-plan.md);
- phase 1: [`2026-09-20-ci-phase1-gates.md`](../worklogs/2026-09-20-ci-phase1-gates.md);
- timelapse macOS job: [`2026-09-21-ci-timelapse-macos.md`](../worklogs/2026-09-21-ci-timelapse-macos.md).

## 1. Goals and Non-goals

Goals:

1. Every push and pull request runs the same gates the deploy script runs
   today (`cargo fmt --check`, `cargo test --locked --all-targets`, strict
   Clippy, the Biome lint, and the release build with `ldd` and version checks). Today those
   gates run only on the Pi itself, during a deployment.
2. Build the release binary **once, in CI, for the Pi's exact ABI**, so that
   deploying no longer compiles on the 1 GB Pi. On-Pi builds have tripped the
   hardware watchdog before (`worklogs/2026-09-18-system-control-panel.md`),
   and the daemon has to be stopped for the whole build to free RAM.
3. Keep deployment an explicit human action with the existing
   backup/verify/rollback path. `CLAUDE.md` forbids deployment without
   explicit approval, and a device meant to run unattended for 365 days
   should not receive unattended pushes.

Non-goals: automatic deployment on merge; hardware-in-the-loop camera
tests; changing the runtime layout on the Pi (`~/.local/bin/optic-daemon`,
`~/.local/bin/web`, the staged sysroot for `LD_LIBRARY_PATH`).

## 2. Constraints That Shape the Design

| Constraint | Consequence |
| --- | --- |
| Repo `keefo/optic` is **private** (the unauthenticated API returns 404) | Minutes are billed against the plan quota. Artifact attestations for private repos need GitHub Enterprise Cloud, so use SHA-256 checksums instead |
| Target is Raspberry Pi OS / Debian 13 `trixie`, `aarch64`, glibc from trixie | Build in a `debian:trixie` container, not on the Ubuntu host image |
| `libcamera` is the Raspberry Pi fork `0.7.2+rpt20260817-1`, available only from `archive.raspberrypi.com` | Add that apt repo in the container and pin the exact versions from `required_packages` in `scripts/build-deploy-optic-daemon.sh` |
| `libcamera-sys` runs `bindgen`, so it needs libclang 19 | Install the same pinned packages system-wide in the container (`libclang1-19` and friends), with `LIBCLANG_PATH=/usr/lib/aarch64-linux-gnu`. The Pi-only `LD_LIBRARY_PATH` / Clippy-wrapper workaround exists because the Pi has no root-installed clang. It is not used in CI; whether CI needs it is confirmed by the first run |
| Pi is on a home LAN (`optic.local`, mDNS) with no inbound access | GitHub cannot push to it. Deployment is **pull-based**, run from the Mac |
| Toolchain pinned to Rust `1.98.1` | `rust-toolchain.toml` is the single source (D2). CI and the deploy script both read its `channel` |
| Biome schema pinned to `2.5.14` in `biome.json` | CI uses Biome `2.5.14`. The version of `~/biome` on the Pi is unrecorded (open item O1) |

## 3. Feasibility Findings (2026-09-20)

- GitHub's `ubuntu-24.04-arm` standard runners are available in private
  repositories as of 2026-01-29. They have 2 vCPU and 8 GB RAM, count
  against the plan's included minutes, and Linux 2-core arm64 lists at
  $0.005/min. This makes a native arm64 build possible with no QEMU and no
  cross-linker.
- The exact pinned packages are available today:
  - `archive.raspberrypi.com/debian trixie main` carries `libcamera0.7`,
    `libcamera-dev`, and `libcamera-ipa` at `0.7.2+rpt20260817-1`.
  - `deb.debian.org trixie main` carries `libturbojpeg0-dev 1:2.1.5-4`,
    `clang-19` and `libclang-19-dev 1:19.1.7-3+b1`.
- All Linux-only code is behind `cfg(target_os = "linux")`. The unit tests
  in `native_camera.rs` are pure functions, and no test is `#[ignore]`d. The
  test suite is therefore expected to pass without a camera, but this is
  confirmed only once CI runs (phase 1 acceptance).
- On the macOS host, the non-Linux code path passes fmt, tests and Clippy
  locally. The results are in the worklog.

## 4. Pipeline Overview

```text
push / PR ─► ci.yml
             ├─ web-lint        ubuntu-24.04     Biome 2.5.14 `biome ci` on src/web
             └─ rust-arm64      ubuntu-24.04-arm, container debian:trixie
                  fmt → test → clippy -D warnings → release build
                  → ldd / version checks → upload artifact (14 days)

tag vX.Y.Z ─► release.yml
             ├─ verify tag == Cargo.toml version
             ├─ reuse the rust-arm64 job (workflow_call)
             └─ draft GitHub Release:
                  optic-daemon-X.Y.Z-aarch64-linux-gnu.tar.gz + SHA256SUMS

Mac (human) ─► ./scripts/build-deploy-optic-daemon.sh --release X.Y.Z
             gh release download → sha256 verify → scp to Pi
             → existing install / verify / rollback (no toolchain, no build)
```

## 5. Workflow Details

### 5.1 `.github/workflows/ci.yml`

- Triggers: `push` on all branches, `pull_request` targeting `main`, and
  `workflow_dispatch`. A `concurrency` group per ref cancels superseded runs.
- `permissions: contents: read` at the top level. Every third-party action
  is pinned to a full commit SHA with a version comment.
- **`changes`** ("Detect changed scope") runs first. It diffs the PR
  (base...head) or push (`before..sha`) and pipes the file list to
  `scripts/ci-changed-scope.sh`. When **every** changed path is
  documentation (`*.md`, `docs/**`, `worklogs/**`, `case/**`), it outputs
  `docs_only=true` and both jobs below are skipped with
  `if: !cancelled() && needs.changes.outputs.docs_only != 'true'`. GitHub
  counts a required check skipped this way as passing, so the `main`
  ruleset's required checks (`Biome (web assets)`,
  `Rust (Debian 13 arm64)`) need no change and docs-only PRs merge
  without a build. Workflow-level `paths-ignore` is deliberately not used:
  the required checks would never report and block the PR. The step fails
  open: a new branch, `workflow_dispatch`, an unknown `before` commit, or
  a failed `changes` job all run the full CI (added 2026-09-21,
  `worklogs/2026-09-21-ci-skip-docs-only.md`). The script also prints
  `timelapse=true|false`, which gates `timelapse-macos` below. It is true
  when any non-`.md` path is under `tools/timelapse/`, or is
  `.github/workflows/ci.yml`, `scripts/ci-changed-scope.sh`, or
  `rust-toolchain.toml`. Empty input and every fallback case give
  `timelapse=true`, so it fails open too.
- **`web-lint`** uses `biomejs/setup-biome` with version `2.5.14`, then runs
  `biome ci`. The checked file list lives in `biome.json` `files.includes`.
  The deploy script runs `biome check` with no paths, so both use the same
  list. Biome also checks `biome.json` itself.
- **`rust-arm64`**:
  1. `runs-on: ubuntu-24.04-arm`, `container: debian:trixie`.
  2. Run `scripts/ci-install-build-deps.sh`. It:
     - installs the base tools;
     - adds the Raspberry Pi archive, with its key taken from the
       `raspberrypi-archive-keyring` `.deb` (pinned by SHA-256) and checked
       against the fingerprint `CF8A1AF502A2AA2D763BAE7E82B129927FA3303E`.
       The loose `raspberrypi.gpg.key` cannot be used: its SHA-1 binding
       signatures are rejected by Debian 13's `sqv` since 2026-02-01;
     - installs **exactly** the `required_packages` pins parsed from
       `scripts/build-deploy-optic-daemon.sh`, so the pins exist in one
       place.
  3. Install rustup, then the `channel` from `rust-toolchain.toml` with
     `rustfmt` and `clippy`.
  4. Set `LIBCLANG_PATH=/usr/lib/aarch64-linux-gnu` and
     `BINDGEN_EXTRA_CLANG_ARGS=-I$(gcc -print-file-name=include)`,
     mirroring the Pi build.
  5. Cache with `Swatinem/rust-cache` (registry plus `target/`), keyed on
     `Cargo.lock` and the toolchain.
  6. Run `cargo fmt --all -- --check`, then
     `cargo test --locked --all-targets`, then
     `cargo clippy --locked --all-targets -- -D warnings`, then
     `cargo build --locked --release`.
  7. Run the same post-build checks as the script: `ldd` shows no
     `not found`, the binary links `libcamera.so.0.7` and
     `libturbojpeg.so.0`, and the `Cargo.toml` version string is embedded.
  8. Stage `optic-daemon`, `web/` (from `src/web`), `optic-daemon.service`,
     `VERSION` and `COMMIT`, write `SHA256SUMS`, and upload with
     `actions/upload-artifact` (`retention-days: 14`).
  9. `timeout-minutes: 60`.
- **`timelapse-macos`** ("Timelapse tool (macOS)", added 2026-09-21).
  `tools/timelapse` is its own Cargo workspace (a Mac-only tool,
  `docs/timelapse-builder.md`), so `rust-arm64`'s root `cargo` commands
  never build or test it.
  1. `runs-on: macos-15`.
  2. Runs `if: !cancelled() && needs.changes.outputs.timelapse != 'false'`,
     so daemon-only and docs-only changes skip it.
  3. Not a required check. Tool-only changes still run `rust-arm64` and
     `web-lint`.
  4. Installs the `rust-toolchain.toml` channel with rustup, then
     `brew install ffmpeg`.
  5. `OPTIC_REQUIRE_FFMPEG=1` makes the ffmpeg encode and frame-order
     integration test fail instead of skipping when ffmpeg is missing.
  6. In `tools/timelapse`, runs `cargo fmt --all -- --check`,
     `cargo test --locked --all-targets`,
     `cargo clippy --locked --all-targets -- -D warnings`, and
     `cargo build --locked --release`.
  7. `Swatinem/rust-cache` with `workspaces: tools/timelapse -> target`.
  8. No artifact is uploaded. `timeout-minutes: 30`.

### 5.2 `.github/workflows/release.yml`

- Trigger: a tag push matching `v*.*.*`. The first step fails unless
  `${GITHUB_REF_NAME#v}` equals the `Cargo.toml` version.
- Calls the `rust-arm64` job from `ci.yml` as a reusable workflow, so the
  build recipe exists only once.
- Packages `optic-daemon-<ver>-aarch64-linux-gnu.tar.gz` and `SHA256SUMS`,
  and creates a **draft** release with `gh release create --draft`. This job
  needs `permissions: contents: write`. A human publishes the release after
  reviewing it.

### 5.3 Deploy: `build-deploy-optic-daemon.sh --release <version>`

This is a new third mode next to `full` and `--assets`. It is still run by
hand from the Mac:

1. `gh release download v<ver>` into a temp dir, then
   `shasum -a 256 -c SHA256SUMS`.
2. Upload the tarball to `~/.local/src/optic-daemon-<ver>` on the Pi, using
   the same atomic `.incoming` directory then `mv` as today.
3. On the Pi, skip the rustup bootstrap, the sysroot rebuild and the cargo
   steps. Keep these checks:
   - staged runtime libs are present (`libturbojpeg.so.0`,
     `libcamera.so.0.7`);
   - `ldd` of the downloaded binary under the staged `LD_LIBRARY_PATH`;
   - the version string is embedded.
4. Reuse the existing install → `cmp` → web diff → `/healthz` →
   `/api/status` version → served-content checks → journal error check →
   rollback path unchanged. Downtime shrinks from "whole build" to
   "restart only", and the daemon no longer needs stopping to free RAM.

The existing no-flag mode (building on the Pi) stays as a fallback for
offline or emergency use.

## 6. Alternatives Considered

| Option | Verdict |
| --- | --- |
| Self-hosted runner on the Pi | **Rejected.** It needs a resident agent on a 1 GB device, and job work on an OverlayFS root. Arbitrary workflow code would run on the production camera, and builds there have already tripped the watchdog |
| GitHub-hosted runner pushes over Tailscale/SSH | **Rejected for now.** It puts a Pi SSH key in GitHub secrets, adds a new network dependency to the Pi, and pushes to deploy automatically, which conflicts with the approval rule |
| Self-hosted runner on the Mac plus a `workflow_dispatch` deploy behind an `environment` approval | **Deferred.** It gives one-click deploys from the GitHub UI, but keeps a runner resident on the Mac. Revisit once the pull-based flow is proven |
| Cross-compile on x86_64 with Debian multiarch `arm64` dev packages | **Fallback** if arm64 runners become unavailable. Tests would need `qemu-user` |
| Prebuilt builder image in GHCR | **Phase 4 option.** Protects against the RPi archive dropping pinned versions and removes apt time from every run |

## 7. Implementation Phases and Acceptance Criteria

Each phase gets its own dated worklog, per `CLAUDE.md`.

1. **CI gates** (`ci.yml`, a shared Biome file list, and `.github/dependabot.yml`
   for `github-actions` only)
   - Acceptance: a green run on a branch push; a deliberately bad fmt,
     Clippy or Biome change turns the run red; the test count matches a
     local run; `ldd` and version checks pass; the artifact downloads.
   - Must record: cold and warm run minutes, and whether the Clippy wrapper
     was needed.
2. **Artifact equivalence on hardware**
   - Install the CI-built binary on the Pi with the existing installer,
     after explicit user approval.
   - Acceptance: `/api/status` version matches; preview, test shot and
     scheduler all work; there are no journal errors; a soak period passes
     that the user chooses.
3. **Release plus `--release` deploy mode** (`release.yml` and the script mode)
   - Acceptance: a tag with the wrong version fails the release; a correct
     tag produces a draft release; a tampered tarball fails the checksum
     check; a forced verification failure rolls back.
4. **Hardening** (optional)
   - A GHCR builder image, a weekly scheduled `cargo deny check advisories`,
     and a Dependabot entry for `cargo` (grouped, PR-only, human-merged,
     because it changes `Cargo.lock`).

## 8. Decisions Needed From the User

- **D1**: Protect branch `main` so the `ci.yml` checks must pass before
  merge. This is a GitHub setting that the user controls.
- **D2** (resolved 2026-09-20): add `rust-toolchain.toml`. The Mac's
  Homebrew `cargo` ignores it; rustup-managed `cargo` honours it.
- **D3**: Whether a macOS job is worth its cost. macOS minutes cost about
  10× Linux on private repos. The recommendation is to keep the macOS check
  local.
- **D4**: Release cadence. The options are tag every version bump, or tag
  only builds that are candidates for deployment.

## 9. Open Items and Risks

- **O1**: Record the Pi's `~/biome --version`. If it differs from 2.5.14,
  align the Pi and CI.
- **O2**: The RPi archive may remove `0.7.2+rpt20260817-1`. CI would then
  fail loudly, just as the Pi bootstrap does. The mitigation is phase 4
  (GHCR image, or cached `.deb`s).
- **O3** (measured 2026-09-20): a Rust job takes about 4.6 min on a cold
  cache and about 1.8 min on a warm one, plus about 5 s for Biome. That is
  roughly 2–5 billed minutes per push. `timelapse-macos` runs only when
  the tool or the CI definition changes, because macOS minutes are billed
  at about 10× Linux on this private repo.
- **O4**: `CLAUDE.md` imports `@AGENTS.md`, but no `AGENTS.md` exists in
  this checkout, so the tooling-constraints section it references is
  missing.
