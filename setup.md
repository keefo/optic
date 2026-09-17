# Project Optic: OS Hardening & Long-Term Optimization Guide

Optic is an ultra-reliable, long-term timelapse system engineered around the Raspberry Pi 5 (1GB) and Raspberry Pi High Quality Camera, fitted with an official 6mm f/1.2 CS-mount wide-angle lens (PT361060M3MP12). Running on a stripped-down, headless Raspberry Pi OS Lite environment, Optic is architected for 365 days of unattended operation—pairing minimal memory overhead with read-only root safeguards (OverlayFS) to prevent file corruption and memory leaks across year-long captures.

---

## Hardware & System Specifications

### Compute & System

* **Host Platform:** Raspberry Pi 5
* **CPU:** Broadcom BCM2712 (Quad-core Arm Cortex-A76 @ 2.4GHz)
* **Memory:** 1 GB LPDDR4X-4267 SDRAM
* **Operating System:** Raspberry Pi OS Lite (64-bit, Debian base, headless)
* **Filesystem Architecture:** Read-only root via OverlayFS with a bounded RAM capture stage and verified transfer to the iMac

### Imaging & Optics

* **Sensor:** Raspberry Pi High Quality Camera (Sony IMX477)
* **Native Resolution:** 12.3 Megapixels (4056 × 3040 px)
* **Sensor Format:** 1/2.3" (7.9 mm diagonal, 1.55 µm × 1.55 µm pixel size)
* **Lens Mount:** CS-mount (with integrated back-focus adjustment ring)
* **Lens Model:** Official Raspberry Pi 6mm CS-Mount (`PT361060M3MP12` / `SC0124`)
* **Focal Length & Aperture:** 6.0 mm, adjustable f/1.2 – f/16
* **Field of View (FoV):** ~63° (horizontal)

### Networking & Access

* **Hostname:** `optic.local`
* **Remote Access:** OpenSSH (key-based authentication recommended)
* **Service Discovery:** mDNS / Avahi zero-configuration networking

---

## Monitoring Baseline: Beszel

Beszel `0.19.0` runs with the Hub on the always-on iMac and the native ARM64 Agent on the Pi. Open the LAN-only dashboard at [http://tests-iMac-Pro.local:8090](http://tests-iMac-Pro.local:8090). The login is stored locally on the iMac in `~/.local/share/beszel/admin-credentials` with owner-only permissions.

The iMac Hub runs as the `dev.beszel.hub` LaunchAgent. Its data is in `~/.local/share/beszel/data`, and its logs are in `~/Library/Logs/Beszel`. The wrapper uses `caffeinate -s` to prevent system sleep while the iMac is on AC power. Because it is a per-user LaunchAgent, the `admin` account must be logged in after an iMac restart. Reserve `192.168.0.231` for the iMac in the router's DHCP settings; the statically linked Pi Agent does not resolve `.local` names reliably.

The Pi Agent runs as the persistent `liam` user service `beszel-agent.service`. Agent-initiated WebSocket mode avoids exposing the Agent's SSH port to the LAN. Beszel records CPU, load, memory, CPU and RP1 temperatures, Active Cooler RPM, root-disk usage and I/O, and network traffic. Check service health with:

```bash
launchctl print "gui/$(id -u)/dev.beszel.hub"
ssh liam@optic.local 'systemctl --user status beszel-agent.service'
```

Phase 6 adds the following Beszel Agent override so the bounded RAM capture stage is visible in monitoring:

```ini
Environment="EXTRA_FILESYSTEMS=/mnt/capture__Capture-RAM"
```

```bash
systemctl --user daemon-reload
systemctl --user restart beszel-agent.service
```

The Phase 6 Pi setup script installs this override only after configuring `/mnt/capture` as `tmpfs`.

---

## Failure Modes & Hardening Strategy

Running unattended on a 1GB board for a full year presents three critical failure vectors:

1. **SD Flash Degradation:** Continuous system journaling and metadata churn destroying flash write endurance.
2. **Out-of-Memory (OOM) Deadlocks:** Swap file thrashing and memory fragmentation triggering the Linux OOM killer during raw frame buffer generation.
3. **Unrecovered System Freezes:** Transient kernel panics, CSI pipeline locks, or brownouts requiring physical power cycles.

The optimizations below harden the operating system to prevent these failures.

### Read-Only Verification

Run the verification script before and after each hardening phase. It checks the configured and effective state without changing the Pi or displaying secret values:

```bash
cd /Users/admin/Documents/projects/optic
ssh liam@optic.local 'bash -s -- --color' < verify.sh
```

Each result is color-coded and labeled `PASS` (green), `WARN` (yellow), `FAIL` (red), or `INFO` (cyan). The script exits with status `1` when a required check fails. Phase 9 OverlayFS remains a warning while that deliberately deferred phase is incomplete. Use `--strict` to make warnings return status `2` when no failures exist, or `--no-color` for plain output.

---

## 1. Eliminate Flash Wear & Set Up RAM Journaling

Standard system logging continually writes to disk. Redirect all logs to an in-memory ring buffer.

Apply this phase with the reusable, idempotent setup script. It saves any differing previous managed drop-in under `/var/backups/optic-hardening`, safely skips settings that already match, and verifies the effective configuration after restarting journald:

```bash
scp /Users/admin/Documents/projects/optic/scripts/setup-phase-01-journaling.sh liam@optic.local:/home/liam/.local/bin/optic-setup-phase-01-journaling
ssh liam@optic.local 'chmod 700 /home/liam/.local/bin/optic-setup-phase-01-journaling'
ssh -t liam@optic.local '/home/liam/.local/bin/optic-setup-phase-01-journaling'
```

Then verify only Phase 1:

```bash
cd /Users/admin/Documents/projects/optic
ssh liam@optic.local 'bash -s -- --phase 1 --color' < verify.sh
```

### A. Constrain Systemd Journal to RAM

Open `/etc/systemd/journald.conf`:

```bash
sudo nano /etc/systemd/journald.conf
```

Uncomment and configure the following directives:

```ini
[Journal]
Storage=volatile
RuntimeMaxUse=32M
```

Apply changes immediately:

```bash
sudo systemctl restart systemd-journald
```

### B. Disable Disk-Heavy Logging Daemons

Remove redundant syslog services that write duplicate records to `/var/log`:

```bash
sudo systemctl stop rsyslog
sudo systemctl disable rsyslog
```

---

## 2. Memory Architecture: zram & Swappiness (1GB RAM)

Raspberry Pi OS Trixie uses `rpi-swap` and `systemd-zram-generator`. Its automatic mode can attach an on-disk writeback file to zram. Select pure **zram** explicitly so swap remains in compressed memory and cannot write pages to the microSD card.

Apply this phase with the reusable, idempotent setup script. It uses administrator drop-ins, backs up differing managed files under `/var/backups/optic-hardening`, validates generated systemd configuration, and applies only the VM sysctls live:

```bash
scp /Users/admin/Documents/projects/optic/scripts/setup-phase-02-memory.sh liam@optic.local:/home/liam/.local/bin/optic-setup-phase-02-memory
ssh liam@optic.local 'chmod 700 /home/liam/.local/bin/optic-setup-phase-02-memory'
ssh -t liam@optic.local '/home/liam/.local/bin/optic-setup-phase-02-memory --reboot'
```

The native swap generator requires a reboot to replace the active zram device. After `optic` returns, verify only Phase 2:

```bash
cd /Users/admin/Documents/projects/optic
ssh liam@optic.local 'bash -s -- --phase 2 --color' < verify.sh
```

### A. Native Pure-zram Policy

The setup script creates `/etc/rpi/swap.conf.d/90-optic.conf`:

```ini
[Main]
Mechanism=zram

[Zram]
RamMultiplier=0.5
FixedSizeMiB=493
```

`FixedSizeMiB` is calculated by the setup script from the installed physical memory; `493` is the expected value for the current board. This explicit value also avoids a parsing defect in the current `rpi-swap` 1.2.4 dynamic-size helper.

It also creates `/etc/systemd/zram-generator.conf.d/90-optic.conf`:

```ini
[zram0]
compression-algorithm=zstd
swap-priority=100
```

### B. Kernel Virtual Memory Parameters

The setup script creates `/etc/sysctl.d/99-optic-memory.conf` to keep high-order RAM pages available for camera buffers:

```ini
# Favor reclaiming pagecache over aggressive swapping
vm.swappiness=10

# Reserve 64MB of physical RAM clear for high-order CSI frame allocations
vm.min_free_kbytes=65536

# Reduce cache pressure to preserve metadata
vm.vfs_cache_pressure=50
```

---

## 3. Hardware Watchdog & Kernel Panic Recovery

Configure the Pi 5's BCM2712 hardware watchdog and kernel auto-reset features so the system reboots itself if a lockup occurs.

Apply this phase with the reusable, idempotent setup script. It checks for the hardware watchdog before making changes, uses isolated administrator drop-ins, and verifies runtime state after re-executing systemd:

```bash
scp /Users/admin/Documents/projects/optic/scripts/setup-phase-03-watchdog.sh liam@optic.local:/home/liam/.local/bin/optic-setup-phase-03-watchdog
ssh liam@optic.local 'chmod 700 /home/liam/.local/bin/optic-setup-phase-03-watchdog'
ssh -t liam@optic.local '/home/liam/.local/bin/optic-setup-phase-03-watchdog'
```

Then verify only Phase 3:

```bash
cd /Users/admin/Documents/projects/optic
ssh liam@optic.local 'bash -s -- --phase 3 --color' < verify.sh
```

### A. Set Kernel Auto-Reboot on Panic

The setup script creates `/etc/sysctl.d/99-optic-recovery.conf`:

```ini
# Automatically reboot 10 seconds after a kernel panic
kernel.panic=10

# Trigger a panic if a hung task or soft lockup is detected
kernel.panic_on_oops=1
```

### B. Bind Systemd to Hardware Watchdog

The setup script creates `/etc/systemd/system.conf.d/90-optic-watchdog.conf`:

```ini
[Manager]
RuntimeWatchdogSec=15s
RebootWatchdogSec=2min
```

---

## 4. Strip Bloat & Disable Unused Hardware

Reduce idle CPU, RAM overhead, and power consumption by stopping unused peripherals and daemons.

Apply this phase with the reusable, idempotent setup script. It changes only failed service and firmware checks, backs up `config.txt` under `/var/backups/optic-hardening`, and confirms SSH and Avahi remain active before rebooting:

```bash
scp /Users/admin/Documents/projects/optic/scripts/setup-phase-04-unused-hardware.sh liam@optic.local:/home/liam/.local/bin/optic-setup-phase-04-unused-hardware
ssh liam@optic.local 'chmod 700 /home/liam/.local/bin/optic-setup-phase-04-unused-hardware'
ssh -t liam@optic.local '/home/liam/.local/bin/optic-setup-phase-04-unused-hardware --reboot'
```

After `optic` returns, verify only Phase 4:

```bash
cd /Users/admin/Documents/projects/optic
ssh liam@optic.local 'bash -s -- --phase 4 --color' < verify.sh
```

### A. Disable Unused Background Daemons

The setup script disables `bluetooth.service`. It leaves absent services and audio daemons unchanged rather than installing or masking nonexistent units.

*(Note: Retain `avahi-daemon` enabled if you access the node via `optic.local`).*

### B. Firmware Hardware Disables

The setup script places one managed copy of each target directive under an `[all]` block in `/boot/firmware/config.txt`:

```ini
# Disable Bluetooth controller
dtoverlay=disable-bt

# Disable onboard audio output
dtparam=audio=off

# Permit HDMI output blanking when the connected display is idle
hdmi_blanking=2
```

---

## 5. Active Cooler Fan Policy (Raspberry Pi 5)

The official Raspberry Pi 5 Active Cooler connects to the dedicated four-pin fan header and is controlled automatically by the kernel's `pwm-fan` thermal driver. Do **not** add a `gpio-fan` or `pwm-fan` overlay and do not run a separate fan-control daemon for the official cooler.

Apply this phase with the reusable, idempotent setup script. It refuses to modify firmware unless the official cooler is detected and no conflicting fan overlay exists, then backs up `config.txt` under `/var/backups/optic-hardening`:

```bash
scp /Users/admin/Documents/projects/optic/scripts/setup-phase-05-active-cooler.sh liam@optic.local:/home/liam/.local/bin/optic-setup-phase-05-active-cooler
ssh liam@optic.local 'chmod 700 /home/liam/.local/bin/optic-setup-phase-05-active-cooler'
ssh -t liam@optic.local '/home/liam/.local/bin/optic-setup-phase-05-active-cooler --reboot'
```

After `optic` returns, verify only Phase 5:

```bash
cd /Users/admin/Documents/projects/optic
ssh liam@optic.local 'bash -s -- --phase 5 --color' < verify.sh
```

For an unattended timelapse system, use an explicit cooling profile rather than relying on firmware defaults. This profile starts cooling early and reaches near-maximum airflow before the CPU approaches its thermal limit:

| CPU temperature | Cooling state | PWM value | Approximate duty cycle |
| --- | ---: | ---: | ---: |
| Below 45°C | Off | 0 | 0% |
| 45–54.9°C | Low | 75 | 29% |
| 55–64.9°C | Medium | 125 | 49% |
| 65–69.9°C | High | 175 | 69% |
| 70°C and above | Maximum | 250 | 98% |

Each stage uses 5°C hysteresis. For example, after entering the low stage at 45°C, the fan switches off only after the CPU falls below 40°C. This avoids rapid fan cycling around a threshold.

### A. Configure the Thermal Stages

The setup script adds exactly one managed copy of each setting under an `[all]` block in `/boot/firmware/config.txt`:

```ini
# Project Optic: Raspberry Pi 5 Active Cooler profile
dtparam=fan_temp0=45000
dtparam=fan_temp0_hyst=5000
dtparam=fan_temp0_speed=75

dtparam=fan_temp1=55000
dtparam=fan_temp1_hyst=5000
dtparam=fan_temp1_speed=125

dtparam=fan_temp2=65000
dtparam=fan_temp2_hyst=5000
dtparam=fan_temp2_speed=175

dtparam=fan_temp3=70000
dtparam=fan_temp3_hyst=5000
dtparam=fan_temp3_speed=250
```

Temperature and hysteresis values are in millidegrees Celsius. PWM values range from `0` to `255`; they specify duty cycle, not an exact RPM.

### B. Verify Detection and Thresholds

After reconnecting, confirm that the cooler is detected:

```bash
cat /sys/class/thermal/cooling_device0/type
cat /sys/class/thermal/cooling_device0/max_state
```

Expected output is `pwm-fan` and `4`. Verify the applied thermal thresholds:

```bash
for file in /sys/class/thermal/thermal_zone0/trip_point_*; do
    printf '%s=' "$(basename "$file")"
    cat "$file"
done
```

The four active trip temperatures should be `45000`, `55000`, `65000`, and `70000`, each with `5000` hysteresis. The separate `110000` critical trip is normal and must not be changed.

### C. Monitor Fan Operation

Display CPU temperature, cooling state, PWM output, and measured fan speed once per second. Discover the `pwmfan` hardware-monitor path dynamically because its `hwmon` number can change between boots:

```bash
fan_hwmon=$(dirname "$(grep -l '^pwmfan$' /sys/class/hwmon/hwmon*/name | head -n 1)")

while true; do
    temperature=$(awk '{printf "%.1f", $1 / 1000}' /sys/class/thermal/thermal_zone0/temp)
    state=$(cat /sys/class/thermal/cooling_device0/cur_state)
    pwm=$(cat "$fan_hwmon/pwm1")
    rpm=$(cat "$fan_hwmon/fan1_input")
    printf '\rtemperature=%s°C state=%s pwm=%s rpm=%s   ' "$temperature" "$state" "$pwm" "$rpm"
    sleep 1
done
```

An RPM of `0` is normal while the cooling state and PWM are both `0`. If PWM is nonzero but RPM remains `0` for more than a few seconds, shut down the Pi and reseat the Active Cooler's fan-header connector. Do not write directly to `pwm1`; automatic thermal control owns that interface.

---

## 6. RAM Capture Stage & Verified iMac Transfer

Never store captured frames on the boot/OS root partition. `/mnt/capture` is a bounded 256 MiB `tmpfs`; a systemd timer transfers completed files to `/Users/admin/Pictures/Optic` on the iMac at `192.168.0.231`.

> **Durability tradeoff:** queued captures exist only in RAM until transfer succeeds. An iMac/network outage eventually fills the bounded queue and causes new captures to fail rather than write to microSD. A Pi power loss loses any queued files. The iMac must remain available for unattended operation.

The iMac receiver uses a dedicated SSH daemon on TCP `2222`. It does **not** enable macOS Remote Login or port `22`. It allows only the generated Pi key, disables passwords, forwarding, PTYs, and arbitrary commands, and forces the Project Optic receiver. The receiver verifies byte count and SHA-256, commits atomically, syncs storage, and acknowledges success before the Pi deletes its source.

### A. Bootstrap the Pi RAM Stage and Client Key

```bash
scp /Users/admin/Documents/projects/optic/scripts/{setup-phase-06-pi-ram-transfer.sh,optic-capture-transfer.sh} liam@optic.local:/home/liam/.local/bin/
ssh liam@optic.local 'chmod 700 /home/liam/.local/bin/setup-phase-06-pi-ram-transfer.sh; chmod 755 /home/liam/.local/bin/optic-capture-transfer.sh'
ssh -t liam@optic.local '/home/liam/.local/bin/setup-phase-06-pi-ram-transfer.sh'
scp liam@optic.local:/home/liam/.ssh/optic_capture_ed25519.pub /tmp/optic_capture_ed25519.pub
```

The first run creates the RAM stage and key but leaves the transfer timer disabled until the iMac host key is pinned.

### B. Configure the Restricted iMac Receiver

```bash
cd /Users/admin/Documents/projects/optic
sudo ./scripts/setup-phase-06-imac-receiver.sh --client-key /tmp/optic_capture_ed25519.pub
```

Captures are stored under `/Users/admin/Pictures/Optic`. The setup exports its dedicated public host key to `/Users/admin/.config/optic/capture_receiver_host_ed25519.pub`.

### C. Pin the Receiver and Enable Transfers on the Pi

```bash
scp /Users/admin/.config/optic/capture_receiver_host_ed25519.pub liam@optic.local:/tmp/optic_capture_receiver_host.pub
ssh -t liam@optic.local '/home/liam/.local/bin/setup-phase-06-pi-ram-transfer.sh --host-key /tmp/optic_capture_receiver_host.pub'
```

Capture applications must use unique destination filenames, write to a hidden temporary name in `/mnt/capture`, close it, and then rename it to the final non-hidden filename. The transfer service ignores hidden files and waits at least 10 seconds after the last modification before sending.

### D. Verify the Pipeline

```bash
cd /Users/admin/Documents/projects/optic
ssh liam@optic.local 'bash -s -- --phase 6 --color' < verify.sh
```

Phase 6 verification checks the tmpfs size and hardening, timer state, pinned key, authenticated receiver probe, latest transfer result, Beszel export, and queued file/byte counts.

---

## 7. Real-Time Clock (RTC) & Timestamp Synchronization — Skipped

This deployment has no RTC backup battery, so Phase 7 battery configuration is skipped. Network time synchronization remains required while the Pi has network access. Do not enable RTC trickle charging unless a supported rechargeable battery is installed.

* If running with an external RTC backup battery on the Pi 5's dedicated JST connector, enable trickle charging in `/boot/firmware/config.txt`:
```ini
# Trickle charge supported ML2020 / rechargeable RTC lithium cell
dtparam=rtc_bbat_vcharge=3000000
```


* Verify the time-synchronization daemon is healthy:
```bash
timedatectl status
```


---

## 8. Raspberry Pi HQ Camera Setup & Commissioning

Phase 8 commissions the Raspberry Pi High Quality Camera (`IMX477`) and official 6 mm CS-mount lens before the root filesystem becomes immutable. The current Pi already has `rpicam-apps`, `camera_auto_detect=1`, and the required `video` and `render` group access; do not install legacy `raspistill` or change firmware overlays unless detection fails.

### A. Connect the Camera with Power Removed

1. Shut the Pi down and disconnect its power supply:

```bash
ssh liam@optic.local 'sudo poweroff'
```

2. Use a Pi 5-compatible 22-pin-to-15-pin camera cable. The older 15-pin-to-15-pin cable supplied with some cameras does not fit the Pi 5 camera/display connector.
3. Open each connector latch, insert the cable fully and squarely with its exposed contacts facing the connector contacts, then close the latch. Do not connect or disconnect the camera while the Pi is powered.
4. For the official 6 mm CS-mount lens, remove the C-to-CS adapter from the HQ Camera if it is fitted. Remove the sensor cap, avoid touching the sensor or glass, and screw the lens directly into the CS mount without overtightening it.
5. Reconnect power and wait for `optic.local` to return.

### B. Verify Firmware and Camera Access

Confirm auto-detection is enabled exactly once and that `liam` can access camera devices:

```bash
ssh liam@optic.local '
grep -nE "^[[:space:]]*camera_auto_detect=1([[:space:]]*(#.*)?)?$" /boot/firmware/config.txt
id -nG
rpicam-still --version
rpicam-still --list-cameras
'
```

Expected camera identification:

```text
0 : imx477 [4056x3040 12-bit RGGB]
```

`liam` must belong to both `video` and `render`. The manual-focus HQ Camera does not provide autofocus; focus and aperture are adjusted on the lens.

### C. Capture a Native-Resolution Test Image

Write to a hidden temporary file first, then expose the completed image to the Phase 6 transfer service with an atomic rename:

```bash
ssh liam@optic.local '
set -e
name="hq-camera-test-$(date -u +%Y%m%dT%H%M%SZ).jpg"
temporary="/mnt/capture/.$name.part"
final="/mnt/capture/$name"
trap '\''rm -f -- "$temporary"'\'' EXIT
rpicam-still --nopreview --timeout 5sec --encoding jpg \
    --width 4056 --height 3040 --quality 95 --output "$temporary"
chmod 0640 "$temporary"
mv -- "$temporary" "$final"
trap - EXIT
sha256sum "$final"
'
```

The transfer timer waits at least 10 seconds, verifies the size and SHA-256 on the iMac, and only then deletes the Pi copy. After approximately 30 seconds, inspect the newest test image:

```bash
image=$(ls -t /Users/admin/Pictures/Optic/hq-camera-test-*.jpg | head -n 1)
shasum -a 256 "$image"
sips -g pixelWidth -g pixelHeight "$image"
open "$image"
```

The hash must match the Pi output, and `sips` must report `4056` by `3040` pixels.

### D. Set Aperture and Focus

1. Open the aperture to `f/1.2` for critical focusing, aim at a detailed subject near the intended timelapse distance, and inspect captures at 100% on the iMac.
2. Adjust the lens focus ring. Use the HQ Camera back-focus ring only if the lens cannot reach the required focus across its normal adjustment range; retighten all lock screws afterward.
3. Set the final operating aperture for the required depth of field and exposure, then repeat the native-resolution test. Focus changes when aperture or mounting is disturbed, so perform this step after the camera is fixed in its final enclosure.

### E. Run the Read-Only Phase Check

```bash
cd /Users/admin/Documents/projects/optic
ssh liam@optic.local 'bash -s -- --phase 8 --color' < verify.sh
```

Proceed only when the Phase 8 checks pass and the transferred image has been visually confirmed sharp, correctly framed, and free of cable or sensor errors.

### F. Deploy the Phase 1 Camera Dashboard

The current `optic-daemon` Phase 1 is the camera control plane; scheduler and in-process sync workers are not yet implemented. Build it natively on the Pi so the release artifact matches the installed AArch64 system:

Every change to daemon source or embedded web assets requires a package version bump and a new Pi deployment. Work is complete only after the restarted service reports that version from `/api/status` and the deployed web assets are verified over HTTP.

The complete pinned environment, user-local native sysroot layout, known
LLVM/Clippy conflicts, and recovery procedure are documented in
[`optic-daemon-build-environment.md`](optic-daemon-build-environment.md). In
particular, never export a variable named `SYSROOT`, and do not expose staged
LLVM 19 to Clippy through `LD_LIBRARY_PATH`.

Run the complete bootstrap, build, upload, guarded installation, and deployment
verification from the development Mac with no arguments:

```bash
cd /Users/admin/Documents/projects/optic
./scripts/build-deploy-optic-daemon.sh
```

The expanded commands below are retained for manual diagnosis only.

```bash
ssh liam@optic.local '
cd /home/liam/.local/src/optic-daemon-0.1.3
export PATH="$HOME/.cargo/bin:$PATH"
export OPTIC_SYSROOT="$HOME/.local/optic-sysroot"
export OPTIC_NATIVE_LIB="$OPTIC_SYSROOT/usr/lib/aarch64-linux-gnu"
export PKG_CONFIG_PATH="$OPTIC_NATIVE_LIB/pkgconfig"
export PKG_CONFIG_SYSROOT_DIR="$OPTIC_SYSROOT"
export LIBCLANG_PATH="$OPTIC_NATIVE_LIB"
export BINDGEN_EXTRA_CLANG_ARGS="-I/usr/lib/gcc/aarch64-linux-gnu/14/include"
unset SYSROOT LD_LIBRARY_PATH
rustup component add rustfmt clippy
cargo fmt --all -- --check
LD_LIBRARY_PATH="$OPTIC_NATIVE_LIB" cargo test --locked --all-targets
env -u SYSROOT -u LD_LIBRARY_PATH cargo clippy --locked --all-targets -- -D warnings
LD_LIBRARY_PATH="$OPTIC_NATIVE_LIB" cargo build --locked --release
./scripts/setup-optic-daemon-phase-01.sh
'
```

The setup script runs as `liam`, installs `/home/liam/.local/bin/optic-daemon`, and enables `/home/liam/.config/systemd/user/optic-daemon.service`. It requires the Phase 6 tmpfs, `video` and `render` group membership, and administrator-provisioned `Linger=yes`.

Validate the service and open the dashboard:

```bash
ssh liam@optic.local '
systemctl --user is-enabled optic-daemon.service
systemctl --user is-active optic-daemon.service
curl --fail http://127.0.0.1:8000/healthz
curl --fail http://127.0.0.1:8000/api/status
'
open http://optic.local:8000/
```

TCP `8000` is the canonical endpoint. The validated Pi reserves ports below `1024` for privileged processes, nothing listens on port `80`, and the user service intentionally remains unprivileged. Do not document `http://optic.local/` unless an administrator later installs and validates a port-80 proxy or redirect.

The deployed unit is enabled and `Linger=yes`, and a controlled user-service restart was validated. A full reboot validation remains pending because the current SSH session has neither passwordless sudo nor noninteractive PolicyKit reboot authorization.

---

## 9. Immutable Protection: Read-Only Root (OverlayFS)

> ⚠️ **Run this step ONLY after confirming your capture scripts, Wi-Fi networks, SSH keys, and systemd units operate reliably.**

OverlayFS mounts the underlying SD card root filesystem as read-only. All runtime writes are redirected to a temporary RAM scratch space. Power loss or unexpected resets will never corrupt the operating system.

1. Open the Raspberry Pi configuration menu:
```bash
sudo raspi-config
```


2. Navigate to **Performance Options** → **Overlay File System**.
3. Choose **Yes** to enable the Overlay File System.
4. Choose **Yes** to make the boot partition read-only.
5. Reboot the board.

### Making Future System Modifications

When applying script adjustments, system upgrades, or package updates:

1. Run `sudo raspi-config`.
2. Under **Performance Options** → **Overlay File System**, choose **No** and reboot.
3. Apply changes to the underlying filesystem.
4. Re-enable the Overlay File System and reboot.

*(Note: Data written to `/mnt/capture` is mounted independently and is never blocked by OverlayFS).*
