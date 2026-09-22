# Dated Worklog: 2026-09-21 - Post-Merge Drift Fixes (verify.sh Phase 6, Stale-Binary Deploy, 0.1.31, Stale Docs)

Status: **implemented, locally tested, and Phase 6 verified read-only on the
Pi**; deploy-script fix not yet exercised by a real Pi deploy.

## Objective

Fix the drift left after the parallel-track merges (PRs #2–#10), as found
by the design-doc review on 2026-09-21:

1. `verify.sh` Phase 6 still checks the retired shell transfer (the
   `optic-capture-transfer` timer and service units, and
   `/etc/optic/capture-transfer.conf`). `optic_sync` replaced it, and the
   timer is no longer installed, so Phase 6 always FAILs.
2. The deploy script can report SUCCESS while installing a **stale
   binary** (`worklogs/2026-09-20-health-alerts.md`, "First deploy reported
   SUCCESS but installed a stale binary"). `tar` restores the Mac's source
   mtimes, and the shared `CARGO_TARGET_DIR` can hold newer artifacts from
   another build, so cargo skips compiling.
3. `Cargo.toml` is still `0.1.30`, but PR #10's latency and preview
   changes were deployed at that version from a branch. So `0.1.30` names
   two different builds. Bump to `0.1.31` so `main` can be deployed as a
   distinct version.
4. Stale doc statements (review bucket C) and the out-of-date worklog
   statuses from PRs #9 and #10.

## Acceptance Criteria

1. `verify.sh` Phase 6 no longer references the retired units or config.
   It checks instead:
   - The `optic-daemon` user unit sets `OPTIC_SYNC_REMOTE_HOST` to the
     expected host, and does not set `OPTIC_SYNC_ENABLED=false`. The
     effective user and port (the unit's value, or the daemon defaults
     `admin`/`2222`) match the expected values.
   - `/api/status` reports sync `enabled: true` and `last_error: null`.
     A non-null error FAILs with the error text.
   - The legacy timer FAILs if it is still enabled or active, and PASSes if
     it is absent or disabled. This was changed from WARN during
     implementation; see Findings.
   - The existing key, known-hosts, receiver-probe, tmpfs and Beszel
     checks are unchanged.
2. Full deploys extract the source with fresh mtimes (`tar -m`), so cargo
   always rebuilds the crate. After `cargo build --release` the script
   FAILs if the binary is not newer than a marker created just before
   the build.
3. `Cargo.toml` and `Cargo.lock` are at `0.1.31`; there are no other version strings.
4. Every stale statement fixed cites the code or worklog that proves the
   current state; historical sections labelled as such are left alone.

## Test Plan (before implementation)

Local (macOS):

1. `bash -n verify.sh scripts/build-deploy-optic-daemon.sh`.
2. The new Phase 6 parsing logic (Environment parsing and sync-object
   extraction from `/api/status` JSON) is tested against sample inputs: a
   healthy status, `last_error` set, sync disabled, and Environment with
   and without port/user overrides. The daemon's compact JSON is produced
   from a real `SyncStatus` shape.
3. `cargo test` on macOS after the version bump (the lockfile must stay
   consistent with `--locked`).
4. CI on the PR: full build (non-docs files changed).

On the Pi (read-only; run the script over SSH via stdin, nothing is written):

5. `ssh liam@optic.local 'bash -s -- --phase 6 --no-color' < verify.sh`.
   Expect no FAIL from the retired checks, and the new sync checks to
   reflect the live daemon.

Not in this change: redeploying `main`. That is a separate step that needs
the user's approval, and it is the real test of criterion 2 (the Pi should
compile the crate, and the marker check should pass).

## Findings During Implementation

- **The retired shell transfer is still installed and running on the Pi.**
  `optic-capture-transfer.timer` is a *system* unit, enabled and active,
  and it fires about every 15 s. The 2026-09-20 check (and my later doc
  edit based on it) looked only at `systemctl --user` and wrongly recorded
  "no timer installed". Every run fails with
  `ssh: connect to host 192.168.0.231 port 2222: No route to host`, because
  `/etc/optic/capture-transfer.conf` still has the old IP. That has three
  consequences:
  - Each run writes about 5 journal lines, roughly 1,200 lines an hour, into
    the 16 MiB persistent journal. That pushes out older crash evidence.
  - `scripts/optic-capture-transfer.sh` ships and then `rm`s **every**
    non-hidden file in `/mnt/capture`, not just captures. Today that
    includes the daemon's `preview_config.json`.
  - If `192.168.0.231` ever answers again (for example, a DHCP lease is
    reused), it would start deleting that file and racing `optic_sync`.
  Because of this, the `verify.sh` check FAILs instead of WARNs. Disabling
  the timer is a privileged host change, so it was **not done** and is left
  to the user.
- The `optic-daemon` stop at 20:28 PDT, and the Pi reboot at about 20:31,
  were not caused by this work. They matched another deploy or session
  (clean exit status 0, several logins, then a fresh boot). Pi commands in
  this worklog waited until the daemon was back and were read-only.

## Implementation Summary

- `verify.sh` Phase 6 changes:
  - The `/etc/optic/capture-transfer.conf` target check is replaced by the
    `optic_sync` target from the unit's `Environment` (with daemon
    defaults `admin`/`2222`), and FAILs on `OPTIC_SYNC_ENABLED=false`.
  - The "timer active" check is inverted: the retired timer must not be
    running.
  - The "latest shell transfer succeeded" check is replaced by `optic_sync`
    status from `/api/status` (`enabled`, `last_error`, `connectivity`).
- `scripts/build-deploy-optic-daemon.sh` changes:
  - Full-mode extraction uses `tar -xzmf` (fresh mtimes).
  - A `.optic-build-started` marker is touched before `cargo build
    --release`, and the script FAILs unless the binary is newer than it.
- `Cargo.toml`/`Cargo.lock`: `0.1.30` → `0.1.31`.
- Docs, correcting the stale statements from the 2026-09-21 review:
  - `README.md`: OverlayFS is planned, not current; the native backend is
    deployed; the daemon's full scope is listed.
  - `docs/optic-daemon.md`:
    - The retired timer is still installed (this corrects my 2026-09-20
      edit).
    - The `optic_sync` allowlist is `testshot-`/`scheduler-`.
    - The "installed service remains on CLI backend" note is removed.
    - 16 missing endpoints are listed.
    - The `/api/config` note is corrected.
  - `docs/optic-daemon-scheduler.md`: all five constraint fields are
    implemented.
  - `docs/optic-daemon-capture-log.md`: the source is `web_ui`/`scheduler`
    with `triggered_by`. The §4 Phase 9 failure mode is corrected: writes
    are silently lost at reboot, not a loud failure.
  - `docs/optic-daemon-capture-performance.md`: the fix is merged (PR #10),
    and §4 item 1 is done except for the `camera.start()` gap.
  - `docs/optic-daemon-camera.md`, `docs/optic-daemon-alerts.md`,
    `docs/optic-daemon-build-environment.md`: status and paths updated.
  - `docs/optic-daemon-ci-cd.md`: D1, D3 and O4 are marked resolved.
  - `setup.md`: §1A is now persistent 16 MiB (matching the script); §6
    says `optic_sync` transfers and the retired timer must be disabled; the
    Phase 6 verification description is updated.
- Worklog statuses: `2026-09-20-capture-latency.md` and
  `2026-09-21-preview-controls.md` are marked merged in PR #10.
  `2026-09-21-ci-new-branch-diff-base.md` records GitHub run `35575800715`.

## Validation

| # | Check | Environment | Result |
|---|---|---|---|
| 1 | `bash -n verify.sh scripts/build-deploy-optic-daemon.sh` | macOS | pass |
| 2 | Sync-status parsing: healthy / `last_error` set / disabled / daemon down | macOS, sample `/api/status` JSON in the real `SyncStatus` field order, with nested objects before and after | 4/4 as expected |
| 3 | Unit-environment parsing: live Pi env / port override / old-IP host / no host / `OPTIC_SYNC_ENABLED=false` / `..._HOST_X` prefix trap | macOS | 6/6 as expected |
| 4 | Stale-binary repro: a tiny crate, a newer artifact from an "other session" in a shared target dir, and an edited source with an older mtime | macOS, cargo | `tar -xzf` ships the **old** binary and the marker check FAILs; `tar -xzmf` builds the new binary and the marker check passes |
| 5 | `cargo test --locked` after the bump | macOS | 158 passed, lockfile consistent |
| 6 | `ssh liam@optic.local 'bash -s -- --phase 6 --no-color' < verify.sh` (read-only, nothing written) | Pi, daemon `0.1.30` | PASS=12, FAIL=1. The new sync target check (`admin@imacpro.local:2222`) and the sync status check (`connectivity=idle`) PASS; the single FAIL is the retired timer, which really is running |
| 7 | CI on the PR | GitHub | pending |

Not run: actionlint, shellcheck (not installed), a real deploy.

## Remaining Limitations / Follow-up

- **User action needed:** disable the retired timer on the Pi
  (`sudo systemctl disable --now optic-capture-transfer.timer`). Optionally
  also remove `/etc/optic/capture-transfer.conf` and the units once
  confirmed. `setup-phase-06-pi-ram-transfer.sh` still installs and enables
  it, so a re-provision would bring it back. That needs a script change,
  not done here.
- The deploy-script fix is proven by the local repro but not yet by a
  real Pi deploy. The next `main` deploy (`0.1.31`, user-approved) should
  show a full test count, a real release build, and no "not rebuilt" FAIL.
- The Phase 6 "Capture transfer queue" INFO line counts every non-hidden
  file, so it reports `preview_config.json` (993 bytes) as queued. This is
  a pre-existing, informational-only check; it was left unchanged.
- The `/api/status` sync check relies on `SyncStatus` staying a flat
  object. A nested field would make it report "unavailable", which fails
  safe.

## User Verification

1. Review the PR diff.
2. After merge, approve a deploy of `main` (`0.1.31`).
3. Disable the retired timer.
4. Run `ssh liam@optic.local 'bash -s -- --phase 6' < verify.sh`. Expect no
   FAIL.
