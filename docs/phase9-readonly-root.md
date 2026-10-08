# Phase 9 Runbook: Read-Only Root (OverlayFS) with a Persistent Data Volume

Status: **design only, not enabled.** No host change described here has
been made. Facts marked "observed", "measured", or "confirmed" were read
from the live Pi on 2026-09-20 without changing it. Each stage below is a
separate step that needs explicit user approval before it runs. Written 2026-09-20 (`worklogs/2026-09-20-provisioning.md`).

This replaces the bare raspi-config steps in `setup.md` §9 as the plan of
record. It implements "Option B" from `docs/optic-daemon-capture-log.md` §4:
a dedicated writable volume for the state that must survive a reboot.

## 1. What raspi-config's OverlayFS actually does

`raspi-config` → *Performance Options* → *Overlay File System* boots through
an initramfs that mounts the SD card root **read-only as the lower layer** and
a **RAM tmpfs as the upper layer**, and presents the merged overlay as `/`.

Consequences for this project:

- **Writes to `/` do not fail. They succeed into RAM and vanish at the next
  boot.** `docs/optic-daemon-capture-log.md` §4 expects `history.db` writes to
  "start failing" with a loud warning. Under this mechanism the
  database would keep working, silently lose every row written since boot on
  each reboot, and grow in RAM. That is worse than a loud failure and is the
  main reason Option B is required, not optional. (That section should be
  updated when this plan is approved; it is outside this track's files.)
- **The upper layer uses RAM on a 1 GB Pi** that already gives 256 MiB to
  `/mnt/capture` and up to 500 MiB (`MemoryMax`) to the daemon. Anything that
  writes steadily to `/` reduces the headroom until the next reboot.
- `ProtectSystem=strict` in `systemd/optic-daemon.service` already makes `/`
  read-only for the daemon, so the daemon can only write to its
  `ReadWritePaths=`. The inventory below still covers every other writer,
  because they share the same RAM.
- The optional "read-only boot partition" step makes `/boot/firmware`
  read-only. Phase 2, 4, and 5 setup scripts edit `config.txt`, so they need
  it writable.

Confirmed on the live Pi (2026-09-20, read-only, from `/usr/bin/raspi-config`):

- `enable_overlayfs` runs `apt-get install -y overlayroot` if needed (not
  installed today) and prepends `overlayroot=tmpfs ` to
  `/boot/firmware/cmdline.txt`. `disable_overlayfs` removes that prefix.
  Both take effect at the next boot. The Pi's config already has
  `auto_initramfs=1`.
- `enable_bootro` / `disable_bootro` add or remove `,ro` on the
  `/boot/firmware` line in `/etc/fstab`, and **refuse to run while the
  overlay is active** ("Overlay in use; cannot update fstab").
- Still to confirm after `overlayroot` is installed: where it mounts the lower
  and upper layers (expected `/media/root-ro` and `/media/root-rw`).

## 2. Runtime write inventory

"Today" is the live Pi (plain ext4 root, `rw`). "Under OverlayFS" assumes the
RAM-upper mechanism in §1 with no Option B.

| Path | Writer and pattern | Under OverlayFS without Option B | Plan |
|------|--------------------|----------------------------------|------|
| `~/.local/state/optic-daemon/history.db` (+ `-wal`, `-shm`) | daemon: one row per capture, SQLite WAL | **Breaks**: rows lost at every reboot; RAM growth | Data volume (bind mount) |
| `~/.local/state/optic-daemon/events.db` (+ `-wal`, `-shm`) | daemon: a few rows per boot, daemon start/stop and Power request (`docs/optic-daemon-system-events.md`), SQLite WAL | **Breaks**: boot/shutdown history lost at every reboot, and every boot looks like the first | Data volume (same directory) |
| `~/.local/state/optic-daemon/config.json` | daemon: on Save (commit), temp file + rename (`src/durable_state.rs`) | **Breaks**: scheduler rules, Station, camera config revert at reboot | Data volume (same directory) |
| `~/.local/state/optic-daemon/schedule_run_state.json` | daemon: on run/pause changes | **Breaks**: run/pause state reverts | Data volume (same directory) |
| `~/.local/bin/optic-daemon`, `~/.local/bin/web/` | deploy script (`setup-optic-daemon-phase-01.sh`, `--assets` path) | **Breaks deploys**: the new build runs until reboot, then the old one returns | Data volume (bind mount) |
| `/etc/systemd/system/optic-daemon.service`, `/etc/systemd/system/systemd-time-wait-sync.service.d/optic-bounded-wait.conf` | `scripts/setup-optic-daemon-system-service.sh` (root), only when the unit changes; deploys never write them and refuse on drift (since 2026-09-21, `docs/optic-daemon-system-service.md`) | Unit changes revert at reboot | Keep on root; change only with the overlay off (§4) |
| `/mnt/capture` (JPEG/DNG, `.part` files, `*.log.json`, `preview_config.json`) | daemon captures and sync deletes; Phase 6 tmpfs | Unaffected: separate tmpfs, already volatile by design | No change |
| `/dev/shm/optic-daemon/` (config and run-state cache) | daemon: hydrated at start from the durable copy | Unaffected: tmpfs, rebuilt each start | No change |
| `/var/log/journal/` (persistent journal, 16 MiB cap) | journald | **Breaks the Phase 1 goal**: crash evidence lost at reboot; up to 16 MiB of RAM | Data volume (bind mount) |
| `/var/lib/systemd/timesync/clock` | systemd-timesyncd: last-sync timestamp | Lost at reboot. After a power loss the clock may start at the image's old time until NTP syncs, so early scheduled captures get wrong times | Data volume (bind mount); or accept, depending on RTC battery (§3.4) |
| `/etc/localtime` | `timedated`, via the Station timezone save (`POST /api/system/timezone`) | Change lost at reboot; system timezone drifts from the saved Station timezone | Change the timezone only with the overlay off, or a later daemon change to re-apply it at startup (not in this track) |
| `~/.local/src/optic-daemon-*`, `~/.cache/optic-*` (incl. `optic-daemon-target`), `~/.cargo`, `~/.rustup`, `~/.local/optic-sysroot`, `~/biome` | `build-deploy-optic-daemon.sh` full build on the Pi. Measured 2026-09-20: `~/.cache` 8.2G, `~/.local/src` 2.3G, `~/.rustup` 513M, `~/.local/optic-sysroot` 197M, `~/.cargo` 176M | **Dangerous**: a build writes GBs, which would exhaust the RAM upper layer | Never build with the overlay on (§4) |
| `/var/backups/optic-hardening/`, `/etc/...` drop-ins, `/boot/firmware/config.txt` | setup scripts | Changes lost at reboot | Run setup scripts only with the overlay off (§4) |
| `/var/lib/systemd/linger/liam`, `/etc/polkit-1/rules.d/6*-optic-*` | setup scripts, once | Read-only at runtime: fine | No change |
| `~/.ssh/optic_capture_known_hosts`, `~/.ssh/optic_capture_ed25519` | read by sync with `StrictHostKeyChecking=yes`, so never written | Fine | No change |
| ~~`~/.local/share/beszel-agent/`; `~/.config/beszel/`~~ | Removed with Beszel on 2026-10-07 (`worklogs/2026-10-07-remove-beszel.md`) | n/a | n/a |
| OS churn: DHCP leases, `/var/lib/systemd/random-seed`, timer stamps, shell history, `/var/tmp` | OS | Lost at reboot; small RAM use | Accept |

`OPTIC_CAPTURE_LOG_DB` and `OPTIC_WEB_ASSETS_DIR` could move these paths
without bind mounts, but that would change the unit's `ExecStart=` and
`ReadWritePaths=`. Bind mounts keep every path, the unit, and the deploy
scripts exactly as they are today.

## 3. Option B design

### 3.1 The data volume

A dedicated ext4 filesystem mounted at `/srv/optic`, outside the overlay.
Candidate backing devices (decision needed):

| Choice | Pros | Cons |
|--------|------|------|
| **B1. New partition on the SD card** | No new hardware | Shrinking the root partition must happen offline (card in another machine) and is destructive if done wrong; needs a full card image first; the data volume shares the SD card's wear |
| **B2. USB flash drive or USB SSD** | No repartitioning; easy to reverse; wear moves off the boot card | One more part to fail or come loose; must be `nofail` so the Pi still boots without it |

Recommendation: **B2 with a small USB SSD**. It needs no destructive
repartitioning, and rollback means unplugging it. If extra hardware is
unacceptable, use B1 with a verified card image taken first.

Sizing, measured 2026-09-20: the state directory is 372K for 106
`history.db` rows (about 1.2 KB per row on disk including WAL; average
`detail_json` 753 bytes). A year at one capture per minute (about 526,000
rows) is roughly 0.6 GB. Add the journal (16 MiB cap) and `~/.local/bin`
(15M). **2 GB minimum**; 8 GB leaves margin for denser schedules.

The SD card is 238 GB with the root partition 7% used (16G of 235G), so B1
has plenty of room to shrink root. It is still an offline, destructive
operation.

Mount options: `defaults,noatime,nofail,x-systemd.device-timeout=10s`. This
fits SQLite WAL's crash-safety model (`docs/optic-daemon-capture-log.md` §3.2).
Label the filesystem `optic-data` and mount by `LABEL=`.

### 3.2 Layout and bind mounts

```text
/srv/optic/                      ext4, LABEL=optic-data
├── state/optic-daemon/          → bind onto /home/liam/.local/state/optic-daemon   (liam:liam 0700)
├── bin/                         → bind onto /home/liam/.local/bin                  (liam:liam 0755)
├── journal/                     → bind onto /var/log/journal                       (root:systemd-journal 2755)
└── timesync/                    → bind onto /var/lib/systemd/timesync              (systemd-timesync, see §3.4)
```

Proposed `/etc/fstab` additions (written with the overlay **off**):

```fstab
LABEL=optic-data  /srv/optic  ext4  defaults,noatime,nofail,x-systemd.device-timeout=10s  0  2
/srv/optic/state/optic-daemon  /home/liam/.local/state/optic-daemon  none  bind,nofail,x-systemd.requires-mounts-for=/srv/optic  0  0
/srv/optic/bin                 /home/liam/.local/bin                 none  bind,nofail,x-systemd.requires-mounts-for=/srv/optic  0  0
/srv/optic/journal             /var/log/journal                      none  bind,nofail,x-systemd.requires-mounts-for=/srv/optic  0  0
```

System fstab mounts come up before `local-fs.target`, so they exist before
`optic-daemon.service` starts (a system unit since 2026-09-21, with `RequiresMountsFor=/home/liam`). The
journal moves from `/run` to `/var/log/journal` in
`systemd-journal-flush.service`, which waits for that mount.

**Fail loudly when the volume is missing.** With `nofail`, a missing USB drive
leaves the bind targets as plain directories on the overlay, and the daemon
would write its state into RAM again without saying so. Proposed guard (a
`systemd/optic-daemon.service` change, not made yet):

```ini
[Unit]
AssertPathIsMountPoint=/home/liam/.local/state/optic-daemon
```

With it, the daemon refuses to start and `verify.sh` fails, rather than
silently writing to RAM. The trade-off is that the dashboard is down until the
volume is back. That is a user decision: a dashboard with no persistence is
the alternative.

### 3.3 Open decisions

1. Backing device: B1 or B2 (§3.1).
2. ~~Whether `~/.config/systemd/user/` moves to the data volume.~~ Resolved
   2026-09-21: optic-daemon is a system unit in `/etc/systemd/system`,
   installed only by a root script, and deploys already refuse to run when it
   differs from `systemd/` (`docs/optic-daemon-system-service.md`). It stays
   on root and changes only with the overlay off.
3. Whether to add `AssertPathIsMountPoint=` (§3.2).
4. Timesync clock persistence (§3.4).
5. Whether to make `/boot/firmware` read-only (Stage 4).

### 3.4 Clock after power loss

The Pi 5 has an RTC (`verify.sh` Phase 7 checks `rtc0`). It keeps time
through a power loss only with a backup battery, and Phase 7 records the
battery as not configured. Without a battery, the persisted
`/var/lib/systemd/timesync/clock` is what stops the clock from starting at an
old time before NTP syncs. Recommendation: fit an RTC battery (no software
change) **or** bind `timesync/` onto `/var/lib/systemd/timesync`. Observed
2026-09-20: `systemd-timesyncd.service` uses `StateDirectory=systemd/timesync`,
and the only ordering shown is `After=systemd-sysusers.service`. So a bind
mount should come with a drop-in adding
`RequiresMountsFor=/var/lib/systemd/timesync`, so timesyncd never writes into
the underlying directory before the mount exists.

## 4. Deploys once root is read-only

| Deploy type | Overlay on | Why |
|-------------|------------|-----|
| `build-deploy-optic-daemon.sh --assets` (web files only) | **Works** | Writes only `~/.local/bin/web/`, on the data volume |
| Binary install built elsewhere (`setup-optic-daemon-phase-01.sh --binary FILE`) | **Works** if the unit file is unchanged | Binary lives on the data volume |
| Full `build-deploy-optic-daemon.sh` (native Pi build) | **Never** | GBs of toolchain, sysroot, and `target/` writes would fill the RAM upper layer and can OOM the Pi mid-build (the watchdog history in `worklogs/2026-09-18-system-control-panel.md` shows how badly heavy builds already behave) |
| Any `setup-phase-*.sh`, `apt`, fstab or unit edits | **Never** | Changes land in RAM and revert at reboot |

Procedure for a full build or any system change:

1. Disable the overlay (§6.1) and reboot. The data volume stays mounted and
   unchanged throughout.
2. Run the normal deploy or setup script and its verification.
3. Re-enable the overlay (Stage 3 commands) and reboot.
4. Run `verify.sh --phase 9` and the daemon checks from `setup.md` §8F.

Proposed deploy-script guard (not implemented yet; `scripts/` belongs to this
track, so it can land with Stage 3): the full build path and
`setup-optic-daemon-phase-01.sh` refuse to run when `findmnt -n -o FSTYPE /`
is `overlay`. The `--assets` path first checks that `~/.local/bin` is a mount
point.

Building off the Pi (for example, cross-compiling in a Linux VM) would remove
the reboot cycle for binary deploys. It needs new build tooling and is outside
this plan.

## 5. Staged enable procedure

Each stage is a separate, user-approved step. Stop at the first failed check.

### Stage 0: Prerequisites (no host change)

- Decisions in §3.3 made.
- `verify.sh` has no FAILs other than Phase 9's (both are WARN today). The
  Phase 3 watchdog drift in `worklogs/2026-09-20-provisioning.md` is
  resolved first.
- Every hand-installed config is scripted (`setup-phase-08-daemon-host-access.sh`
  and the others) so a rebuild never depends on the overlay's lower layer
  holding lost hand edits.
- A full SD card image taken from the Mac and its checksum recorded.
- A tested way to reach the card from the Mac (SD reader) for §6.2.

### Stage 1: Data volume and bind mounts, overlay still off

1. Attach and format the volume (`mkfs.ext4 -L optic-data`), create the §3.2
   directories with the listed owners and modes.
2. Stop `optic-daemon.service` (dashboard down; scheduler must be `Paused`).
   Copy data preserving attributes (`rsync -aHAX`) from
   `~/.local/state/optic-daemon`, `~/.local/bin`, and `/var/log/journal` into
   the volume. Keep the originals underneath the mount points as the rollback
   copy.
3. Add the fstab lines, `systemctl daemon-reload`, `mount -a`, start the
   daemon, and reboot once (approved reboot) to prove the mounts come up at
   boot.
4. Verify: every bind target is a mount point, `history.db` row count
   unchanged, a test capture adds a row, scheduler rules intact, `journalctl
   --list-boots` shows the pre-reboot boot.
5. Soak at least 72 hours with the overlay still off, including one planned
   power-cycle.

### Stage 2: Guard rails

- Add the §7 checks to `verify.sh` and the §4 guards to the deploy scripts.
  If approved, add `AssertPathIsMountPoint=` to the unit and deploy it.
- Test the negative case: with the volume unplugged or unmounted, the daemon
  refuses to start (if the guard is adopted) and `verify.sh` fails. Then
  restore.

### Stage 3: Enable the overlay (root only; boot stays writable)

1. Run the read-only confirmations listed at the end of §1.
2. If Stage 4 (read-only boot) is wanted, run
   `sudo raspi-config nonint enable_bootro` **now**, while the overlay is
   still off; raspi-config refuses to edit fstab once the overlay is active.
3. `sudo raspi-config nonint enable_overlayfs` (installs `overlayroot`, adds
   `overlayroot=tmpfs` to `cmdline.txt`), then an approved reboot.
4. Verify: `findmnt /` is `overlay`, the data-volume mounts are present, the
   daemon is active at the expected version, a capture adds a `history.db`
   row, a Station save persists across a **second** reboot, and the upper
   layer's usage stays small (`df -h` on the upper mount).
5. Soak at least 7 days: watch upper-layer usage, memory headroom, and
   capture success in Capture History.

### Stage 4 (optional): Read-only boot partition

Only after Stage 3's soak. Because `enable_bootro` refuses to run while the
overlay is active: `disable_overlayfs`, approved reboot, `enable_bootro`,
`enable_overlayfs`, approved reboot. (Or do it in Stage 3 step 2.) Every later
`config.txt` change then needs the reverse cycle first.

## 6. Rollback

### 6.1 Normal (Pi reachable over SSH)

1. `sudo raspi-config nonint disable_overlayfs`, then an approved reboot.
   (It remounts `/boot/firmware` read-write itself when needed.)
2. Confirm `findmnt -n -o FSTYPE /` is `ext4`. The data volume stays in use;
   nothing else changes.
3. Only if the read-only boot should also go: `sudo raspi-config nonint
   disable_bootro` **after** that reboot (it refuses while the overlay is
   active), then another approved reboot.

### 6.2 Emergency (Pi does not boot or SSH is unreachable)

1. Power off. Put the SD card in the Mac; the FAT boot partition mounts
   automatically.
2. Save a copy of `cmdline.txt`, then delete the leading `overlayroot=tmpfs `
   (the text `enable_overlayfs` prepends; §1). Keep it one line. Eject.
3. Boot. Root is plain ext4 again, and the lower layer holds everything as it
   was before Stage 3.

### 6.3 Removing the data volume (undo Stage 1)

1. With the overlay off: stop the daemon, then copy newer state back from
   `/srv/optic/...` to temporary locations (`rsync -aHAX`).
2. Remove the fstab lines, `systemctl daemon-reload`, unmount the binds and
   `/srv/optic`.
3. Move the copied state into the now-exposed original directories, start
   the daemon, and verify row counts and config.

## 7. `verify.sh` checks to add (Phase 9)

All read-only. They land in Stage 2 so they are proven before the overlay is
enabled.

- The data volume is mounted: `findmnt -n -o SOURCE,FSTYPE /srv/optic` is
  `ext4` and `LABEL=optic-data` (FAIL otherwise).
- Each bind target is a mount point backed by `/srv/optic`:
  `findmnt -n /home/liam/.local/state/optic-daemon`, `~/.local/bin`,
  `/var/log/journal` (FAIL otherwise; FAIL is the reason for the Stage 2
  negative test).
- The daemon's `history.db` is writable **and on the data volume**:
  `findmnt -n -T ~/.local/state/optic-daemon/history.db -o TARGET` is the
  bind mount, not `/`.
- Data-volume free space above a threshold (WARN below 20%).
- When root is `overlay`: upper-layer usage below a threshold (WARN at 64 MiB,
  FAIL at 128 MiB), so steady RAM writes are caught before they starve the
  daemon.
- The existing Phase 9 checks stay: root is `overlay`, and boot is `ro` if
  Stage 4 is adopted.
- Phase 1 journal check (updated 2026-09-20): `system.journal` exists, now on
  the bind mount.
