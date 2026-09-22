# Design: Run optic-daemon as a System Service

Status: **implemented and live on the Pi since 2026-09-21** (cutover, camera,
unprivileged deploys, rollback rehearsal, and a reboot showing
`time-sync.target` → `optic-daemon.service` were all observed). An offline
boot has not been tested. Record:
`worklogs/2026-09-21-system-service-migration.md`.

## 1. Why

The Pi is a dedicated, headless time-lapse station, and optic-daemon is what
it exists to run. Until now it ran as `liam`'s systemd **user** service. That
has costs for an unattended appliance:

- **Clock after power loss.** The Pi 5 RTC has no battery (`BATT_V` reads
  0 V, checked 2026-09-21), so after a power cut the clock starts wrong until
  NTP syncs. On the 2026-09-21 20:31 boot the daemon started at 20:31:20 and
  NTP first reached a server at 20:31:47. A user unit cannot be ordered after
  system targets such as `time-sync.target`, so scheduled captures and event
  timestamps can use a wrong clock.
- **Monitoring.** The Beszel agent lists only system services (its binary
  uses go-systemd's `NewSystemConnectionContext` only), so optic-daemon is
  invisible there.
- **Logs.** `journalctl --user -u optic-daemon` returns nothing on this Pi
  (`docs/optic-daemon-capture-performance.md`), so the deploy script's
  error-log check could never fire.
- **Sandbox.** In a user manager, `ProtectSystem=`/`ProtectHome=` force a
  private user namespace, and device and cgroup settings are silently
  skipped. The unit's sandbox was partly decorative.

## 2. Decisions

1. **Unprivileged at runtime.** `/etc/systemd/system/optic-daemon.service`
   with `User=liam`, `Group=liam`, `SupplementaryGroups=video render`. The
   binary, web assets and state stay under `/home/liam`, with explicit paths
   (`%h` would expand to root's home in a system unit). Nothing moves on
   disk.
2. **Boot ordering.** `Wants=`/`After=` `network-online.target` and
   `time-sync.target`, plus `RequiresMountsFor=/mnt/capture /home/liam`.
   `WantedBy=multi-user.target`.
3. **Bounded wait for NTP.** `time-sync.target` only waits for a real sync if
   `systemd-time-wait-sync.service` is enabled, and that unit ships with
   `TimeoutStartSec=infinity`: on a boot without network, the daemon would
   never start. A drop-in bounds it to 90 s and exits successfully either way
   (`systemd/systemd-time-wait-sync.service.d/optic-bounded-wait.conf`).
   Cost: the dashboard comes up about 30 s later on a normal boot, and up to
   90 s later offline. Captures and timestamps are then correct from their
   first write.
4. **Deploys stay unprivileged.** The deploy script, as `liam`, installs the
   binary and web assets and restarts the unit. The restart is authorized by
   one new PolicyKit rule, `64-optic-daemon-manage-unit.rules`:
   `org.freedesktop.systemd1.manage-units` for `optic-daemon.service`, verbs
   `start`/`stop`/`restart` only, managed by the Phase 8 script. The same
   rule serves the dashboard's Restart daemon button.
5. **Unit changes are a separate root step.** A root script
   (`scripts/setup-optic-daemon-system-service.sh`) installs the unit and
   drop-in, runs `daemon-reload`, and performs the one-time migration. On
   every deploy, the deploy script checks that the installed unit matches
   `systemd/optic-daemon.service` byte for byte. If it doesn't, the deploy
   refuses and says which script to run. Unit changes are rare and should be
   deliberate.
6. **Device access is explicit.** Under the system manager,
   `ProtectClock=yes` adds a `DeviceAllow=` entry, which turns device access
   into an allow-list. The camera's nodes (`/dev/media*`, `/dev/video*`,
   `/dev/dma_heap/*`) are listed explicitly by device class, so the camera
   keeps working while other devices stay blocked. The cutover verifies this
   with real captures.
7. **Lingering stays on.** It is no longer needed for optic-daemon, but the
   Beszel agent is still a user service.

## 3. Cutover and Rollback

`setup-optic-daemon-system-service.sh` (root, idempotent, `--dry-run`,
`--rollback`):

1. Preconditions: the binary is installed, `/mnt/capture` is tmpfs, `liam`
   is in `video`/`render`, and PolicyKit rule 64 is installed.
2. Install the unit and the time-wait-sync drop-in, backing up any previous
   copy to `/var/backups/optic-hardening/`. `systemd-analyze verify`, then
   `daemon-reload`, then enable `systemd-time-wait-sync.service`.
3. Stop and disable the user unit, and move its file into the backup
   directory. Both units would compete for the camera and port 8000, so the
   user unit is always stopped first.
4. `systemctl enable` + `restart optic-daemon.service`, then wait (up to
   60 s) for `/api/status` to answer with the camera detected. A device
   access mistake in the sandbox therefore fails the check instead of
   passing it.
5. On a failed health check, roll back automatically: stop and disable the
   system unit, restore and start the user unit.

`--rollback` performs step 5 on demand (for example, if a camera problem
shows up later).

## 4. Validation

The worklog's test plan covers local checks, the cutover on the Pi, the
camera (preview and a real capture), the Restart daemon button, a deploy
through the new path, and one approved reboot that shows the start order
(`time-sync.target` before `optic-daemon.service`).
