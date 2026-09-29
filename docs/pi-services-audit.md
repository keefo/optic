# Pi Services Audit (systemd system and user units)

Status: **read-only audit of `optic.local`, 2026-09-21 22:09–22:14 PDT**
(boot `35ca4964`, up 50 min, daemon deployed at 21:44). Nothing on the Pi was
changed. Worklog and raw commands: `worklogs/2026-09-21-pi-services-audit.md`.

Verdicts:

- **KEEP**: the station needs it, or disabling it gains nothing.
- **SAFE TO DISABLE**: this station does not use it (see the evidence), and
  disabling it cannot break capture, the web UI, sync, SSH, or monitoring.
- **DISABLE WITH CARE**: not needed in normal operation, but there is a
  risk or an unverified point. Read that unit's section first.
- **ALREADY OFF**: disabled, masked, or not installed.

RAM is the unit's cgroup `MemoryCurrent` (the memory cgroup is enabled,
setup.md §2). A dash means the unit has no running process. Anything not
observed on the Pi is marked **unverified**.

## 1. Baseline Facts (observed)

| Fact | Evidence |
|------|----------|
| Memory | `free -m`: total 990, used 513, available 476 MiB; zram swap 494 MiB, 29 MiB used. `/proc/swaps`: `/dev/zram0` is the only swap (the three extra `.swap` units are udev aliases of it) |
| Network link is **Wi-Fi** | `ip -br addr`: `wlan0 UP 192.168.0.195/24`, `eth0 DOWN` (carrier 0). `default via 192.168.0.1 dev wlan0`. nmcli: `wlan0:wifi:connected:netplan-wlan0-…`, 5180 MHz, signal 82 |
| Network manager | NetworkManager 1.52.1 (active), with connections generated from netplan (`/run/NetworkManager/system-connections/netplan-*.nmconnection`). `systemd-networkd` disabled; `networkctl` shows every link `unmanaged`; `dhcpcd` inactive |
| Wi-Fi profile on disk | `/etc/netplan/90-NM-af6538bb-….yaml` (root 0600). Its UUID matches the active connection `af6538bb-…`. NetworkManager rewrote it at 21:18:58, when NM started |
| `imac.local` resolution | `getent hosts imac.local` → `192.168.0.231` (`192.168.0.202` when audited). `nsswitch`: `files mdns4_minimal [NOTFOUND=return] dns` (libnss-mdns, which queries **avahi-daemon**). `systemd-resolved` inactive; `/etc/resolv.conf` → `192.168.0.1` |
| Beszel hub address | `beszel-agent` uses `HUB_URL=http://192.168.0.231:8090` (an IP, not mDNS). Was `192.168.0.202` when audited; the Mac's DHCP lease moved and the hub was unreachable for days (`worklogs/2026-09-29-beszel-hub-address.md`) |
| Time | `timedatectl`: synchronized yes, NTP active (timesyncd, `2.debian.pool.ntp.org`). `systemd-time-wait-sync` enabled with the `optic-bounded-wait.conf` drop-in active; it took 31.5 s this boot. `critical-chain`: `optic-daemon` ← `time-sync.target` @32.6 s |
| Linger | `loginctl show-user liam`: `Linger=yes` |
| Listening ports | TCP `0.0.0.0:8000` (optic-daemon), TCP `22` v4 and v6 (sshd), UDP `5353` (avahi), plus two ephemeral UDP ports. `nftables` is disabled (no host firewall) |
| Bluetooth / audio | `/sys/class/bluetooth` absent; no bluetooth modules in `lsmod`. Only the HDMI sound codec is loaded (`snd_soc_hdmi_codec`, used by `vc4`). The firmware lines `dtoverlay=disable-bt`, `dtparam=audio=off` and `hdmi_blanking=2` are present |
| AppArmor | `/sys/module/apparmor/parameters/enabled` = `N`; LSM list = `capability` |
| APT periodic | `apt-config dump \| grep -i periodic` returns nothing; `unattended-upgrades` not installed. `apt-daily.service` last ran 20:13:20 and finished in 2 s |
| Storage | `mmcblk0p2` ext4 `/` on a plain partition (no LVM: `/usr/sbin/lvs` and `/sbin/lvm` absent). SD card supports discard (`DISC-GRAN 16M`) |
| Failed units | `systemctl --failed` and `systemctl --user --failed`: none |
| Hardening checks | `verify.sh --phase 4`: PASS=12 FAIL=0. `--phase 6`: PASS=13 FAIL=0 (retired transfer timer not running) |
| Boot time | `systemd-analyze`: 1.4 s kernel + 32.7 s userspace; the NTP wait is 31.5 s of it |

## 2. Unit Inventory

### 2.1 System services, running

| Unit | What it does | RAM | Used here? Evidence | Verdict | Why |
|------|--------------|-----|---------------------|---------|-----|
| `optic-daemon.service` | Camera, web UI :8000, scheduler, `optic_sync` | 56.5 MiB (RSS 123 MiB) | Yes. Listens on :8000; `verify.sh` Phase 6 sync PASS | KEEP | The station's purpose |
| `ssh.service` | OpenSSH server | 9.6 MiB | Yes. Listens on :22; this audit used it | KEEP | Only remote access path |
| `NetworkManager.service` | Manages `wlan0` (DHCP, Wi-Fi) | 6.2 MiB | Yes. Owns the default route over `wlan0` | KEEP | The network link |
| `wpa_supplicant.service` | Wi-Fi authentication for NM | 0.8 MiB | Yes. Wi-Fi is the real link (`eth0` DOWN) | KEEP | Disabling it would drop the Pi off the network |
| `avahi-daemon.service` (+ `.socket`) | mDNS: announces `optic.local`, resolves `.local` | 1.5 MiB | Yes. `optic.local` is how the Mac reaches it; `imac.local` for `optic_sync` resolves via `mdns4_minimal` → avahi | KEEP | Needed both ways; `verify.sh` Phase 4 requires it |
| `systemd-timesyncd.service` | NTP client | 1.3 MiB | Yes. Synchronized; the dashboard's NTP button restarts it | KEEP | No RTC battery |
| `systemd-timedated.service` | D-Bus time/timezone API (on demand) | 2.2 MiB | Yes. Started 21:44:06, with the daemon; the dashboard timezone uses it | KEEP | Exits by itself when idle |
| `polkit.service` | Authorization for the D-Bus calls | 5.2 MiB | Yes. Rules 60–64 (reboot, NTP, timezone, power-off, daemon restart) | KEEP | Dashboard buttons and unprivileged deploys |
| `dbus.service` (+ `.socket`) | System message bus | 4.8 MiB | Yes. logind, timedated, polkit and NM all use it | KEEP | Core |
| `systemd-journald.service` (+ `systemd-journald.socket`, `systemd-journald-dev-log.socket`) | Journal, persistent, 16 MiB cap | 5.5 MiB | Yes. `journalctl --disk-usage` 14.7 MiB | KEEP | Crash evidence (Phase 1) |
| `systemd-logind.service` | Sessions, reboot/power-off | 4.6 MiB | Yes. Power menu via polkit rules 60/63 | KEEP | Core |
| `systemd-udevd.service` (+ `systemd-udevd-control.socket`, `systemd-udevd-kernel.socket`) | Device manager | 6.1 MiB | Yes. Camera, `/dev/media*`, `/dev/video*` nodes | KEEP | Core |
| `cron.service` | Classic cron | 0.5 MiB | Nothing found: every `/etc/cron.*` script and `/etc/cron.d/e2scrub_all` exits when systemd is running; `liam` has no crontab. **Root's crontab unverified** (`/var/spool/cron/crontabs` unreadable without sudo) | DISABLE WITH CARE | §3.7 |
| `getty@tty1.service` | HDMI console login, **with autologin as `liam`** | 0.5 MiB + session 0.4 MiB | Only if a keyboard and screen are attached. `who`: `liam tty1` logged in since boot | DISABLE WITH CARE (the autologin, not the getty) | §3.6 |
| `serial-getty@ttyAMA10.service` | Login on the Pi 5 debug UART | 0.2 MiB | Not in use now. It is the rescue console if Wi-Fi fails | KEEP | Cheapest recovery path on a headless box |
| `user@1000.service` | `liam`'s user manager | 19.7 MiB (beszel-agent is 14.2 of it) | Yes. Hosts beszel-agent; needs linger | KEEP | Beszel |

### 2.2 System services, enabled (boot oneshots and on-demand)

| Unit | What it does | RAM | Used here? Evidence | Verdict | Why |
|------|--------------|-----|---------------------|---------|-----|
| `NetworkManager-wait-online.service` | Holds `network-online.target` until NM connects | – | Yes. `optic-daemon` has `Wants`/`After=network-online.target`; took 6.0 s, in parallel with the 31.5 s NTP wait | KEEP | Disabling it lets the daemon start before Wi-Fi is up, with no boot-time gain |
| `NetworkManager-dispatcher.service` | Runs NM hook scripts on demand | – | Not started this boot | KEEP | D-Bus activated, zero RAM, part of NM |
| `systemd-time-wait-sync.service` | Holds `time-sync.target` until NTP sync, bounded to 90 s | – | Yes. Intentional (docs/optic-daemon-system-service.md §2.3) | KEEP | Correct clock for the first capture |
| `systemd-zram-setup@zram0.service` | Creates zram swap | – | Yes. `/dev/zram0` 495 MiB | KEEP | Phase 2 |
| `cloud-init-main.service`, `cloud-init-local.service`, `cloud-init-network.service`, `cloud-config.service`, `cloud-final.service` (+ `cloud-init-hotplugd.socket`) | First-boot provisioning from `/boot/firmware/{user-data,network-config}` (Raspberry Pi Imager) | – steady; **65.3 MiB peak** at boot ("Consumed 698ms CPU time, 65.3M memory peak") | Per-instance modules last ran 2026-09-14/15 (`/var/lib/cloud/instance/sem`); per-boot stages still run every boot, until 34 s. Status `degraded done`: missing module `cc_netplan_nm_patch` | DISABLE WITH CARE | §3.5 |
| `rpi-eeprom-update.service` | Boot check of the bootloader EEPROM (`-s -a`) | – | Ran: "Skipping automatic bootloader upgrade. current 1756339608 >= min 1746713597". It flashes only if the bootloader is older than a minimum | KEEP | Safety net, no routine flashing. A newer bootloader (2026-05-26) is available; applying it is a separate decision |
| `e2scrub_reap.service` | Removes stale LVM snapshots left by e2scrub | – (0.43 s boot) | No. Root is not on LVM; `lvs` not installed | SAFE TO DISABLE | §3.3 |
| `sshswitch.service` | Enables ssh if `/boot/firmware/ssh` exists | – | Ran; ssh is enabled anyway | KEEP | No-op, nothing to gain |
| `regenerate_ssh_host_keys.service` | Regenerates host keys on first boot only | – | `ConditionFirstBoot=yes`, not met | KEEP | No-op; correct if the image is ever cloned |
| `sshd-keygen.service`, `sshd-unix-local.socket` | Host-key creation if missing; local AF_UNIX sshd socket | – | No keys missing; socket idle | KEEP | Part of OpenSSH, no gain |
| `systemd-pstore.service` | Saves kernel crash records from pstore | – | Condition not met (no pstore data) | KEEP | Would hold panic evidence; zero cost |
| `udisks2.service` | Desktop disk-mount D-Bus service | – | Not started this boot (inactive, no start time); headless, no removable media | SAFE TO DISABLE (no current saving) | §3.4 |
| `apparmor.service` | Loads AppArmor profiles | – | Kernel has AppArmor off (`N`), so the unit is skipped | KEEP (no-op) | Disabling it gains nothing |
| `console-setup`, `keyboard-setup` | Console font and keymap | – | Boot oneshots, 0.15 s | KEEP | Needed for a usable local/rescue console |
| `alsa-restore.service` (static) | Restores ALSA mixer state | – | Ran at boot (only an HDMI codec exists) | KEEP (no gain) | ms-scale oneshot. Its udev rule is a noise source (§4); disabling the unit does not silence that |
| `rpi-zram-writeback`, `rpi-resize-swap-file`, `rpi-remove-swap-file@`, `rpi-setup-loop@` (static) | `rpi-swap` helpers (zram writeback, swap files) | – | None active this boot. Phase 2 selects pure zram with no writeback, and `/dev/zram0` is the only swap | KEEP | Static helpers, only run when the `rpi-swap` generator asks for them; nothing to disable |
| Core boot oneshots (static): `kmod-static-nodes`, `systemd-modules-load`, `systemd-sysctl`, `systemd-binfmt`, `systemd-random-seed`, `systemd-journal-flush`, `systemd-tmpfiles-setup`, `systemd-tmpfiles-setup-dev`, `systemd-tmpfiles-setup-dev-early`, `systemd-udev-trigger`, `systemd-udev-load-credentials`, `systemd-user-sessions`, `user-runtime-dir@1000`, `systemd-fsck@…` | systemd's own boot setup (sysctls incl. Phase 2/3 drop-ins, module load, journal flush to disk, `/run/user/1000` for linger) | – | Yes, all ran this boot (`active exited`) | KEEP | Core; static units cannot be disabled |
| Mounts and swap: `mnt-capture.mount`, `tmp.mount`, `boot-firmware.mount`, `run-user-1000.mount`, `dev-zram0.swap` (+ 3 udev aliases) | RAM capture stage, `/tmp`, firmware partition, user runtime dir, zram swap | – | Yes. `/mnt/capture` tmpfs 256 MiB (Phase 6 PASS); `/proc/swaps` shows only `/dev/zram0` | KEEP | Station storage and memory design |
| `netplan-ovs-cleanup.service` | Open vSwitch cleanup (netplan-generated) | – | Condition not met | KEEP | Generated by netplan each boot; no-op |
| `systemd-fsck-root`, `systemd-remount-fs` (enabled-runtime) | Root fsck/remount | – | Ran | KEEP | Core |
| `getty@.service`, `autovt@` (template/alias) | Console gettys | – | See `getty@tty1` | KEEP | – |
| `optic-capture-transfer.service` (static) | Retired shell capture transfer (ships and **deletes** every non-hidden file in `/mnt/capture`) | – | No. Timer disabled and stopped 21:16:47 today; last run "files=0". Replaced by `optic_sync` | ALREADY OFF (leftover files: §3.8) | Can still be started by hand |

### 2.3 System timers

`systemctl list-timers --all` lists 8 timers; all are enabled except the
static `systemd-tmpfiles-clean.timer`.

| Timer | What it does | Last / next run | Used here? Evidence | Verdict | Why |
|-------|--------------|-----------------|---------------------|---------|-----|
| `apt-daily.timer` | `apt.systemd.daily update` | 09-21 20:13 / 09-22 11:26 | No. `APT::Periodic` is not set, so the job exits at once (it ran in 2 s) | SAFE TO DISABLE | §3.1 |
| `apt-daily-upgrade.timer` | `apt.systemd.daily install` | 09-21 06:04 / 09-22 06:56 | No. Same, and `unattended-upgrades` is not installed | SAFE TO DISABLE | §3.1 |
| `man-db.timer` | Rebuilds the man-page index | 09-21 00:49 / 09-22 04:13 | No. Headless appliance; writes `/var/cache/man` on the SD card | SAFE TO DISABLE | §3.2 |
| `e2scrub_all.timer` | Weekly online ext4 check, **LVM only** | 09-20 03:10 / 09-27 03:10 | No. No LVM (see above) | SAFE TO DISABLE | §3.3 |
| `fstrim.timer` | Weekly TRIM | 09-21 01:06 / 09-28 01:14 | Yes. The SD card supports discard | KEEP | Helps the card's wear levelling over a year |
| `logrotate.timer` | Rotates `/var/log/*` (wtmp, btmp, apt, dpkg) | daily | Yes | KEEP | Bounds `/var/log` over 365 days |
| `dpkg-db-backup.timer` | Daily copy of the dpkg status database to `/var/backups` | daily | Yes | KEEP | Recovery aid; writes only when the database changed |
| `systemd-tmpfiles-clean.timer` (static) | Cleans `/tmp` and `/var/tmp` | daily | Yes | KEEP | Core |
| `apt-listchanges.timer` | – | – | Disabled | ALREADY OFF | – |
| `optic-capture-transfer.timer` | Retired transfer schedule | Stopped 21:16:47 today | No | ALREADY OFF | §3.8 |

### 2.4 System sockets (static, socket-activated)

`systemd-creds.socket`, `systemd-hostnamed.socket`, `systemd-initctl.socket`,
`systemd-rfkill.socket`, `systemd-sysext.socket`,
`systemd-journald.socket`, `systemd-journald-dev-log.socket`,
`systemd-udevd-control.socket`, `systemd-udevd-kernel.socket`, `dbus.socket`,
`avahi-daemon.socket`: **KEEP**. They are listening sockets with no process
behind them until used, so disabling them saves nothing and some are core
(journald, udev, dbus). `cloud-init-hotplugd.socket` goes with cloud-init
(§3.5).

### 2.5 User units (`liam`, `systemctl --user`)

| Unit | What it does | RAM | Used here? Evidence | Verdict | Why |
|------|--------------|-----|---------------------|---------|-----|
| `beszel-agent.service` | Beszel monitoring agent → hub `192.168.0.231:8090` | 14.2 MiB | Yes. Phase 6 "Beszel exports the capture RAM stage" PASS | KEEP | Monitoring; needs linger (`Linger=yes`) |
| `dbus.service` (+ `.socket`) | Session bus | 0.3 MiB | Yes. The user manager needs it | KEEP | Core |
| `mpris-proxy.service` | **Bluetooth** media-player proxy (package `bluez`), enabled for all users via `/etc/systemd/user/default.target.wants` | 0.2 MiB (RSS 2 MiB) | No. The Bluetooth controller is disabled in firmware and `bluetooth.service` is off | SAFE TO DISABLE | §3.4 |
| `gpg-agent.socket`, `gpg-agent-ssh.socket`, `gpg-agent-extra.socket`, `gpg-agent-browser.socket`, `dirmngr.socket`, `keyboxd.socket` | GnuPG, on demand | – (0 connections accepted) | No | KEEP (no gain) | Sockets only; zero RAM until something runs `gpg` |
| `ssh-agent.socket` | OpenSSH agent, on demand | – | No | KEEP (no gain) | Same |
| `rpi-connect.service` | Raspberry Pi Connect remote access (`rpi-connect-lite` 2.12.2 installed) | – | Disabled | ALREADY OFF | Package still installed (optional purge, not a service change) |
| `systemd-tmpfiles-setup.service`, `systemd-tmpfiles-clean.timer` (user) | – | – | Disabled (defaults) | ALREADY OFF | – |
| user timers | – | – | `systemctl --user list-timers --all`: 0 timers | – | – |

### 2.6 Already Off (observed)

| Unit | State | Who did it |
|------|-------|------------|
| `bluetooth.service` | disabled, inactive (`bluez` still installed) | Phase 4 script |
| `hciuart`, `ModemManager`, `cups`, `triggerhappy`, `rpi-display-backlight` | not installed (`not-found`) | image / Phase 4 |
| global user units pulseaudio / pipewire | absent | verify.sh Phase 4 PASS |
| `rsyslog` | not present in any unit listing | Phase 1 |
| `systemd-networkd` (+ wait-online, socket), `ssh.socket`, `nftables`, `rsync`, `rpi-resize`, `rpi-usb-gadget-ics`, `ppp@`, `wpa_supplicant@*` templates, `systemd-sysext`/`confext`, `systemd-pcrlock-*` | disabled | image defaults |
| `alsa-utils`, `hwclock`, `sudo.service`, `x11-common`, `userconfig`, `cryptdisks*` | masked | image defaults (the `sudo` command is unaffected; `sudo.service` is a Debian compatibility stub) |
| `optic-capture-transfer.timer` | disabled, stopped 2026-09-21 21:16:47 | user, today |

## 3. Candidates: Command, Saving, Risk, Undo

**Preferred: apply all of §3 with the provisioning scripts** (added
2026-09-21, see §5). They run the same commands, with the guards described
below, and a rebuilt Pi gets the same result:

```bash
scp scripts/setup-phase-04-unused-hardware.sh liam@optic.local:/home/liam/.local/bin/optic-setup-phase-04-unused-hardware
scp scripts/setup-phase-06-pi-ram-transfer.sh liam@optic.local:/home/liam/.local/bin/setup-phase-06-pi-ram-transfer.sh
ssh -t liam@optic.local '~/.local/bin/optic-setup-phase-04-unused-hardware --dry-run && ~/.local/bin/optic-setup-phase-04-unused-hardware'
ssh -t liam@optic.local '~/.local/bin/setup-phase-06-pi-ram-transfer.sh --dry-run && ~/.local/bin/setup-phase-06-pi-ram-transfer.sh'
ssh liam@optic.local 'bash -s -- --no-color' < verify.sh
```

The per-item commands below are the manual equivalent. Run them yourself
on the Pi (`ssh -t liam@optic.local`). After any change, run
`ssh liam@optic.local 'bash -s -- --no-color' < verify.sh`. Honest
headline: **all SAFE items together save under 1 MiB of steady RAM.** The
real gains are fewer wakeups and SD-card writes, fewer moving parts over 365
days, and (cloud-init) a 65 MiB memory spike and about 30 s less of the boot
spent provisioning.

### 3.1 `apt-daily.timer`, `apt-daily-upgrade.timer` (SAFE)

```bash
sudo systemctl disable --now apt-daily.timer apt-daily-upgrade.timer
```

- Saving: two daily wakeups (about 2 s each), no RAM. They currently do
  nothing because `APT::Periodic` is unset.
- Risk: none today. If someone later sets `APT::Periodic::*`, it silently
  has no effect. Manual `sudo apt update/upgrade` is unaffected.
- Undo: `sudo systemctl enable --now apt-daily.timer apt-daily-upgrade.timer`

### 3.2 `man-db.timer` (SAFE)

```bash
sudo systemctl disable --now man-db.timer
```

- Saving: one daily run and its `/var/cache/man` writes. The cron fallback
  (`/etc/cron.daily/man-db`, `/etc/cron.weekly/man-db`) exits when systemd is
  running, so nothing replaces it.
- Risk: `man -k`/`apropos` index goes stale. `man` itself still works, and
  dpkg triggers still update the index on package installs.
- Undo: `sudo systemctl enable --now man-db.timer`

### 3.3 `e2scrub_all.timer`, `e2scrub_reap.service` (SAFE)

```bash
sudo systemctl disable --now e2scrub_all.timer
sudo systemctl disable e2scrub_reap.service
```

- Saving: 0.43 s at boot and a weekly wakeup. Both act only on ext4 volumes
  on LVM, and this root is a plain partition (`lvs` not even installed).
  `/etc/cron.d/e2scrub_all` also exits when systemd is running.
- Risk: none unless the root moves to LVM. It is not a replacement for
  boot-time fsck, which stays (`fsck.repair=yes`, `systemd-fsck-root`).
- Undo: `sudo systemctl enable --now e2scrub_all.timer && sudo systemctl enable e2scrub_reap.service`

### 3.4 `mpris-proxy` (user) and `udisks2` (SAFE)

```bash
sudo systemctl --global disable mpris-proxy.service
systemctl --user stop mpris-proxy.service
sudo systemctl disable udisks2.service
```

- Saving: mpris-proxy 0.2 MiB cgroup (2 MiB RSS), one process fewer.
  udisks2 is not running now, so no saving; disabling only makes sure
  nothing pulls it in at boot. (What pulls it in (its `WantedBy=`) was not
  inspected: **unverified**. It would still be D-Bus activatable unless
  masked.)
- Risk: none. Bluetooth is off in firmware and there is no desktop.
- Undo: `sudo systemctl --global enable mpris-proxy.service && systemctl --user start mpris-proxy.service`;
  `sudo systemctl enable udisks2.service`

### 3.5 cloud-init (DISABLE WITH CARE)

```bash
sudo touch /etc/cloud/cloud-init.disabled   # cloud-init's documented off switch; its generator then skips all stages
```

- Saving: at boot, a 65.3 MiB memory peak and about 0.7 s of CPU (the
  stages run until 34 s into the boot). It also removes the `degraded`
  status and the `cc_netplan_nm_patch` warning. Steady RAM: none.
- Why it's not needed: the Imager's first-boot provisioning ran on
  2026-09-14/15 (semaphores in `/var/lib/cloud/instance/sem`). The Wi-Fi
  profile lives in `/etc/netplan/90-NM-af6538bb-….yaml`, written by
  NetworkManager (UUID matches the active connection), not by cloud-init.
- Risk / unverified: I did not verify whether cloud-init re-applies
  `/boot/firmware/network-config` on every boot. If it does, disabling it
  removes "edit `network-config` on the SD card from the Mac" as a way to
  change the Wi-Fi password. Afterwards you would use `nmcli` over SSH or the
  UART. Do it only with a way back in (UART on `ttyAMA10`, or HDMI and a
  keyboard), and confirm Wi-Fi returns on the **next reboot** (the reboot is
  your call).
- Undo: `sudo rm /etc/cloud/cloud-init.disabled`, then reboot.

### 3.6 Autologin on `getty@tty1` (DISABLE WITH CARE)

Keep the getty. Remove only the autologin drop-in, which gives anyone at an
HDMI screen and keyboard a `liam` shell. setup.md §8G documents that `liam`
has blanket passwordless sudo (`/etc/sudoers.d/liam-nopasswd`); I did not
re-check that here.

```bash
sudo install -d -m 0700 /var/backups/optic-hardening
sudo mv /etc/systemd/system/getty@tty1.service.d/autologin.conf /var/backups/optic-hardening/getty-tty1-autologin.conf
sudo systemctl daemon-reload
sudo systemctl restart getty@tty1.service   # ends the idle tty1 session
```

- Saving: about 0.4 MiB (the idle `login` + `bash` session). Mainly a
  physical-access security gain.
- Risk: the local console then needs `liam`'s password. `passwd -S liam`
  reports state `P` (a usable password is set; observed 2026-09-21). Make
  sure you know it before removing autologin; otherwise SSH is the only way
  in. The Phase 4 script skips this item unless the state is `P`.
- Undo: `sudo mv /var/backups/optic-hardening/getty-tty1-autologin.conf /etc/systemd/system/getty@tty1.service.d/autologin.conf && sudo systemctl daemon-reload && sudo systemctl restart getty@tty1.service`

### 3.7 `cron.service` (DISABLE WITH CARE)

First check the part I could not read:

```bash
sudo ls -la /var/spool/cron/crontabs
sudo crontab -l -u root
```

If both are empty:

```bash
sudo systemctl disable --now cron.service
```

- Saving: 0.5 MiB and one daemon. Every `/etc/cron.*` job on this Pi defers
  to a systemd timer.
- Risk: a future package that ships only a cron job would silently not run.
  Root's crontab is **unverified**, and cron logged "Running @reboot jobs" at
  boot, which is its normal startup message but could also mean a job.
- Undo: `sudo systemctl enable --now cron.service`

### 3.8 Retired `optic-capture-transfer` leftovers (DISABLE WITH CARE: file removal)

These are already off, but still installed. The service can be started by
hand, and if it runs it ships and then deletes every non-hidden file in
`/mnt/capture`, racing `optic_sync`
(`worklogs/2026-09-21-post-merge-drift-fixes.md`). Leftovers observed:

- `/etc/systemd/system/optic-capture-transfer.service` and `.timer`
- `/etc/optic/capture-transfer.conf` (the only file in `/etc/optic`, which holds the old iMac target)
- `/usr/local/libexec/optic-capture-transfer`
- `/home/liam/.local/bin/optic-capture-transfer.sh` and `setup-phase-06-pi-ram-transfer.sh`

**Keep** `~/.ssh/optic_capture_ed25519` and `~/.ssh/optic_capture_known_hosts`:
`optic_sync` uses them, and `verify.sh` Phase 6 checks them.

```bash
sudo install -d -m 0700 /var/backups/optic-hardening/retired-transfer
sudo mv /etc/systemd/system/optic-capture-transfer.service \
        /etc/systemd/system/optic-capture-transfer.timer \
        /etc/optic/capture-transfer.conf \
        /usr/local/libexec/optic-capture-transfer \
        /var/backups/optic-hardening/retired-transfer/
sudo rmdir /etc/optic
sudo systemctl daemon-reload
# liam's own copies (no sudo):
mkdir -p ~/.local/share/optic-retired
mv ~/.local/bin/optic-capture-transfer.sh ~/.local/bin/setup-phase-06-pi-ram-transfer.sh ~/.local/share/optic-retired/
```

- Saving: no RAM. It removes a destructive unit that could be started by
  mistake, and a stale config.
- Risk: low. `verify.sh` Phase 6 passes with the timer absent (it checks
  only "not active and not enabled").
  `scripts/setup-phase-06-pi-ram-transfer.sh` would reinstall it on a
  re-provision; that needs a separate script change.
- Undo: move the files back and run `sudo systemctl daemon-reload`. The
  timer stays disabled.

### 3.9 Not recommended, and why

- **`wpa_supplicant`, `NetworkManager`, `NetworkManager-wait-online`**:
  Wi-Fi is the only live link (`eth0` has no carrier).
- **`avahi-daemon`**: `optic.local` and `imac.local` (`optic_sync`) both
  depend on it; `systemd-resolved` is not running.
- **`fstrim.timer`**: useful on an SD card over a year.
- **`serial-getty@ttyAMA10`**: the rescue console when the network fails;
  costs 0.2 MiB.
- **`rpi-eeprom-update`**: it only flashes a bootloader below the minimum
  version.
- **User GnuPG and ssh-agent sockets, `apparmor`, `sshswitch`,
  `regenerate_ssh_host_keys`, `systemd-pstore`**: no-ops with zero RAM.
  Changing them adds diff to maintain for no gain.

## 4. Failing and Noisy Units

`systemctl --failed` (system and user): **none**. `journalctl -b -p warning`:
11 entries this boot (the previous boot `677b609e` had 13):

| Source | Count | Message | Assessment |
|--------|------:|---------|------------|
| netplan generator (`generate`) | 4 | `Permissions for /lib/netplan/00-network-manager-all.yaml are too open` | Package-shipped file, 0644, contains no secrets (it only hands control to NM). Harmless. A `chmod` would be overwritten by the next `netplan.io` upgrade |
| `systemd-udevd` | 4 | `90-alsa-restore.rules:18/22 GOTO="alsa_restore_std" has no matching label` | Packaging issue in the alsa rules file (the `alsa-utils` unit is masked). Harmless; disabling services does not silence it |
| `wpa_supplicant` | 2 | `nl80211: … Registration to specific type not supported`; `bgscan simple: Failed to enable signal strength monitoring` | brcmfmac driver limitations. Harmless; Wi-Fi connected at signal 82 |
| kernel | 1 | `raspberrypi-firmware …: Request 0x00030097 returned status 0x80000001` | The firmware does not support one mailbox property; the exact meaning is **unverified**. No functional effect observed |
| cloud-init (own log, below journal warning priority) | 1 | `Could not find module named cc_netplan_nm_patch` → status `degraded done` | Removed by §3.5 |

## 5. Making Choices Stick

Done 2026-09-21 at the user's request ("in long term we might need to
rebuild our optic pi box"). Applied on the Pi by the user on 2026-09-21
at 22:54, with nothing skipped. Afterwards `verify.sh` Phases 4 and 6 are
all PASS, and a rerun of either script changes nothing. A reboot at
22:57 came back with cloud-init `disabled`, Wi-Fi up, and optic-daemon
starting after `time-sync.target`:

- `scripts/setup-phase-04-unused-hardware.sh` disables every §3.1–§3.7
  item. Guards: apt only while `APT::Periodic` is unset; e2scrub only if `/`
  is not on LVM; cron only if no crontab exists and every cron job skips
  under systemd; cloud-init only once `cloud-init status` is `done`;
  autologin only if `liam`'s password state is `P`. Skipped items are
  reported at the end of the run.
- `scripts/setup-phase-06-pi-ram-transfer.sh` no longer installs the retired
  shell transfer, and retires any leftover files (§3.8) into
  `/var/backups/optic-hardening/retired-capture-transfer.<time>/`.
- `verify.sh` Phase 4 FAILs on the unconditional items and WARNs on cron,
  cloud-init and autologin. Phase 6 FAILs while retired transfer files
  remain.
- Pre-existing bug fixed along the way: the Phase 4 renderer moved its
  `config.txt` block to the end, so after Phase 5 appended its block every
  rerun wanted to rewrite `config.txt` and reboot. It now re-renders the
  block in place.
