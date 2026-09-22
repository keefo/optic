# Dated Worklog: 2026-09-21 - Pi Services Audit

Status: **audit complete (read-only, observed on the Pi). Part 2 (persist the choices in the bootstrap scripts): deployed by the user on the Pi, verified, and reboot-tested (boot `e91d3b14`, 22:57 PDT); user-accepted 2026-09-21; committed for a PR**

Branch: `pi-services-audit` (Orca worktree).

## Objective

The user asked: "review the current running system services and user
services, and see which ones are safe to disable, and why". Make a read-only
inventory of every running or enabled system and user unit on `optic.local`,
and give each one a verdict with evidence. Nothing is changed on the Pi. The
user decides what to disable and runs the `sudo` commands themselves.

## Scope and Constraints

- The Pi audit is strictly read-only. No `sudo`, no
  `systemctl start/stop/enable/disable/mask`, no `apt`, no reboot, and no
  writes on the Pi.
- No changes to `src/` (other Orca tracks own it).
- Output: this worklog and `docs/pi-services-audit.md`.
- Making the choices permanent in `scripts/setup-phase-04-unused-hardware.sh`
  and `verify.sh` Phase 4 is a separate change, and needs its own approval.

## What the Station Must Keep (from the task and the docs)

`optic-daemon.service` (system, `User=liam`), `beszel-agent.service` (user,
needs linger), `ssh`, the network link actually in use and its manager,
whatever resolves `imacpro.local`, time sync (`systemd-time-wait-sync` with
the bounded drop-in), the hardware watchdog, zram, journald (persistent,
16 MiB), polkit, the `/mnt/capture` tmpfs, fan/thermal control, and
libcamera.

Already disabled on purpose (setup.md §1, §4; verify.sh Phase 4): rsyslog,
`bluetooth.service`, `hciuart.service`, `ModemManager.service`,
`cups.service`, the global user units pulseaudio/pipewire (masked), and in
firmware `dtoverlay=disable-bt`, `dtparam=audio=off`, `hdmi_blanking=2`.

## Acceptance Criteria

1. Every running or enabled unit (system and `--user`: services, sockets,
   timers) appears in the doc's table with its function, RAM, whether this
   station uses it (with evidence), a verdict (KEEP / SAFE TO DISABLE /
   DISABLE WITH CARE / ALREADY OFF), and the reason.
2. Each SAFE or CARE unit has the exact command, the expected saving, the
   risk, and the undo command.
3. Failing and noisy units are listed, from `systemctl --failed` and
   `journalctl -b -p warning`.
4. Every statement is either observed on the Pi (with the command) or
   marked unverified.
5. The user is asked which items to disable. Nothing is disabled by me.

## Test Plan (written before collection)

This is an audit, so "tests" are evidence checks.

- T1 Collection runs only read-only commands. Review the command list
  before running: no `sudo`, no state-changing `systemctl` verbs, no
  redirection to files on the Pi.
- T2 Unit coverage: every unit in
  `systemctl list-units --state=running` and
  `systemctl list-unit-files --state=enabled` (system and `--user`, types
  service/socket/timer) appears in the doc table. Check by diffing the unit
  names in the raw output against the table.
- T3 The network verdict is backed by `ip -br addr`, the default route and
  the active manager's own status (nmcli/networkctl/dhcpcd). Wi-Fi items
  are marked SAFE only if the default route is not over `wlan0`.
- T4 The resolver verdict is backed by `getent hosts imacpro.local` and by
  which of avahi / systemd-resolved is running with nss-mdns in
  `/etc/nsswitch.conf`.
- T5 The apt timers verdict is backed by `apt-config dump | grep Periodic`.
- T6 RAM figures come from `systemctl show -p MemoryCurrent` (memory
  cgroup is enabled per setup.md §2) and `ps` RSS. Units with no figure are
  marked as such.
- T7 Failure case: if SSH to `optic.local` fails after two retries, record
  it, and mark the audit unverified rather than guessing.

Environment: the live Pi `optic.local`, reached from the Mac over SSH as
`liam`.

## Collection

Environment: `optic.local`, Debian 13 trixie, kernel `6.18.50+rpt-rpi-2712`,
boot `35ca4964` (up 50 min), 2026-09-21 22:09–22:14 PDT. Reached from the
Mac with `ssh -o BatchMode=yes -o ConnectTimeout=8 liam@optic.local 'bash -s' < collectN.sh`.
The scripts were piped over stdin and saved nothing on the Pi; the output
went to the session scratchpad. SSH connected on the first try every time.

T1 (read-only): the three collection scripts only use `systemctl
list-*/show/cat/is-*`, `journalctl`, `loginctl show-*`, `ip`, `nmcli -t
… show/status/list --rescan no`, `networkctl list`, `getent`, `timedatectl`,
`ss`, `lsmod`, `lsblk`, `cat /proc/swaps` (bare `swapon` is not on liam's SSH PATH), `cloud-init status`,
`rpi-eeprom-update` (no flags: status only), `apt-config dump`, `dpkg -l/-S`,
`ls`, `cat`, `grep`. No sudo and no redirection on the Pi. Wi-Fi secrets were
redacted (`sed`) before printing `/boot/firmware/network-config`.

| Group | Commands | Observed |
|-------|----------|----------|
| Units | `systemctl [--user] list-units --type=service,socket,timer`, `list-unit-files --state=…`, `list-timers --all` | 16 running system services, 8 system timers, 0 user timers. User: beszel-agent, dbus, mpris-proxy + 7 sockets |
| Failed | `systemctl [--user] --failed` | none / none |
| Linger | `loginctl show-user liam -p Linger` | `Linger=yes` |
| Memory | `systemctl show -p MemoryCurrent` per unit, `ps --sort=-rss`, `free -m` | used 513 / 990 MiB; optic-daemon 56.5 MiB cgroup (RSS 123 MiB); user@1000 19.7 MiB (beszel 14.2) |
| Network | `ip -br addr`, `ip route`, `nmcli`, `networkctl`, `dhcpcd` | Wi-Fi `wlan0` default route, `eth0` DOWN; NetworkManager + netplan; networkd/dhcpcd unused |
| Radio | `iw`, `rfkill` | **not installed** on the Pi; used `/sys/class/rfkill` (`phy0 wlan soft=0 hard=0`, no bluetooth rfkill) and `nmcli device wifi list --rescan no` instead |
| Resolver | `resolvectl`, `getent hosts imacpro.local`, `nsswitch.conf` | resolved inactive; `imacpro.local` → 192.168.0.202 via `mdns4_minimal` (avahi) |
| Time | `timedatectl`, `show-timesync`, `systemctl cat systemd-time-wait-sync` | synced; bounded drop-in active; wait 31.5 s this boot |
| Journal | `journalctl -b -p warning` | 11 entries (netplan perms 4, alsa udev rule 4, wpa_supplicant 2, firmware 1); previous boot 13 |
| Firmware | `/boot/firmware/config.txt` | `disable-bt`, `audio=off`, `hdmi_blanking=2`, fan curve present |
| Baseline | `verify.sh --phase 4`, `--phase 6` (read-only) | Phase 4 PASS=12 FAIL=0; Phase 6 PASS=13 FAIL=0 INFO=1 |

T2 (coverage): extracted 90 unit names from the raw output (running,
enabled, active) and grepped each one in `docs/pi-services-audit.md`. The
first pass missed 20 (shorthand such as `gpg-agent{,-ssh}` and unlisted
static boot oneshots). I spelled them out, and the second pass missed only
three non-units (`NEXT`, `Sun`, `Tue`, header words from the timer table).
T3–T6 are satisfied; the evidence is in the doc's §1. T7: not triggered.

## Findings

- **SAFE TO DISABLE:** `apt-daily.timer` and `apt-daily-upgrade.timer`
  (`APT::Periodic` unset); `man-db.timer`; `e2scrub_all.timer` and
  `e2scrub_reap.service` (no LVM); user `mpris-proxy.service` (a Bluetooth
  proxy, enabled globally); `udisks2.service` (not running; no saving).
- **DISABLE WITH CARE:** cloud-init (65.3 MiB peak at boot; unverified
  whether it re-applies `network-config` each boot); `getty@tty1`
  **autologin as liam** (security, not RAM); `cron.service` (root crontab
  unverified); removing the retired `optic-capture-transfer` files.
- **KEEP:** everything the station depends on. Specifically,
  `wpa_supplicant` stays because Wi-Fi is the real link, and
  `NetworkManager-wait-online` stays because the daemon wants
  `network-online.target`.
- Total steady RAM from all SAFE items: under 1 MiB. The gains are fewer
  wakeups and SD writes, fewer moving parts, and a faster, lighter boot
  (cloud-init).
- Info only: a newer bootloader EEPROM (2026-05-26) is available, but the
  boot service does not auto-apply it (current ≥ minimum). `nftables` is
  disabled, so :22 and :8000 are open to the LAN. Not changed.

## Mismatches Found

- `README.md` said optic-daemon "runs as `liam`'s persistent systemd user
  service". It has been a system service since 2026-09-21 (observed: running
  as `optic-daemon.service` in the system manager, `User=liam`). Fixed.
- setup.md §6 already documents that `setup-phase-06-pi-ram-transfer.sh`
  reinstalls the retired transfer timer. Not changed here.

## Files Changed

- `worklogs/2026-09-21-pi-services-audit.md` (new)
- `docs/pi-services-audit.md` (new)
- `README.md` (one sentence: system service, not user service)
- `CLAUDE.md` (= `AGENTS.md`): two tooling bullets (`swapon` PATH; `iw`/`rfkill` not installed)

No changes in `src/`, `scripts/`, `systemd/` or `verify.sh`. Nothing
changed on the Pi.

## Limitations and Next Steps

- Unverified: root's crontab (the Phase 4 script now checks it as root);
  `udisks2`'s `WantedBy=`; whether cloud-init re-applies `network-config`
  each boot; the meaning of firmware request `0x00030097`. Resolved in
  Part 2: `passwd -S liam` reports `P` (usable password set).
- `iw` and `rfkill` are not installed on the Pi; I used sysfs and nmcli
  instead.
- Next: the user picks items (question asked in the session). The user runs
  the `sudo` commands from `docs/pi-services-audit.md` §3, then runs
  `verify.sh`. After that, and if approved as a separate change, the choices
  go into `scripts/setup-phase-04-unused-hardware.sh` and `verify.sh`
  Phase 4 (and §3.8 into the Phase 6 script).
- User verification: review the doc's verdicts. After any change, run
  `ssh liam@optic.local 'bash -s -- --no-color' < verify.sh` and check
  that the dashboard, sync and Beszel still work.

---

# Part 2: Persist the Choices in the Bootstrap Scripts

## Request

After reading the audit, the user asked: "can we add those service
disablement into our bootstrap scripts? because in long term we might need
to rebuild our optic pi box". That was the answer for both the SAFE and the
CARE items. So every item in `docs/pi-services-audit.md` §3 goes into the
provisioning scripts, with matching `verify.sh` checks. Applying the change
to the live Pi is still the user's step: they run the updated script with
`sudo`.

## Acceptance Criteria

1. `scripts/setup-phase-04-unused-hardware.sh` also, idempotently:
   - disables and stops `apt-daily.timer`, `apt-daily-upgrade.timer`
     (skipped if any `APT::Periodic::` value is configured), `man-db.timer`,
     `e2scrub_all.timer` and `e2scrub_reap.service` (skipped if `/` is on
     device-mapper/LVM), and `udisks2.service`;
   - globally disables the user unit `mpris-proxy.service` and stops the
     running instance for `liam`;
   - disables `cron.service` only if `/var/spool/cron/crontabs` holds no
     crontab; otherwise it skips and lists them;
   - creates `/etc/cloud/cloud-init.disabled` only if cloud-init is
     installed and `cloud-init status` reports `done`;
   - moves `getty@tty1` autologin drop-ins (those containing `--autologin`)
     into `/var/backups/optic-hardening/`, only if `liam` has a usable
     password (`passwd -S` state `P`); otherwise it skips with a warning;
   - leaves absent units alone; `--dry-run` reports `[OK]`, `[CHANGE]` or
     `[SKIP]` per item and changes nothing.
2. `scripts/setup-phase-06-pi-ram-transfer.sh` no longer installs or
   enables the retired shell transfer (sender, config, service, timer).
   If those files are present, it stops the timer and service and moves the
   files into a timestamped backup directory. The tmpfs, key, host-key pin
   and Beszel drop-in are unchanged. It no longer needs
   `optic-capture-transfer.sh` next to it.
3. `verify.sh` Phase 4 checks each new item: FAIL for the unconditional
   ones, WARN for the guarded ones (cron, cloud-init, autologin) and for
   apt when `APT::Periodic` is set. Phase 6 FAILs while retired transfer
   files remain. All existing checks are unchanged.
4. Docs: setup.md §4 and §6, and `docs/pi-services-audit.md` (§3 and §5).

## Test Plan (written before implementation)

Local (Mac):
- S1 `bash -n` on both scripts and `verify.sh`.
- S2 `--help` for both scripts runs locally and lists the new behavior.
- S3 shellcheck: not installed on the Mac (`command -v shellcheck` empty),
  so it is recorded as unavailable.

Pi, read-only (no sudo; the scripts are piped over stdin; nothing is
written):
- P1 `bash -s -- --dry-run < scripts/setup-phase-04-unused-hardware.sh`.
  Expected: `[OK]` for bluetooth and firmware; `[CHANGE]` for the apt
  timers, man-db, e2scrub_all, e2scrub_reap, udisks2, mpris-proxy,
  cloud-init and autologin (or `[SKIP]` if `liam` has no usable password).
  For cron, a notice that the check needs root (as non-root the spool is
  unreadable).
- P2 `bash -s -- --dry-run < scripts/setup-phase-06-pi-ram-transfer.sh`.
  Expected: `[OK]` for the mount, Beszel drop-in, key and pinned host key;
  `[CHANGE]` to retire the 5 leftover files. No `[BLOCKED]` for the missing
  sender helper.
- P3 `verify.sh --phase 4` and `--phase 6`. Expected: the old 12 and 13
  checks still PASS, and the new checks FAIL or WARN for exactly the items
  not yet applied.

Pi, applying (the user's step, with `sudo`): the Phase 4 script, then the
Phase 6 script, then `verify.sh` (all phases) is all PASS, apart from WARNs
for items the guards skipped. A reboot at a time the user chooses confirms
that Wi-Fi comes back with cloud-init disabled.

## Implementation

- `scripts/setup-phase-04-unused-hardware.sh`:
  - New helpers `unit_off` (absent, or stopped and disabled/masked; the
    Bluetooth check now uses it with the same semantics),
    `global_user_unit_off`, `apt_periodic_configured`, `root_on_lvm`,
    `cron_jobs_needing_cron`, `cloud_init_off`/`cloud_init_status`,
    `autologin_dropins` and `service_user_password_state`.
  - `--dry-run` reports `[OK]`/`[CHANGE]`/`[SKIP]`/`[CHECK]` for each item.
    The apply path disables each item, collects skipped ones, re-asserts
    the end state and prints skipped items as warnings. `--help` lists
    everything.
  - Autologin: the drop-in is moved to
    `/var/backups/optic-hardening/getty-tty1-<name>.phase-04.<time>.bak`,
    then `daemon-reload`. The getty is restarted, except when the script
    itself runs on tty1.
  - **Pre-existing bug fixed:** `render_firmware_config` always moved the
    Phase 4 block to the end of `config.txt`. After Phase 5 appends its
    block, every Phase 4 rerun saw a mismatch, rewrote `config.txt` and
    wanted a reboot. Observed: the `HEAD` script's dry-run on the Pi said
    `[CHANGE] Back up and update target directives`, although `verify.sh`
    passes all three lines. An existing block is now re-rendered in place.
    Fresh-file output is unchanged.
- `scripts/setup-phase-06-pi-ram-transfer.sh`: removed the sender, config,
  service and timer installation and the timer enable; it no longer needs
  `optic-capture-transfer.sh`. It retires leftovers (stop the timer and
  service first, then move the 5 files into
  `/var/backups/optic-hardening/retired-capture-transfer.<time>/`, then
  `rmdir /etc/optic` if empty). The mount unit is unchanged; its
  `Before=optic-capture-transfer.service` line is left alone, so the live
  unit isn't rewritten for a harmless ordering hint.
- `verify.sh` Phase 4: 7 new FAIL-level checks (man-db, udisks2, the 2 apt
  timers unless `APT::Periodic` is set, the 2 e2scrub units unless `/` is
  on LVM, and mpris-proxy globally) and 3 WARN-level checks (cron,
  cloud-init, autologin). Phase 6: FAIL while retired transfer files remain.
- Docs: setup.md §4 (table of disabled services and guards; the in-place
  firmware note) and §6 (scp line, retired-timer note, steps A/C wording);
  `docs/pi-services-audit.md` §3 (apply via the scripts; password state)
  and §5.

## Validation (Part 2)

| Test | Command | Environment | Result |
|------|---------|-------------|--------|
| S1 | `bash -n` on both scripts and `verify.sh` | Mac | PASS |
| S2 | `bash scripts/setup-phase-0{4,6}-*.sh --help` | Mac | PASS, exit 0, new text shown |
| S3 | shellcheck | Mac | **unavailable** (not installed) |
| F1 | Renderer fixtures, new vs `HEAD` (`fw/` in the scratchpad) | Mac (BSD awk) | a) fresh file: byte-identical to `HEAD`; b) block at end: matches; c) Phase 5 block after it: matches (`HEAD`: mismatch, bug reproduced); d) drifted value inside the block: repaired in place; e) stray `dtparam=audio=on` after the block: removed |
| P1 | `ssh … 'bash -s -- --dry-run' < scripts/setup-phase-04-unused-hardware.sh` | Pi, as `liam`, read-only | exit 0. `[OK]` bluetooth, `[OK] config.txt already matches` (was `[CHANGE]` before the renderer fix); `[CHANGE]` for man-db, udisks2, both apt timers, both e2scrub units, mpris-proxy, cloud-init and autologin; `[CHECK]` for cron (the spool is root-only) |
| P1b | `… --dry-run < scripts/setup-phase-05-active-cooler.sh` (unchanged script) | Pi, read-only | `[OK] config.txt already matches the fan policy`, so the two phases no longer fight |
| P2 | `… --dry-run < scripts/setup-phase-06-pi-ram-transfer.sh` | Pi, read-only | exit 0. `[OK]` mount, Beszel drop-in, key, pinned host key; `[CHANGE] Retire` for exactly the 5 leftover files; no `[BLOCKED]` |
| P1c | `cron_jobs_needing_cron` extracted and run with `CRON_SPOOL=/nonexistent-spool`, then with the real spool | Pi, as `liam`, read-only | `[]`: no `/etc/cron*` job needs cron. Real spool as liam: `[unreadable]`, so the apply run (root) decides from the root crontab |
| P3 | `verify.sh --phase 4`, `--phase 6` | Pi, read-only | Phase 4: PASS=12 WARN=3 FAIL=7. The 12 original checks still PASS; the 7 FAILs and 3 WARNs are exactly the items not yet applied. Phase 6: PASS=13 FAIL=1, the new retired-files check listing the 5 files |

Not done (the user's step, or needs approval): running the scripts with
`sudo` on the Pi, the verify run after applying (expected all PASS, apart
from a WARN for cron if the root crontab guard skips it), and the reboot
that confirms Wi-Fi without cloud-init.

## Files Changed (Part 2)

- `scripts/setup-phase-04-unused-hardware.sh`
- `scripts/setup-phase-06-pi-ram-transfer.sh`
- `verify.sh`
- `setup.md`
- `docs/pi-services-audit.md`
- this worklog

## Remaining Limitations (Part 2)

- `scripts/optic-capture-transfer.sh` is now unused in the repo (only
  `src/optic_sync.rs` mentions it, in a history comment). It was not
  deleted: deleting files needs the user's approval.
- setup.md §6 D and §8 C still describe the old timer's behavior
  ("waits at least 10 seconds", "after approximately 30 seconds"). Whether
  `optic_sync` behaves the same was not verified, so that text was left
  unchanged.
- Idempotency of the apply path (second run = all "already") has not been
  observed on the Pi, because applying is the user's step.

---

# Part 3: Wi-Fi Setup Proposal (design only)

The user asked whether Bluetooth could be used to set up Wi-Fi from an
iPhone on a new box, so that no service needed later gets disabled now.
Read-only check on the Pi: `nmcli -f WIFI-PROPERTIES device show wlan0`
gives `AP: yes`, `5GHZ: yes`; `dnsmasq-base` 2.91 and `bluez` 5.82 are
installed; `rpi-usb-gadget-ics.service` is disabled. At the user's request,
I wrote `docs/wifi-onboarding.md` (proposal, not implemented). It
recommends a setup hotspot with a captive page, with Ethernet and a
dashboard form as the fallback. BLE would need an iPhone app. None of the
audit's disable items blocks these options; Bluetooth stays reversible.
The user chose to run the Part 2 scripts as planned.

---

# Part 2 Deployment (by the user, 2026-09-21 22:54 PDT)

The user ran the updated scripts with sudo and pasted the output.

- Phase 4 apply: disabled bluetooth (again; see below), man-db.timer,
  udisks2, both apt timers, e2scrub_all.timer, e2scrub_reap, mpris-proxy
  (global), and cron ("every cron job defers to systemd", so the root
  crontab guard passed). Created `/etc/cloud/cloud-init.disabled` and moved
  the autologin drop-in to
  `/var/backups/optic-hardening/getty-tty1-autologin.conf.phase-04.20260921T225437.bak`.
  `config.txt already matches the plan`: no firmware write and no reboot,
  which confirms the renderer fix on the real file. No skipped items.
- Phase 6 apply: retired the 5 files into
  `/var/backups/optic-hardening/retired-capture-transfer.20260921T225447/`.
- `verify.sh` (all phases): **PASS=137 WARN=2 FAIL=2 INFO=6.** Phase 4:
  22/22 PASS; Phase 6: 15 PASS (including the new retired-files check) and
  1 INFO. The 2 FAILs are Phase 3 (`RuntimeWatchdogSec` 90 s vs the planned
  15 s). They come from the hand-installed
  `/etc/systemd/system.conf.d/90-watchdog-headroom.conf`, which predates
  this work and is documented in setup.md §8G and
  `worklogs/2026-09-20-provisioning.md`. Not caused by this change. The 2
  WARNs are Phase 9 OverlayFS (deferred by design).

My read-only follow-up, 22:55:

- Idempotency: both dry-runs again report all `[OK]`, with no `[CHANGE]`.
- `systemctl --failed` (system and user): none. `optic-daemon` active,
  `/healthz` 200. `beszel-agent` active. `getty@tty1` active, with no
  autologin session (`who` lists only the SSH session). `cron`, `udisks2`
  and user `mpris-proxy` inactive. 4 system timers remain (was 8).
  `/etc/optic` is gone.
- Cosmetic, pre-existing: the original Bluetooth block runs
  `systemctl disable --now` whenever the unit is loaded, so every run
  prints "Disabled bluetooth.service". Harmless; not changed.

# Reboot Test (the user rebooted; my checks were read-only, 22:57 PDT)

Boot `35ca4964` ended 22:56:59; boot `e91d3b14` started 22:57:12.

| Check | Observed |
|-------|----------|
| cloud-init | `cloud-init status`: `disabled`; its units inactive; no cloud or e2scrub entries in `systemd-analyze blame` |
| Wi-Fi | `wlan0 UP 192.168.0.195/24`, default via `wlan0`, active connection `netplan-wlan0-…`. `eth0` DOWN, as before |
| Resolver | `getent hosts imacpro.local` → `192.168.0.202` |
| Time and order | synchronized, NTP active. `optic-daemon` @32.612 s after `time-sync.target` @32.609 s (the wait took 31.5 s). Boot 1.4 s + 32.6 s, the same as before |
| Daemon | `/healthz` 200, version `0.1.31`, sync `enabled=True, connectivity=idle, last_error=None` |
| Services | no failed units (system or user). optic-daemon, getty@tty1, avahi, ssh and NetworkManager active; beszel-agent active; user mpris-proxy inactive. `who` shows only the SSH sessions: no tty1 autologin |
| Timers | 4 left: systemd-tmpfiles-clean, dpkg-db-backup, logrotate, fstrim |
| Memory | 1 min after boot: used 510 MiB, available 480 MiB, swap 0 used. Not comparable to the earlier 513 MiB (50 min uptime, different load), so no saving is claimed |
| Journal warnings | 11, the same known sources as before (netplan permissions 4, alsa udev rule 4, wpa_supplicant 2, firmware 1). The cloud-init `degraded` warning is gone |
| `verify.sh` (all) | PASS=137 WARN=2 FAIL=2 INFO=6. The 2 FAILs are the pre-existing Phase 3 watchdog override; the 2 WARNs are Phase 9 |

User acceptance: given 2026-09-21 ("Accept: commit + open PR").
Remaining: the pre-existing items listed in Part 2's limitations.
