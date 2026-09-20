# Dated Worklog: 2026-09-19 - Fix `POST /api/system/reboot` Failing

Status: **Complete and user-verified on real hardware.** Round 1 fix
(drop `NoNewPrivileges=true`) turned out to be necessary but insufficient
— the button failed again with the same generic error text after a real
reboot, for a different underlying reason. Round 2 root-caused and fixed
(switch to `systemctl reboot` over D-Bus + a scoped polkit rule, keep all
filesystem sandboxing, restore `NoNewPrivileges=true`), deployed as
0.1.26, and the user rebooted the Pi via the button: fresh boot confirmed,
daemon back up automatically, sandboxing intact, polkit rule present,
`/healthz` ok.

## Objective

User reported clicking the "Reboot Pi" dashboard button produces:

```
System action failed: reboot command exited with exit status: 1
```

Diagnose why `reboot_host()` (`src/system_status.rs:222`, `sudo systemctl
reboot`) fails, and fix it.

## Round 1: `NoNewPrivileges` (Necessary, Turned Out Insufficient)

### Root Cause 1

`systemd/optic-daemon.service` sets `NoNewPrivileges=true` (part of the
sandboxing added across earlier hardening work). This flag categorically
blocks any privilege escalation via `execve()`, which includes `sudo` —
`sudo` needs its setuid-root bit to gain root, and `NoNewPrivileges`
disables setuid/setgid/file-capability effects for the whole process tree.
So `sudo systemctl reboot`, run from inside the daemon, never gets a
chance to authenticate; `sudo` itself refuses immediately.

This was a gap in the original System Control Panel feature
(`worklogs/2026-09-18-system-control-panel.md`): that work added
passwordless sudo (`/etc/sudoers.d/liam-nopasswd`, `NOPASSWD: ALL`) and
confirmed it works from an interactive SSH shell, but never checked it
against the daemon's own `NoNewPrivileges=true` sandbox. `restart-daemon`
(the other B-scope action) needs no privilege escalation, so it worked and
masked the gap — that's consistent with the prior worklog's status line
("B implemented and deployed, functionally untested").

### Diagnosis 1 (performed before any code change)

- `ssh liam@optic.local 'sudo -n -l'` — confirmed the sudoers grant is
  correctly in place and usable from a normal shell (`(ALL) NOPASSWD: ALL`).
- `ssh liam@optic.local 'systemctl --user show optic-daemon.service -p
  NoNewPrivileges'` → `NoNewPrivileges=yes`.
- Reproduced the exact failure non-destructively, without touching the
  real daemon or rebooting anything, by running `sudo -n true` (a no-op)
  inside a scratch systemd transient unit with the same flag:
  ```
  sudo systemd-run --pipe --wait --collect -p NoNewPrivileges=yes \
      --uid=liam --gid=liam -- sudo -n true
  ```
  Result: exit status 1, with sudo's own diagnostic —
  ```
  sudo: The "no new privileges" flag is set, which prevents sudo from running as root.
  sudo: If sudo is running in a container, you may need to adjust the container configuration to disable the flag.
  ```
  This matches the reported error exactly (same exit status, same
  `Command::status()` failure path in `reboot_host()`).

### Decision 1 (user-approved)

Two fix options were presented:
1. Drop `NoNewPrivileges=true` from the unit. Liam already has blanket
   `NOPASSWD: ALL` sudo (a decision made and confirmed in the prior
   worklog), so the flag isn't adding real defense-in-depth against the
   daemon's own sudo usage — it's only blocking the feature that needs it.
2. Keep `NoNewPrivileges=true`, switch to a D-Bus call to
   `org.freedesktop.login1.Manager.Reboot` with a new scoped polkit rule,
   avoiding sudo/setuid entirely.

User chose **option 1** (drop `NoNewPrivileges=true`).

## Test Plan (written before implementation)

- No unit-testable logic changes — this is a systemd sandbox flag flip,
  not application code. Regression coverage isn't meaningful here the way
  it is for the capture-log or stats-parsing modules.
- Local (macOS) checks before deploy, matching the deploy script's own
  gate: `cargo fmt --all -- --check`, `cargo test --locked --all-targets`,
  `cargo clippy --locked --all-targets -- -D warnings`.
- `systemd-analyze --user verify` against the edited unit file (already
  run on the Pi as part of the normal `setup-optic-daemon-phase-01.sh`
  install step) to confirm the file is still syntactically valid after
  removing the line.
- Empirical hardware validation (destructive, needs explicit go-ahead
  immediately before running, per the same policy the prior worklog used
  for this exact test):
  - After deploy, confirm `systemctl --user show optic-daemon.service -p
    NoNewPrivileges` → `no`.
  - Click "Reboot Pi" (or `POST /api/system/reboot` directly): expect
    `200 {"message":"rebooting"}` and the Pi to actually go down and come
    back up with `optic-daemon.service` auto-started (already
    enabled/lingering per prior worklog).

## Implementation Summary

- `systemd/optic-daemon.service`: removed the `NoNewPrivileges=true` line.
  No other sandboxing directives touched.
- `Cargo.toml`: version bump `0.1.24` → `0.1.25`, matching this project's
  convention of a version bump per deployed change (the deploy script
  gates on the binary containing the `Cargo.toml` version string).

## Validation

- **macOS dev target:**
  - `cargo fmt --all -- --check` — clean.
  - `cargo update --workspace --precise 0.1.25 -p optic-daemon` — updated
    only the local package's own version entry in `Cargo.lock` (confirmed
    via `git diff Cargo.lock`: single-line version bump, no dependency
    changes), needed because `Cargo.toml`'s version bump made the
    previously-committed lock file stale for `--locked` runs.
  - `cargo test --locked --all-targets` — 38/38 passed (0 failed, 0
    ignored), including the pre-existing `system_status::tests` suite
    (unaffected by this change — it covers stats-gathering logic, not the
    reboot subprocess call, which has no meaningful unit-testable
    behavior of its own).
  - `cargo clippy --locked --all-targets -- -D warnings` — clean.
- **Pi-native build/deploy (`scripts/build-deploy-optic-daemon.sh`,
  `optic-daemon` 0.1.24 → 0.1.25):**
  - Remote `cargo fmt --check` — clean.
  - Remote `cargo test --locked --all-targets` — 46/46 passed (Pi build
    includes the Linux-only `native_camera`/`native_codec` suites not run
    on macOS).
  - Remote `cargo clippy --locked --all-targets -- -D warnings` — clean.
  - Release build linked correctly against `libcamera.so.0.7` /
    `libturbojpeg.so.0`; install-with-rollback-protection script completed
    with no rollback triggered; `systemctl --user is-enabled`/`is-active`,
    `/healthz`, `/api/status` (version match), `/app.js` and `/` content
    checks all passed; no error-level journal entries since deploy start.
  - Post-deploy check (separate SSH session, after user reported wanting
    to confirm SSH itself was still healthy following the deploy):
    `ssh liam@optic.local` connected fine; `systemctl --user show
    optic-daemon.service -p NoNewPrivileges` → **`NoNewPrivileges=no`**
    (previously `yes`); `systemctl --user is-active optic-daemon.service`
    → `active`; `curl http://127.0.0.1:8000/healthz` → `ok`; `/api/status`
    version → `0.1.25`. Confirms the fix is live. (Deploying only restarts
    the daemon process, not the Pi itself, so SSH was never at risk from
    this step — the actual reboot test, which does take the Pi down, was
    intentionally not run this session.)
- **Actual reboot-button test (user-performed):** user clicked "Reboot
  Pi" on the dashboard. Post-reboot check via SSH: `uptime -p` → "up 17
  minutes" (fresh boot), `systemctl --user is-active optic-daemon.service`
  → `active`, `/api/status` version → `0.1.25`. The daemon's own
  `Install`/enable + linger configuration (from the original System
  Control Panel worklog) brought it back up with no manual intervention.
  **Reboot control confirmed working end-to-end.**

## User-Verification Steps (Round 1 — superseded, see Round 2)

- User clicked "Reboot Pi" on the dashboard; the Pi rebooted and
  `optic-daemon.service` came back up on its own. At the time this was
  read as confirmation the fix worked end-to-end.

## Round 1's Limitations (as understood at the time — since revised)

- Removing `NoNewPrivileges` was flagged as a real widening of what a
  compromised daemon process could do (could now exec setuid/setgid
  binaries; combined with the blanket `NOPASSWD: ALL` grant, daemon
  compromise ≈ root compromise). This concern is now moot: Round 2
  restores `NoNewPrivileges=true` and removes the daemon's `sudo`
  dependency entirely.

---

## Round 2: The Reboot Button Failed Again — Same Symptom, Different Cause

User reported the same `System action failed: reboot command exited with
exit status: 1` error on a later click, after Round 1 had apparently
succeeded once. Re-investigated rather than assuming it was a fluke.

### Root Cause 2

`systemd/optic-daemon.service`'s `ProtectSystem=strict` and
`ProtectHome=read-only` — untouched by Round 1 — **each independently**
force this `systemctl --user` (rootless) service into a private Linux
user namespace that can only map the service's own UID (1000→1000, range
1). This is how systemd implements filesystem sandboxing for a service
owned by an unprivileged user: it can't set up the real bind-mount sandbox
without root, so it falls back to an unprivileged user namespace, and an
unprivileged user can only ever map their own UID into a new namespace.

Inside that namespace, UID 0 (root) has no mapping and appears as the
kernel's overflow UID, `65534`. `sudo` performs a hard ownership check on
its own binary at startup (it must appear setuid-root) and refuses:

```
sudo: /etc/sudo.conf is owned by uid 65534, should be 0
sudo: /usr/bin/sudo must be owned by uid 0 and have the setuid bit set
```

This is unrelated to `NoNewPrivileges` and would have blocked `sudo`
either way. **This means the Round-1 "successful" reboot is no longer
trusted as evidence the fix worked** — this failure mode is 100%
reproducible (verified 3 times in a row below) and there is no persisted
journal from before the Round-1 reboot to check what actually triggered
it (this Pi's user journal isn't persistent, and the system journal only
goes back to the current boot). Given the project's own prior history of
watchdog-triggered "mysterious reboots" (`worklogs/2026-09-18-system-control-panel.md`),
a coincidental trigger is plausible and, given the reproduction rate below,
more likely than the sudo call having actually succeeded.

### Diagnosis (all non-destructive; no reboot triggered)

- Found the exact failure in the **system** (not user) journal —
  `sudo journalctl --since "-1 hour"` — three real, distinct attempts from
  the live daemon (PIDs 1781, 1806, 1840, all showing the same two-line
  sudo ownership error), confirming this happened for real, repeatedly, on
  the actual service.
- Confirmed the live daemon process (`MainPID`, e.g. 898) has
  `/proc/<pid>/uid_map` = `1000 1000 1` — a genuinely restricted namespace,
  not the host's default `0 4294967295` range.
- Reproduced the exact two-line sudo error in an isolated scratch unit,
  with no reboot and no effect on the real service:
  ```
  systemd-run --user --pipe --wait --collect \
      -p ProtectSystem=strict -p ProtectHome=read-only -- \
      bash -c 'cat /proc/self/uid_map; sudo -n true'
  ```
- Isolated which directive causes it by testing each alone the same way:
  **either `ProtectSystem=strict` alone, or `ProtectHome=read-only`
  alone, independently reproduces it.** A scratch unit with neither gets
  the full host UID range and `sudo -n true` succeeds (exit 0).
- An earlier same-session `nsenter` test had (incorrectly) suggested sudo
  was fine post-Round-1: it joined the daemon's mount/pid/net namespaces
  but not its **user** namespace (no `--user` flag), so it ran with the
  host's unrestricted UID view and passed — a false negative that didn't
  reflect what the real sandboxed process actually sees.

### Decision (user-approved)

Two options were presented:
1. Also drop `ProtectSystem=strict` and `ProtectHome=read-only`, keep
   using `sudo`. Simple, but a materially bigger sandboxing rollback than
   Round 1 scoped — on a daemon that also handles camera and network I/O.
2. Switch `reboot_host()` from `sudo systemctl reboot` to plain
   `systemctl reboot` (no sudo at all), which talks to systemd/logind over
   D-Bus. D-Bus authorization is a PolicyKit decision based on the real
   kernel-level peer credentials of the connecting process, not a
   namespace-relative `stat()` of a binary's owner — so it isn't affected
   by the UID-mapping problem. Requires a new, narrowly scoped polkit rule
   granting the daemon's user (`liam`) the `org.freedesktop.login1.reboot`
   action without interactive auth (by default this action requires
   `auth_admin_keep` for a non-active/lingering session — this is also
   the reason `docs/optic-daemon.md`'s "Reboot persistence" row noted
   "PolicyKit required interactive authorization" as a pending item).
   Lets **all** existing sandboxing stay intact, including restoring
   `NoNewPrivileges=true` from Round 1, since the daemon would no longer
   invoke `sudo` for anything.

User chose **option 2**.

### Verification of the D-Bus/polkit approach (before touching code)

All non-destructive — none of this reboots the Pi:
- `pkaction --verbose --action-id org.freedesktop.login1.reboot` — confirms
  the action defaults to `implicit any/inactive: auth_admin_keep` (matches
  the "requires interactive auth" symptom noted in the prior doc), while
  `implicit active: yes` (only for an active graphical/console session,
  which this lingering background service doesn't have).
- Installed `/etc/polkit-1/rules.d/60-optic-daemon-reboot.rules`:
  ```js
  polkit.addRule(function(action, subject) {
      if (action.id == "org.freedesktop.login1.reboot" &&
          subject.user == "liam") {
          return polkit.Result.YES;
      }
  });
  ```
- `pkcheck --action-id org.freedesktop.login1.reboot --process <pid>` as a
  plain `liam` shell process → exit 0 (was: "Authorization requires
  authentication" / exit 2 before the rule).
- Re-ran the same `pkcheck` **inside a scratch unit with the identical
  sandbox** (`ProtectSystem=strict` + `ProtectHome=read-only`, no sudo) →
  exit 0. Confirms the D-Bus path works from inside the real daemon's
  sandbox, unlike sudo.
- Confirmed basic D-Bus reachability from inside the same sandbox with two
  harmless read-only calls (`systemctl show-environment`,
  `loginctl list-sessions`) → both exit 0. (`RestrictAddressFamilies`
  already allows `AF_UNIX`, which D-Bus uses; `ProtectSystem=strict`
  makes `/run` read-only but doesn't block connecting to an existing
  socket file there.)

### Implementation Summary (Round 2)

- `src/system_status.rs`: `reboot_host()` now runs `systemctl reboot`
  directly (no `sudo`). Doc comment rewritten to explain both why `sudo`
  can never work here and why the D-Bus path does.
- `systemd/optic-daemon.service`: restored `NoNewPrivileges=true` — no
  longer needed to be off, since the daemon has no remaining `sudo`/setuid
  dependency for anything (`restart-daemon` already used a plain
  `systemctl --user restart` with no privilege escalation). This closes
  the loophole Round 1 opened, and keeps `ProtectSystem=strict` /
  `ProtectHome=read-only` fully intact as promised.
- `/etc/polkit-1/rules.d/60-optic-daemon-reboot.rules` (new, on the Pi,
  outside the git repo — host config, not project source): grants `liam`
  passwordless authorization for exactly `org.freedesktop.login1.reboot`.
  Narrower in scope than the existing blanket sudo `NOPASSWD: ALL` grant,
  though that grant remains in place untouched (out of scope; the prior
  worklog's decision to keep it stands).
- `Cargo.toml`/`Cargo.lock`: version bump `0.1.25` → `0.1.26`.

### Validation (Round 2)

- **macOS dev target:** `cargo fmt --all -- --check`, `cargo test
  --locked --all-targets` (38/38 passed), `cargo clippy --locked
  --all-targets -- -D warnings` — all clean.
- **Pi-native build/deploy (`scripts/build-deploy-optic-daemon.sh`,
  `optic-daemon` 0.1.25 → 0.1.26):** remote `cargo fmt --check`, `cargo
  test --locked --all-targets` (46/46 passed), `cargo clippy --locked
  --all-targets -- -D warnings` all clean; release build linked
  correctly; install-with-rollback-protection completed with no rollback;
  `is-enabled`/`is-active`, `/healthz`, `/api/status` version match,
  `/app.js` and `/` content checks all passed; no error-level journal
  entries since deploy start.
- **Hardware validation, performed and confirmed working** (see
  User-Verification Steps below for the actual reboot-button test).

## User-Verification Steps (Round 2) — Done

User independently tested the actual "Reboot Pi" button on the dashboard
and confirmed it works. Separately, post-reboot checks via SSH: `uptime
-p` → "up 1 minute" (fresh boot), `systemctl --user is-active
optic-daemon.service` → `active`, `/api/status` version → `0.1.26`,
`systemctl --user show ... -p NoNewPrivileges -p ProtectSystem -p
ProtectHome` → `yes` / `strict` / `read-only` (all as configured,
unchanged by the reboot), polkit rule file still present, `/healthz` →
`ok`. **Reboot control confirmed working end-to-end, by both the user
directly and by SSH-based verification, with sandboxing fully intact.**

## Closure

Objective met, acceptance criteria satisfied, both fix rounds documented
with root cause/decision/validation, and the user has verified the
feature themselves on real hardware. **This worklog is closed — no
further action pending on the reboot control itself.**

## Remaining Limitations / Follow-up

- The new polkit rule is host configuration living outside this git repo
  (`/etc/polkit-1/rules.d/` on the Pi) — not version-controlled the way
  the systemd unit is. Same category as the existing sudoers drop-in.
  Worth a mention in `docs/optic-daemon.md`'s host-setup material so a
  future rebuild-from-scratch doesn't silently lose it.
- The existing blanket sudo `NOPASSWD: ALL` grant for `liam` is now truly
  unused by the daemon itself (nothing in `optic-daemon` calls `sudo` any
  more). It was previously kept per explicit user decision for other
  reasons (interactive WiFi-debugging convenience) — not revisited here,
  out of scope for this fix.
