# Project Optic

Optic is an ultra-reliable, long-term timelapse system engineered around the Raspberry Pi 5 (1GB) and Raspberry Pi High Quality Camera, fitted with an official 6mm f/1.2 CS-mount wide-angle lens (PT361060M3MP12). Running on a stripped-down, headless Raspberry Pi OS Lite environment, Optic is architected for 365 days of unattended operation—pairing minimal memory overhead with read-only root safeguards (OverlayFS) to prevent file corruption and memory leaks across year-long captures.

## Specifications

### Compute & System
* **Host Platform:** Raspberry Pi 5
* **CPU:** Broadcom BCM2712 (Quad-core Arm Cortex-A76 @ 2.4GHz)
* **Memory:** 1 GB LPDDR4X-4267 SDRAM
* **Operating System:** Raspberry Pi OS Lite (64-bit, Debian base, headless)
* **Filesystem Architecture:** Read-only root via OverlayFS with isolated write target for capture storage

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
* **Camera Dashboard:** [http://optic.local:8000/](http://optic.local:8000/)

## SSH Access

Connect over the local network via:

```bash
ssh liam@optic.local
```

## Hardening Verification

Run the read-only hardening verifier directly on `optic` without installing it:

```bash
cd /Users/admin/Documents/projects/optic
ssh liam@optic.local 'bash -s -- --color' < verify.sh
```

Results use green `PASS`, yellow `WARN`, red `FAIL`, and cyan `INFO` labels. Use `--no-color` for plain output. No settings are changed and no secret values are displayed.

Run one phase at a time with `--phase 1` through `--phase 9`. Phase 7 battery configuration is skipped on this deployment, Phase 8 commissions the HQ Camera, and Phase 9 enables OverlayFS. Reusable setup scripts are stored in `/Users/admin/Documents/projects/optic/scripts` where automation is appropriate.

## Camera Daemon

The `optic-daemon` Phase 1 source provides the embedded camera dashboard, IMX477 status, validated camera controls, profile-aware MJPEG previews and test shots, and atomic publication to the Phase 6 RAM transfer stage. Camera requests enter the bounded FIFO `optic_camera` actor and use one persistent native `libcamera` owner for preview and still capture. Master Archive previews at the full `4056 × 3040` resolution at 2 FPS; 4K DCI and 2K Binning use aspect-correct `1352 × 720` and `1014 × 760` previews at up to 8 FPS. Still captures remain Master Archive (`4056 × 3040`, JPEG Q100 + DNG), 4K DCI (`4056 × 2160`, JPEG Q95 with optional DNG), and 2K Binning (`2028 × 1520`, JPEG Q85). The installed service remains on the previous backend until this source is deployed after soak testing. It runs as `liam`'s persistent systemd user service and is intentionally exposed on unprivileged TCP port `8000`; nothing listens on port `80`.

```bash
ssh liam@optic.local 'systemctl --user status optic-daemon.service'
curl http://optic.local:8000/healthz
```

Bootstrap the pinned Pi build environment, upload the current versioned source,
run all validation, build, install, and verify the deployment with one command
from the development Mac:

```bash
cd /Users/admin/Documents/projects/optic
./scripts/build-deploy-optic-daemon.sh
```

See [`optic-daemon.md`](optic-daemon.md) for the deployment procedure, API,
and live-validation record. See
[`optic-daemon-build-environment.md`](optic-daemon-build-environment.md) for
the pinned Raspberry Pi toolchain, native-library sysroot, exact build order,
and failure diagnosis.
