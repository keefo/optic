# Dated Worklog: 2026-09-19 - Stop `optic-daemon` During Build to Free RAM

Status: **implemented, syntax-verified, not yet exercised on a real
deploy run** — user was mid-reboot-test on the Pi when this was made, so
a live end-to-end run was deliberately deferred to avoid racing with
that.

## Objective

User's concern: build tasks (`scripts/build-deploy-optic-daemon.sh`,
which runs `cargo test`/`clippy`/`build --release` with LTO on the Pi)
keep using enough RAM to cause problems on this 990MB-RAM Pi. Prior
concrete evidence: `worklogs/2026-09-18-system-control-panel.md`'s
"Incidental Finding" — heavy parallel compilation twice tripped the
hardware watchdog and reset the board mid-build, root-caused via `dmesg`
and fixed at the time by raising `RuntimeWatchdogSec` to 90s (a
mitigation, not a reduction in memory pressure itself).

Ask: stop `optic-daemon.service` before the build task, to free its RAM
for the build.

## Acceptance Criteria

- The build script stops `optic-daemon.service` before the
  memory-intensive steps (toolchain bootstrap through
  `cargo build --release`).
- The daemon is guaranteed to end up running again by the time the script
  exits, regardless of *where* it exits — normal success, a
  verification-triggered rollback, or a build/test/lint failure partway
  through (the case not already covered by existing logic).
- No change to what's actually validated or installed; this is purely
  about resource use during the build window.

## Test Plan (written before implementation)

- Not unit-testable (it's a deploy shell script, no test harness exists
  for it in this project).
- `bash -n` on the outer script, and separately on the extracted
  remote heredoc body (the outer parser does not syntax-check heredoc
  content — confirmed this needs a separate extraction to actually catch
  errors inside it).
- Trace all exit paths by inspection: `fail()` calls (env/prereq checks,
  sysroot rebuild failures), an uncaught `set -e` abort (e.g. `cargo test`
  failing directly), `rollback()` (post-build verification failures), and
  the normal success path — confirm each one leaves the daemon running.
- Real validation (deferred, needs a live deploy run): confirm the daemon
  actually stops right after the prerequisite checks, stays stopped
  through the build, and is active again with the correct version by the
  time the script prints `SUCCESS`.

## Implementation Summary

- `scripts/build-deploy-optic-daemon.sh`, remote heredoc section:
  - After the prerequisite checks, added `systemctl --user stop
    optic-daemon.service || true` (the `|| true` guards a fresh box with
    no unit installed yet).
  - Added a single `on_exit()` function registered via `trap on_exit
    EXIT`, right before the stop command: it ensures the daemon is
    running (`is-active --quiet || start`) and preserves the script's
    real exit status (`local status=$?` captured first, `exit "$status"`
    last). This is the one thing that fixes the actual gap: an
    unhandled `set -e` abort during `cargo test`/`clippy`/`build`
    previously exited with the daemon down and nothing to bring it back.
  - Folded the pre-existing clippy-wrapper temp-file cleanup into the
    same shared `CLIPPY_WRAPPER` variable/trap instead of the old
    separate `trap 'rm -f "$CLIPPY_WRAPPER"' EXIT` / `trap - EXIT` pair —
    that pair would otherwise silently overwrite (during clippy) and
    then permanently clear (right after) the new daemon-restart trap,
    losing the safety net for the rest of the script (including the
    heaviest step, the LTO release build).

## Validation

- `bash -n scripts/build-deploy-optic-daemon.sh` — clean.
- Extracted the remote heredoc body (`sed` between the `<<'REMOTE_SCRIPT'`
  markers) and ran `bash -n` on it separately — clean. (Necessary because
  the outer script's heredoc is opaque to the outer parser.)
- `shellcheck` not available in this environment; not run.
- **Not yet run for real** — no live deploy was triggered this session
  after this change, since the user was actively testing the reboot fix
  on the same Pi at the time and a build would have raced with that
  (SSH connection + `sudo`/systemd state during a reboot cycle).

## Remaining Limitations / Follow-up

- Not empirically confirmed on hardware yet: that the daemon actually
  stops/restarts correctly around a real build, and that this measurably
  helps (e.g., peak RSS during the build, or simply "no more watchdog
  resets under heavy compilation"). Next deploy run will be the first
  real exercise of this path — worth watching its output for the new
  `==> Stopping optic-daemon.service...` line and confirming the final
  `SUCCESS` line still reports the daemon active at the expected version.
- Dashboard and camera are unavailable for the full build duration now
  (previously they stayed up throughout, since the daemon was rebuilt in
  place and only briefly restarted at install time). This is the
  intended tradeoff per the user's request, not an oversight, but worth
  surfacing here explicitly since it's a behavior change to every future
  deploy, not just this one.
