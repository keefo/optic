# Dated Worklog: 2026-09-21 - optic-daemon as a System Service

Status: **deployed and hardware-validated on the Pi (cutover, camera, deploys, Restart daemon, rollback rehearsal, real reboot); awaiting user acceptance (Beszel)**

Branch: `feat/system-service-migration` (renamed from
`add-a-shutdown-button-on-dashboard` at the user's request. This migration is
the branch's focus; the Power menu and the system events log,
`worklogs/2026-09-21-shutdown-button.md` and
`worklogs/2026-09-21-system-events-log.md`, are side tasks on it).

Design: `docs/optic-daemon-system-service.md`.

## Objective

Move optic-daemon from `liam`'s systemd user manager to a system service that
still runs as `liam`. The user's reason: the Pi is a dedicated headless
time-lapse station. Gains: boot ordering after NTP sync (no RTC battery),
visibility in Beszel's Systemd Services, logs in the system journal, and a
sandbox that is actually enforced.

## Facts Established Before Planning (Pi, 2026-09-21)

- Beszel agent 0.19.0 is a user service and only calls go-systemd's
  `NewSystemConnectionContext`. Its list shows system units only (it shows
  the failed `optic-capture-transfer.service`).
- The RTC has no battery: `BATT_V` = 0 V, `charging_voltage` = 0.
- Boot `46d2f216`: the daemon started 20:31:20; timesyncd first contacted a
  server at 20:31:47.
- `systemd-time-wait-sync.service` is disabled, with
  `TimeoutStartSec=infinity`.
- `liam` is in `adm`, so the system journal is readable without sudo.
- The deploy script's journal error check uses `journalctl --user -u`,
  which returns nothing on this Pi, so the check never fired.

## Acceptance Criteria

1. `optic-daemon.service` is a system unit (`/etc/systemd/system`), enabled
   for `multi-user.target`, running as `liam` (groups `video`, `render`). No
   user unit remains enabled or installed.
2. After a reboot it starts after `time-sync.target` and
   `network-online.target`. With network, that means after NTP sync. Without
   network, the wait is bounded at about 90 s, and
   `systemd-time-wait-sync.service` still ends successfully.
3. Camera works: `/api/status` reports the IMX477, the preview streams, and a
   Capture & transfer succeeds (history row, success).
4. Deploys still run as `liam` without sudo. A full deploy restarts the
   system unit through PolicyKit rule 64. The deploy refuses if the installed
   unit differs from `systemd/optic-daemon.service`. The journal checks read
   the system journal. Rollback restores the binary and restarts the unit.
5. The dashboard's Restart daemon works, and the events log shows
   `daemon_restart_requested` → `daemon_stop` → `daemon_start`.
6. Rule 64 allows only `start`/`stop`/`restart` of `optic-daemon.service` for
   `liam`. Restarting another unit (`ssh.service`) is still denied. Checked by
   the Phase 8 script and `verify.sh`.
7. `verify.sh` checks the system unit (enabled, active, `User=liam`) and that
   no user unit is left.
8. A root cutover script with `--dry-run` and `--rollback`, and automatic
   rollback if the health check fails.
9. Beszel's Systemd Services lists optic-daemon (**user acceptance**: I
   cannot open the Beszel UI).
10. Docs updated: design, `setup.md`, `docs/optic-daemon.md`, the build
    environment and capture-performance journal commands, alerts, the Phase 9
    inventory, and `README.md`.

## Test Plan

Local (Mac):
- L1 `cargo fmt --check`, `clippy -D warnings`, `cargo test --locked`,
  `biome ci`.
- L2 `bash -n` on every changed script. The new script's `--help`, and
  `--print` of the managed files (no root needed).
- L3 Unit test: `restart_daemon_detached` uses the system manager with
  `--no-block` (a pure helper building the argument list).

Pi (each step only with user approval; the cutover is a privileged host
change):
- P1 Phase 8 `--dry-run`: only rule 64 is `[CHANGE]`. Then apply it: pkcheck
  authorizes restart of `optic-daemon.service` and denies `ssh.service`.
- P2 Cutover `--dry-run`: the planned actions match §3 of the design. Then the
  real cutover: health and version OK; `systemctl --user` shows no
  optic-daemon; `systemctl show -p User,ActiveState,UnitFileState` shows
  liam/active/enabled.
- P3 Camera: `/api/status` detected; preview frames arrive
  (`/api/stream/mjpeg` bytes); one Capture & transfer succeeds.
- P4 `journalctl -u optic-daemon` as `liam` shows the daemon's lines.
- P5 Restart daemon (the button's endpoint) → events log sequence, service
  active again.
- P6 A full deploy through the updated script: success, restart via
  PolicyKit, the unit-drift check passes; a deliberate drift test (a copy of
  the script against a modified unit) refuses.
- P7 Approved reboot: `systemd-analyze critical-chain optic-daemon.service`
  shows `time-sync.target` before it; the events log shows a reboot and a
  boot; the timesyncd sync precedes the daemon start in the journal.
- P8 `verify.sh --phase 8`: all pass.
- P9 Rollback rehearsal: `--rollback` returns to the user unit and healthy,
  then the cutover again. Only if the user wants it; otherwise it is recorded
  as untested.
- Beszel: user verification.

## Implementation

| File | Change |
|------|--------|
| `systemd/optic-daemon.service` | System unit: `User=liam`, `Group=liam`, `SupplementaryGroups=video render`; explicit `/home/liam` paths; `Wants`/`After` `network-online.target time-sync.target`; `RequiresMountsFor=/mnt/capture /home/liam`; `DevicePolicy=closed` + `DeviceAllow` for `char-video4linux`, `char-media`, `char-dma_heap`, `char-drm` (rw); empty `CapabilityBoundingSet`/`AmbientCapabilities`; `WantedBy=multi-user.target`. Existing sandbox and limits kept |
| `systemd/systemd-time-wait-sync.service.d/optic-bounded-wait.conf` (new) | `ExecStart=/usr/bin/timeout 90 /usr/lib/systemd/systemd-time-wait-sync`, `SuccessExitStatus=124`, `TimeoutStartSec=100s` |
| `scripts/setup-optic-daemon-system-service.sh` (new) | Root cutover: preconditions (binary, tmpfs, groups, rule 64), install and back up unit + drop-in, `systemd-analyze verify`, enable time-wait-sync, stop/disable/back up the user unit (`systemctl --user -M liam@`), enable + restart the system unit, health check = `/api/status` with the camera detected, automatic rollback; `--dry-run`, `--rollback` |
| `scripts/setup-phase-08-daemon-host-access.sh` | Rule `64-optic-daemon-manage-unit.rules` (manage-units, `optic-daemon.service`, start/stop/restart, liam); pkcheck: restart authorized, `reload` still refused |
| `scripts/setup-optic-daemon-phase-01.sh` | No longer installs units; refuses if the installed unit/drop-in differ from `systemd/`; `systemctl restart` of the system unit |
| `scripts/build-deploy-optic-daemon.sh` | Unit-drift check in the prerequisites (before the daemon is stopped for the build); lingering no longer required; all `systemctl --user`/`journalctl --user` → system; rollback restores the binary and restarts (unit untouched); new script in the required paths |
| `src/system_status.rs` | `restart_daemon_detached` → `systemctl --no-block restart optic-daemon.service`, logs failures; test `restart_targets_the_system_service_without_blocking` |
| `verify.sh` | Phase 8: system unit `enabled active liam`, no user unit, time-wait-sync enabled, rule 64 authorize + scope checks, rule 64 file mode; lingering message now refers to the Beszel agent |
| Docs | `docs/optic-daemon-system-service.md` (new), `setup.md` §F/§G, `docs/optic-daemon.md` §9, the build-environment and capture-performance journal/systemctl commands, alerts, `README.md`, the Phase 9 inventory + §3.3 decision 2 resolved |

## Local Validation (2026-09-21)

| Test | Result |
|------|--------|
| L1 | `cargo fmt --check` OK; `clippy --all-targets -D warnings` OK; `cargo test --locked` **170 passed**; `biome ci` clean |
| L2 | `bash -n` on all of `scripts/*.sh` and `verify.sh`: OK. `--help` prints usage |
| L3 | `restart_targets_the_system_service_without_blocking` passes |
| Rule 64 logic (Node, stub `polkit`) | liam restart/start/stop of optic-daemon.service → YES; `reload` → no decision; `ssh.service` restart → no decision; another user → no decision; `manage-unit-files` → no decision |
| Unit on the Pi, read-only (copies in `/tmp`, nothing installed) | `systemd-analyze verify` exit 0; `systemd-analyze security --offline` exposure **3.5 OK**; `/usr/bin/timeout 1 sleep 5` exits 124 |
| Device classes | `/proc/devices` on the Pi lists `video4linux` (81), `drm` (226), `dma_heap` (252), `media` (511). The running daemon holds `/dev/media*`, `/dev/video*`, `/dev/v4l-subdev*` open |

Environment note: `scp` to `optic.local` failed once with `Connection
closed`; `-4` and a retry worked (the same transient mDNS/IPv6 issue as
earlier today).

## Pi Rollout (user approved 2026-09-21: all steps incl. reboot, plus a rollback rehearsal)

| Step | Command / check | Observed |
|------|-----------------|----------|
| P1 dry run | `sudo -n optic-setup-phase-08-daemon-host-access --dry-run` | 60–63 `[OK]`; only `[CHANGE] Install PolicyKit rule: …/64-optic-daemon-manage-unit.rules` |
| P1 apply | same script | `Installed …/64-optic-daemon-manage-unit.rules`; its pkcheck loop passed (restart authorized) and the new scope check passed (`reload` refused); "… timezone, and optic-daemon restart actions are authorized." |
| P2 stage | `tar` of `scripts/setup-optic-daemon-system-service.sh` + `systemd/` → `~/.local/src/optic-cutover-20260921T204949` (symlink `optic-cutover`) | OK (harmless macOS xattr warnings from GNU tar) |
| P2 dry run | `sudo -n …/setup-optic-daemon-system-service.sh --dry-run` | unit `[missing]`, drop-in `[missing]`, time-wait-sync `[disabled]`, system unit `not-found/inactive`, `[CHANGE] Stop, disable, and back up the user unit` |
| P2 cutover | same script, 20:49:55 → 20:49:58 | time-wait-sync symlinked into `sysinit.target.wants`; user unit removed from `default.target.wants`; system unit enabled in `multi-user.target.wants`; health check passed (camera detected); exit 0. After: `UnitFileState=enabled`, `ActiveState=active`, `User=liam`, `Group=liam`, `SupplementaryGroups=video render`, `DevicePolicy=closed`; `ps` shows the process as `liam` with `video`; `~/.config/systemd/user/` has no optic-daemon unit |
| P3 camera | `/api/status` | version 0.1.30 (old binary, new unit), camera detected |
| P3 preview | `POST /api/stream/start` `{"settings":{},"profile":"binning_2k"}`, 4 s of `/api/stream/mjpeg`, `POST /api/stream/stop` | "preview started"; **6,511,838 bytes** of MJPEG in 4 s; "preview stopped" |
| P3 capture | `POST /api/capture` binning_2k, no DNG | `testshot-2k-binning-1790049024808.jpg`, 337,775 bytes, 2028×1520; history row success, 1016 ms |
| P4 journal | `journalctl -u optic-daemon.service` as liam | daemon lines returned (capture perf stages). Lines carry ANSI colour codes (tracing's default); cosmetic, predates this change, left as follow-up |
| P6 drift logic | `cmp` of a modified copy vs the installed unit / the repo unit vs installed | modified → drift detected (deploy would refuse); repo unit → matches (deploy proceeds) |
| P6 full deploy #1 (`scripts/build-deploy-optic-daemon.sh`) | exit 0; the Pi ran **173** tests (Linux-only included); the drift check passed; the journal shows `systemd[1]` (system manager) stopping/starting the unit, i.e. the restart went through PolicyKit rule 64 as liam; `SUCCESS: optic-daemon 0.1.30 is active` |
| P5 Restart daemon (`POST /api/system/restart-daemon`) | MainPID 5830 → 5942, active; events `14 daemon_restart_requested`, `15 daemon_stop`, `16 daemon_start`. **Found:** a spurious `WARN optic-daemon restart request failed status=signal: 15 (SIGTERM)`. The restart stops the whole cgroup, which can kill the `systemctl --no-block` child before it exits. **Fixed:** a status with no exit code (killed by a signal) counts as success; a non-zero exit still warns. Local: fmt/clippy OK, 170 tests pass |
| P9 rollback rehearsal | `--rollback` 20:53:26 | "Rollback complete"; system unit `not-found/inactive`, the unit and drop-in removed, time-wait-sync `disabled`; user unit `enabled/active`, file `liam:liam 644`; `/api/status` camera detected |
| P9 re-cutover | script again, 20:53:35 | exit 0; system `enabled/active`, no user unit file, time-wait-sync `enabled`; timestamped backups of every replaced or removed file in `/var/backups/optic-hardening/` |
| P6 full deploy #2 (restart-warning fix) | exit 0, 173 tests on the Pi, `SUCCESS`; Restart daemon again → active, **0** `WARN … restart` lines |
| P7 reboot (Power → Reboot endpoint) 20:56:00 | Down 20:56:06, back 20:56:52 (new boot `677b609e`, `uptime -s` 20:56:14). `systemd-analyze critical-chain optic-daemon.service`: `optic-daemon.service @33.118s` ← `time-sync.target @33.114s` ← `systemd-time-wait-sync.service @1.077s +32.036s`. Journal: network-online 20:56:27.654; **NTP contact 20:56:48.583**; time-wait-sync finished 20:56:48.607; time-sync.target 20:56:48.607; **optic-daemon started 20:56:48.611**. `systemd-time-wait-sync`: `Result=success ExecMainStatus=0` |
| P7 events | `/api/events` | `26 reboot_requested`, `27 reboot {target: reboot.target}` (detected under the system unit), `28 boot` 20:56:14 with `previous_boot.ended: reboot`, `29 daemon_start` 20:56:48 (after the NTP sync) |
| P3 after reboot | `/api/status` | camera detected, uptime 21 s |
| P8 `verify.sh --phase 8` as liam | | **PASS=21 WARN=0 FAIL=0**, incl. the new checks: system service `enabled active` User=liam, no user unit, bounded NTP wait enabled, rule 64 authorizes restart, rule 64 refuses `reload`, rule 64 file `644 root:root` |
| Beszel precondition | `systemctl list-units --type=service optic-daemon.service` (system manager, what the Beszel agent queries) | `optic-daemon.service loaded active running` |

## Acceptance Criteria Status

1 ✅ system unit, liam, no user unit · 2 ✅ ordered after NTP sync (observed); the offline 90 s bound is by design and the `timeout` 124 exit was checked, but an offline boot was **not** tested · 3 ✅ status, preview, capture · 4 ✅ two unprivileged full deploys, drift check · 5 ✅ Restart daemon + events · 6 ✅ rule 64 scope · 7 ✅ verify.sh · 8 ✅ cutover/rollback/auto-rollback. Auto-rollback runs the same code as `--rollback`, which was rehearsed, but a failing health check was not induced · 9 ⏳ Beszel UI (user) · 10 ✅ docs.

## Limitations and Follow-ups

- An offline boot (no network) was not tested. Expected: time-wait-sync
  gives up at 90 s with success, and the daemon starts with the RTC's stale
  clock. Test it by booting with Wi-Fi unavailable, if wanted.
- Every boot now brings the dashboard up after NTP sync (32 s on this boot).
- Journal lines carry ANSI colour codes (tracing default). This is cosmetic
  and predates this change; `.with_ansi(false)` under systemd would clean it
  up.
- The staging copy `~/.local/src/optic-cutover-20260921T204949` (symlink
  `optic-cutover`) is left on the Pi; later deploys stage the full tree,
  including this script, under `~/.local/src/optic-daemon`. It is safe to
  delete.
- Not caused by this work: the system unit `optic-capture-transfer.service`
  is failed (it will be visible in Beszel too); unknown paths return 500.

## User Verification

1. Beszel → the Pi → Systemd Services: optic-daemon should be listed as
   running.
2. The dashboard works as before; Capture History → System events shows the
   20:56 reboot.

## Revision 1 (2026-09-21): Beszel did not list optic-daemon

Status: **resolved without a code change** (the planned drop-in was dropped;
see "Correction" below)

User report after the rollout: Beszel still did not show optic-daemon.

Findings:
- Beszel agent 0.19.0 reads `SERVICE_PATTERNS` (default `*.service`) and
  lists units via go-systemd `ListUnitsByPatterns` (strings in the binary).
- This boot: the agent (user service) started at 20:56:22, and
  optic-daemon at 20:56:48, after the NTP wait.
- Restarting only the agent (21:03:46, optic-daemon running) made
  optic-daemon appear in Beszel (**confirmed by the user**). So the agent
  builds its service list when it starts, and a unit that has not started yet
  is left out until the agent restarts.

Fix: a drop-in for the agent's user unit,
`~/.config/systemd/user/beszel-agent.service.d/optic-wait-for-daemon.conf`.
Its `ExecStartPre` polls the system manager (read-only, no privileges) until
`optic-daemon.service` is active or failed, for at most 180 s, and never
fails the agent's start (`-` prefix). The cutover script installs it (owned
by liam) and restarts the agent; `--rollback` removes it. `verify.sh` checks
it.

Test plan:
- R1 `bash -n`; `systemd-analyze --user verify` of the agent unit with the
  drop-in on the Pi.
- R2 Apply (rerun the cutover script, idempotent) → the agent's
  `ExecStartPre` returns at once while optic-daemon is active; the agent is
  active.
- R3 Reboot (**user approval**): the agent's start (`ActiveEnterTimestamp`)
  is after optic-daemon's; Beszel still lists optic-daemon (user check).

### Correction (source read after the plan)

The Beszel agent source for the installed tag
(`henrygd/beszel` `v0.19.0`, `agent/systemd.go`) shows that the service list
is **rebuilt every 10 minutes**, not only at start. A unit that has never
been active (`ActiveEnterTimestamp` 0) is skipped, and picked up at the next
refresh. At 20:56:22 optic-daemon was still waiting for NTP, so it was
skipped; the user's first check came before the 21:06 refresh. The agent
restart at 21:03:46 only made it appear sooner. After a reboot optic-daemon
shows up in Beszel within 10 minutes on its own. The drop-in
(`systemd/beszel-agent.service.d/optic-wait-for-daemon.conf`) was written but
**never installed**, and was deleted: it would delay the agent's start (and
its early-boot metrics) on every boot to save that gap.

## Finding (2026-09-21): memory cgroup is disabled, so memory limits were never enforced

Asked by the user: why Beszel shows no CPU or memory for most services.

- `/proc/cmdline` contains `cgroup_disable=memory`;
  `/sys/fs/cgroup/cgroup.controllers` = `cpuset cpu io pids` (no `memory`);
  `system.slice` subtree_control = `pids`.
- `MemoryCurrent` is `[not set]` for every service checked (optic-daemon,
  ssh, NetworkManager, journald, avahi, cron, polkit, timesyncd), although
  `DefaultMemoryAccounting=yes`. Beszel ignores the unavailable value
  (`MaxUint64`), so it shows no memory.
- **optic-daemon's `MemoryHigh=350M` / `MemoryMax=500M` are not enforced**:
  its cgroup has no `memory.max`/`memory.high`. This was true under the user
  unit as well; `docs/optic-daemon.md` said it "enforces" them. Docs
  corrected.
- CPU: `CPUUsageNSec` is populated for every service. Beszel's CPU % is a
  delta between samples, rounded to 2 decimals, and 0 on the first sample
  (`internal/entities/systemd`), so idle services legitimately show 0 or
  nothing. It is not missing data.
- Enabling it needs `cgroup_enable=memory` in `/boot/firmware/cmdline.txt`
  plus a reboot: a boot-config change that needs **user approval**, not made.

## Revision 2 (2026-09-21): enable the memory cgroup (user approved)

Status: **applied and hardware-validated, including a real on-Pi release
build without the SD swap file**

Objective: make optic-daemon's `MemoryHigh=350M`/`MemoryMax=500M` real, and
give Beszel per-service memory.

Facts (Pi, before the change):
- The firmware prepends `cgroup_disable=memory` to the kernel command line;
  `cmdline.txt` (a single line on vfat `/boot/firmware`) comes after it. The
  Raspberry Pi kernel honours a later `cgroup_enable=memory`.
  `CONFIG_MEMCG=y`, kernel `6.18.50+rpt-rpi-2712`, 990 MiB RAM.
- Risk checked: tmpfs pages the daemon writes to `/mnt/capture` are charged
  to its cgroup. `/mnt/capture` is capped at 256 MiB; the daemon's RSS is
  27 MiB (VmHWM, idle). Worst case (full stage + a capture in progress) is
  near `MemoryHigh` (throttling and reclaim to zram), and a full stage makes
  captures fail on space anyway. `MemoryMax` leaves ~150 MiB of headroom.

Change: `scripts/setup-phase-02-memory.sh` manages the `cgroup_enable=memory`
token in `/boot/firmware/cmdline.txt`: append if missing, keep the file a
single line, back it up, write via a temp file + rename in the same directory
(no chmod/chown on vfat), dry-run `[OK]`/`[CHANGE]`, and a reboot required
when the runtime controller is missing. `verify.sh` §2 checks the token and
the runtime `memory` controller.

Test plan:
- M1 `bash -n`; run the token-editing function against temp copies of the
  real cmdline (token absent → appended once; present → unchanged; stays a
  single line).
- M2 Pi dry run: only the cmdline `[CHANGE]` plus "reboot required"; zram and
  sysctl `[OK]`.
- M3 Apply + approved reboot: `/proc/cmdline` ends with
  `cgroup_enable=memory`; `cgroup.controllers` includes `memory`;
  optic-daemon's cgroup has `memory.max` = 524288000 and `memory.high` =
  367001600; `MemoryCurrent` is set for services.
- M4 Load: one Master Archive + DNG capture succeeds; record `memory.peak`
  for optic-daemon; no `memory.events` `high`/`max`/`oom` counts.
- M5 `verify.sh --phase 2` and `--phase 8` pass. Beszel shows memory (user).

### Results

| Test | Observed |
|------|----------|
| M1 | Token function on temp copies of the real `cmdline.txt`: appended once, 1 line, 1 backup, the diff is only the token; a second run is a no-op |
| M2 dry run | **Also found unrelated drift**: `[CHANGE] …/etc/rpi/swap.conf.d/90-optic.conf` (installed `FixedSizeMiB=493`, plan now 495: MemTotal is 1014416 kB) and the runtime zram check failing because **`/var/swap.img` (2 GiB, pri=10) was active**, from a hand-added `/etc/fstab` line (file dated Sep 16 23:47; recorded nowhere in the repo). Stopped and asked the user |
| User decisions | Run the full Phase 2 script (incl. the 493→495 zram resize); remove the SD swap file after it was explained (the build-OOM safety net vs SD wear, stalls, the Phase 2 contradiction, and incompatibility with Phase 9) |
| Apply | Phase 2 script: backed up + installed `90-optic.conf` (495), backed up + appended the token (`cmdline.txt` now ends `… cfg80211.ieee80211_regdom=CA cgroup_enable=memory`); native rpi-swap output validated; sysctls unchanged |
| Swap file | `/etc/fstab` backed up (`/var/backups/optic-hardening/etc_fstab.20260921T211824.bak`), line commented with a pointer to this worklog; `findmnt --verify` clean; `swapoff /var/swap.img` (0 used); `daemon-reload`; `/proc/swaps` = zram0 only. **The file is kept** (2 GiB) so it can be re-enabled by uncommenting the line |
| M3 reboot 21:18:39 | back 21:19:28. `/proc/cmdline`: `cgroup_disable=memory … cgroup_enable=memory`; controllers `cpuset cpu io memory pids`; optic-daemon `memory.max=524288000`, `memory.high=367001600`, current 32 MiB; `MemoryCurrent` set for ssh (10 MiB), NetworkManager (26 MiB), journald (7 MiB); zram0 506864 KiB (495 MiB), no other swap |
| M4 load | Master Archive + DNG: 10,947,022 B JPEG + 24,673,306 B DNG, 4056×3040, success, 1973 ms. optic-daemon `memory.peak` **129 MiB**; `memory.events`: low 0, high 0, max 0, oom 0, oom_kill 0. `memory.stat`: anon 9, file 52, shmem 34 MiB (capture files waiting in the tmpfs, charged to the daemon as expected) |
| M5 | `verify.sh --phase 2` **PASS=18 FAIL=0** (new memory-cgroup checks pass; "All active swap devices use zram"); `--phase 8` **PASS=21 FAIL=0** |
| Build without the SD swap (1st try) | A full deploy right after the reboot took **8 s**: the release build was cached, so it proved nothing (reported as such). The sampler also stopped early, and the closing `pkill -f` matched its own SSH command (exit 255) |
| Build without the SD swap (real) | `touch src/main.rs` (mtime only) → the deploy recompiled optic-daemon, **LTO release link `Finished … in 1m 17s`**, 173 tests, `SUCCESS`, 21:20:59 → 21:22:57. Sampler every 2 s (65 samples): **min MemAvailable 25 MiB** and **peak zram used 254 MiB of 495** (both at 21:22:51, the LTO link). Kernel log: 0 OOM/kill lines; `uptime -s` unchanged (no watchdog reset); daemon active |

### Revision 2 limitations

- The build fits, but tightly: 25 MiB available at the LTO peak, and zram at
  about half. Only the crate + LTO link was rebuilt; a clean build of all
  dependencies (e.g. after a toolchain or `Cargo.lock` change) was not
  measured. If one ever fails or trips the watchdog, limit build jobs
  (`CARGO_BUILD_JOBS=2` in the deploy) rather than bringing back SD swap.
- `/var/swap.img` (2 GiB) is still on disk, disabled. Delete it once
  confident (deleting it needs approval), or uncomment its `/etc/fstab` line
  to restore it.
- A completely full 256 MiB `/mnt/capture` stage plus a capture in progress
  can reach optic-daemon's `MemoryHigh` (throttling and reclaim to zram).
  Captures already fail on a full stage.

### Beszel memory display (user acceptance, 2026-09-21)

The user first reported that memory still did not show. Cause: Beszel agent
0.19.0 re-reads per-service CPU/memory over D-Bus only on its 10-minute
refresh (`getServiceStats(nil, true)`); in between it serves cached values
(`refresh=false`). The agent started at 21:19:00 (boot), so the first read
with the memory cgroup on was at 21:29. At 21:29:44 `busctl get-property …
org.freedesktop.systemd1.Service MemoryCurrent` as liam returned
optic-daemon 27,361,280 and ssh 10,551,296 (`MemoryPeak` 32,735,232), and
the user then confirmed that **Beszel shows memory usage** (and, earlier,
that optic-daemon is listed). After any reboot, expect per-service data in
Beszel to appear or update up to 10 minutes after the agent starts.

## Rebase onto main before the PR (2026-09-21)

- Rebased onto `origin/main` `feb90e5` (PR #11: verify.sh Phase 6 checks
  optic_sync, stale-binary deploy guard with `tar -m`, version 0.1.31).
- One conflict: the API list in `docs/optic-daemon.md`. Resolved by keeping
  main's expanded list and adding shutdown (with reboot), restart-daemon
  (now via rule 64), and `GET /api/events`.
- **Integration fix:** PR #11's new Phase 6 check read the sync settings with
  `systemctl --user show optic-daemon.service`, which is empty for the system
  service. Changed to `systemctl show`. On the Pi, read-only:
  `verify.sh --phase 6` PASS=13 (incl. "Capture sync target —
  admin@imacpro.local:2222"), `--phase 2` PASS=18, `--phase 8` PASS=21.
- Local on the rebased tree: fmt, clippy `-D warnings`, `cargo test` (170),
  `biome ci`, `bash -n` on all scripts: all pass.
- The Pi still runs this branch's pre-rebase build (0.1.30). The rebased
  0.1.31 has not been deployed; that happens with the next deploy after
  merge.
