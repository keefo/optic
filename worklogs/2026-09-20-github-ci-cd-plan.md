# Dated Worklog: 2026-09-20 - GitHub CI/CD Pipeline Research and Plan

Status: **planned** (research and design only). No workflow, script, Pi,
or GitHub-settings changes were made. Awaiting user review of the plan and
decisions D1–D4.

## Objective

Research and plan a GitHub CI/CD pipeline for `optic-daemon` that fits
the project's constraints:

- a private repo;
- an `aarch64` Debian 13 target with a pinned Raspberry Pi `libcamera`;
- a 1 GB Pi that cannot safely compile;
- no inbound network path to the Pi;
- the rule that deployment needs explicit approval.

## Acceptance Criteria (for this research task)

1. A design document exists in `docs/` covering CI gates, release
   artifacts, deployment, rejected alternatives, phased acceptance
   criteria, and open decisions.
2. The feasibility claims the design depends on are checked against live
   sources rather than assumed.
3. Nothing is implemented or deployed. Implementation happens in later
   worklogs, one per phase.

## Test Plan (written before drafting the plan)

1. Confirm repo visibility, because it decides billing and attestation
   availability.
2. Confirm the arm64 GitHub-hosted runners are usable in private repos.
3. Confirm that the exact pinned native packages exist in the Raspberry Pi
   and Debian `trixie` arm64 indexes.
4. Confirm the test suite has no hardware-only tests that would fail in CI.
5. Run fmt, tests and Clippy locally on macOS, to record the baseline of
   the non-Linux code path.

## Research Performed and Observed Results

| # | Check | Command / source | Result |
| --- | --- | --- | --- |
| 1 | Repo visibility | `curl -s -o /dev/null -w '%{http_code}' https://api.github.com/repos/keefo/optic` | `404` unauthenticated, so **private**. `gh` is not authenticated on this host (`gh auth login` prompt) |
| 2 | arm64 runners for private repos | GitHub changelog 2026-01-29, "arm64 standard runners are now available in private repositories"; GitHub runner reference | `ubuntu-24.04-arm` is usable, with 2 vCPU / 8 GB and billing against included minutes |
| 3a | RPi archive pins | `archive.raspberrypi.com/debian/dists/trixie/main/binary-arm64/Packages.gz` | `libcamera0.7`, `libcamera-dev`, `libcamera-ipa` = `0.7.2+rpt20260817-1`; pool file `libcamera-dev_0.7.2+rpt20260817-1_arm64.deb` listed |
| 3b | Debian pins | `deb.debian.org/debian/dists/trixie/main/binary-arm64/Packages.gz` | `libturbojpeg0-dev 1:2.1.5-4`, `clang-19` and `libclang-19-dev 1:19.1.7-3+b1` |
| 4 | Hardware-only tests | `rg '#\[ignore'` returns nothing; read `native_camera.rs` `mod tests` | Tests are pure-function or tempdir based. Whether they pass in a Linux container stays **unverified** until phase 1 CI runs |
| 5a | macOS fmt | `cargo fmt --all -- --check` | Pass |
| 5b | macOS tests | `cargo test --locked --all-targets` (scratch `CARGO_TARGET_DIR`) | `109 passed; 0 failed; 0 ignored` |
| 5c | macOS Clippy | `cargo clippy --locked --all-targets -- -D warnings` | Pass, no warnings. The whole fmt, test and Clippy sequence took 1m47s wall-clock on a cold scratch target |

Local toolchain: `cargo 1.98.1` and `rustc 1.98.1` from Homebrew, which
matches the pinned `RUST_VERSION`. Biome is not installed on the Mac.

## Mismatches Found

- `CLAUDE.md` imports `@AGENTS.md`, but `AGENTS.md` does not exist in this
  worktree. Recorded as open item O4 in the plan. It was not created,
  because its content is the user's to define.

## Files Changed

- `docs/optic-daemon-ci-cd.md` (new): the design plan.
- `worklogs/2026-09-20-github-ci-cd-plan.md` (new): this worklog.

## Limitations and Next Steps

- Not implemented. The first measured facts (cold and warm build minutes,
  whether the Clippy LLVM wrapper is needed in the container, and the test
  pass count in Linux) will come from phase 1.
- The CI-built binary has not been hardware-validated. Phase 2 needs
  explicit user approval to install it on the Pi.
- Waiting on user decisions D1–D4 (`docs/optic-daemon-ci-cd.md` §8).
