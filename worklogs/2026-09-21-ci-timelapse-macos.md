# Dated Worklog: 2026-09-21 - CI: macOS Job for the Timelapse Tool

Status: **implemented and tested on GitHub Actions** (PR #5). The job is
not a required check.

## Objective

User request: `main`'s CI (`.github/workflows/ci.yml`) only builds
`optic-daemon` on a Linux arm64 runner. `tools/timelapse/` is a separate
Cargo workspace, so the root `cargo test`/`clippy`/`fmt` never touch it:
PR #5 showed two green checks while none of the tool's 45 tests ran. Add a
macOS job for the tool.

## Decisions (user, 2026-09-21)

1. **Trigger:** only when the tool changes. `scripts/ci-changed-scope.sh`
   gains a second output, `timelapse=true|false`, which fails open the same
   way `docs_only` does.
2. **ffmpeg:** `brew install ffmpeg`, and require the real encode and
   order integration test via `OPTIC_REQUIRE_FFMPEG=1` (a skip becomes a
   failure).
3. **Delivery:** rebase `feat/timelapse-builder` onto `main`, add the job
   there, and push to PR #5, so the PR's own checks exercise the job.
   Rebased onto `a56e01a` cleanly; 45/45 tests still pass locally.

Not changed: the ruleset's required checks (`Biome (web assets)`,
`Rust (Debian 13 arm64)`). The new check is non-required until the user
decides otherwise. Tool-only changes still run the daemon jobs (unchanged
behavior; possible follow-up).

## Acceptance Criteria

1. A PR or push that changes `tools/timelapse/**` (non-`.md`) runs the job
   `Timelapse tool (macOS)` on `macos-15`. It runs `cargo fmt --check`,
   `clippy --locked --all-targets -D warnings`, `test --locked
   --all-targets`, and `build --locked --release` in `tools/timelapse`,
   with ffmpeg installed and the encode test required.
2. It also runs when the CI definition that governs it changes
   (`.github/workflows/ci.yml`, `scripts/ci-changed-scope.sh`,
   `rust-toolchain.toml`).
3. It's skipped for daemon-only changes and docs-only changes.
4. Fail-open: if `changes` fails or has no diff range (new branch,
   `workflow_dispatch`, unknown `before`), the job runs.
5. `docs_only` behavior is byte-for-byte unchanged for existing inputs.
   The existing jobs are untouched.
6. The Rust toolchain comes from `rust-toolchain.toml` (1.98.1), like the
   arm64 job.

## Test Plan (before implementation)

1. Local: `bash -n` on the script. Run the classifier on file lists and
   compare both outputs:
   - docs-only
   - daemon-only (`src/web.rs`)
   - tool code (`tools/timelapse/src/plan.rs`)
   - tool `Cargo.lock`
   - tool `.md` only
   - mixed daemon plus tool
   - `.github/workflows/ci.yml`
   - `scripts/ci-changed-scope.sh`
   - `rust-toolchain.toml`
   - `case/` only
   - empty input
   - blank lines only
2. Local: the existing 11 docs-only cases from
   `worklogs/2026-09-21-ci-skip-docs-only.md` give the same `docs_only`
   values as before.
3. Local: parse `ci.yml` as YAML (Ruby's bundled `yaml`, as the previous
   CI worklog did) and check the new job's `needs`/`if`/`runs-on`/steps.
   Check `changes` exports `timelapse`.
4. Local: `OPTIC_REQUIRE_FFMPEG=1 cargo test` passes with ffmpeg present.
   With ffmpeg hidden from PATH, it fails with a clear message; without the
   variable, the test skips.
5. GitHub: pushing the rebased branch to PR #5 changes `tools/`, the
   workflow, and the script, so the macOS job must run and pass. Record the
   run URL, duration, and ffmpeg version. The two required checks must
   still pass.
6. Criteria 3 and 4 on GitHub (a daemon-only or docs-only PR skipping the
   job) are verified by the classifier cases plus review of the `if:`
   expression. No separate throwaway PR is planned.

## Implementation Summary

- `scripts/ci-changed-scope.sh`: now prints `docs_only=` (logic unchanged)
  and `timelapse=`. `timelapse` is true for any non-`.md` path under
  `tools/timelapse/`, or `.github/workflows/ci.yml`,
  `scripts/ci-changed-scope.sh`, or `rust-toolchain.toml`. Empty input also
  gives true.
- `.github/workflows/ci.yml`:
  - `changes` exports `timelapse`, and its no-diff fallback now emits
    `docs_only=false` plus `timelapse=true`.
  - New job `timelapse-macos` ("Timelapse tool (macOS)"): `macos-15`, the
    rustup channel from `rust-toolchain.toml`, `brew install ffmpeg`,
    `OPTIC_REQUIRE_FFMPEG=1`, and fmt/test/clippy/release build in
    `tools/timelapse`. It uses `Swatinem/rust-cache` (same pinned SHA) with
    `workspaces: tools/timelapse -> target`, and `timeout-minutes: 30`.
  - `web-lint` and `rust-arm64` are untouched.
- `tools/timelapse/tests/cli_e2e.rs`: the encode test panics instead of
  skipping when ffmpeg is missing and `OPTIC_REQUIRE_FFMPEG` is set.
- Docs:
  - `docs/optic-daemon-ci-cd.md`: status, worklog link, §5.1 (the
    `timelapse` output and the new job), and O3 (macOS billing).
  - `docs/timelapse-builder.md`: tests section and CI note.
- `CLAUDE.md` (`AGENTS.md` is a symlink to it):
  - added a `tools/` entry to the repository structure;
  - appended the zsh `$var:r` rule to Environment and Tooling Constraints,
    as its self-reflection rule requires (the failure is from
    `worklogs/2026-09-20-timelapse-builder.md`).

  This is a shared file, so there is a small merge-conflict risk with the
  other tracks.
- `worklogs/2026-09-20-timelapse-builder.md`: its status and the
  "AGENTS.md missing" and "no tools/ entry" items were updated. They were
  out of date after the rebase and push.

## Validation (local)

Environment: macOS 15.8 dev iMac, bash 3.2, Ruby's bundled `yaml`,
Homebrew cargo 1.98.1, and ffmpeg 9.0.2. actionlint, shellcheck, and yq
are not installed, so they were not run.

1. `bash -n scripts/ci-changed-scope.sh`: clean.
2. Classifier: **21/21 passed**. Each case compared both outputs, and
   `docs_only` was also compared with `origin/main`'s original script:
   - Docs-only, AGENTS+CLAUDE, `case/`, and tool `.md`-only gave
     `docs_only=true`, `timelapse=false`.
   - Daemon `src/web.rs`, `src/web/app.js`, mixed docs+src, root
     `Cargo.lock`, `verify.sh`, `.github/dependabot.yml`,
     `scripts/ci-install-build-deps.sh`, and `tools/other/x.rs` gave
     `docs_only=false`, `timelapse=false`.
   - Tool code, tool `Cargo.lock`, tool test plus docs, mixed daemon+tool,
     `ci.yml`, the scope script, and `rust-toolchain.toml` gave
     `docs_only=false`, `timelapse=true`.
   - Empty input and blank lines gave `docs_only=false`, `timelapse=true`.
   - Old and new `docs_only` agreed on every input.
3. `ruby -ryaml` parse of `ci.yml`:
   - Jobs are `changes, web-lint, rust-arm64, timelapse-macos`.
   - `changes.outputs` is `docs_only,timelapse`.
   - The new job has `needs: changes`, the expected `if:`, `macos-15`,
     `working-directory: tools/timelapse`, and the expected 9 steps.
   - Both existing jobs' `needs`/`if` are unchanged.
   - `bash -n` passes on the extracted scope and toolchain step scripts.
     The channel parses as `1.98.1` from `tools/timelapse`.
4. Extracted `changes` step, run with `bash --noprofile --norc -eo
   pipefail`: **7/7 as expected**.

   | Input | Result |
   |---|---|
   | PR #5 range (tool) | `timelapse=true` |
   | PR #6 range (health-alerts, daemon) | `docs_only=false timelapse=false` |
   | PR #3 range (docs) | `docs_only=true timelapse=false` |
   | PR #4 range (ci.yml plus script) | `timelapse=true` |
   | push with zero `before` | fallback `docs_only=false timelapse=true` |
   | push with unknown `before` | fallback `docs_only=false timelapse=true` |
   | `workflow_dispatch` | fallback `docs_only=false timelapse=true` |

5. `OPTIC_REQUIRE_FFMPEG`:
   - Set, with ffmpeg present: `cargo test --locked --all-targets` gave
     41 plus 4 passed.
   - Set, with ffmpeg hidden (`env -i PATH=/usr/bin:/bin`, test binary run
     directly): FAILED, `ffmpeg not on PATH but OPTIC_REQUIRE_FFMPEG is
     set`.
   - Unset, with ffmpeg hidden: `SKIPPED: ffmpeg not on PATH`, then pass.
6. `cargo fmt` and `cargo clippy --locked --all-targets -- -D warnings`
   in `tools/timelapse`: clean.

## Validation (GitHub Actions, PR #5, head `0b1e0d9`)

Both the `push` run (35574394334) and the `pull_request` run (35574397375)
passed every job.

| Job | Push run | PR run |
|---|---|---|
| Detect changed scope | pass, 6 s | pass, 7 s |
| Biome (web assets) | pass, 8 s | pass, 8 s |
| Rust (Debian 13 arm64) | pass, 1m47s | pass, 1m43s |
| **Timelapse tool (macOS)** | **pass, 29 s** | **pass, 31 s** |

- `changes` emitted `docs_only=false` and `timelapse=true`.
- The push-run log of the macOS job
  (`gh run view 35574394334 --job 106252885163 --log`) shows:
  - image `macos-15-arm64` (macOS 15.7.9);
  - rustup not preinstalled, so it was installed, then `1.98.1-aarch64-apple-darwin`
    (rustc 1.98.1);
  - `brew install ffmpeg` poured the `ffmpeg--9.0.1_1.arm64_sequoia`
    bottle, taking about 3 s including dependencies;
  - `cargo test --locked --all-targets`: 41 unit and 4 integration tests
    passed, no `SKIPPED` line (with `OPTIC_REQUIRE_FFMPEG=1` a skip would
    have failed), and the integration suite finished in 0.34 s;
  - clippy clean; release build finished in 3.59 s.

  The 29 s total is plausible: the crate has about 44 small dependencies.
- The runner's ffmpeg is 9.0.1 and the dev iMac's is 9.0.2. Both pass.
- Skip path: the commit recording these results changes only this
  worklog, so its CI run should skip all three build jobs. See below.
