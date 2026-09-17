# Design Document: `optic-daemon` (Decoupled Systems Architecture)

`optic-daemon` is a modular, single-binary systems service written in **Rust** for the Raspberry Pi 5 under headless Raspberry Pi OS Lite and an immutable root filesystem (OverlayFS).

It organizes the operational lifecycle of a 365-day autonomous timelapse into **three strictly decoupled subsystems** that communicate primarily via the persistent filesystem (`/mnt/capture`) and a single shared hardware mutex.

> **Implementation status:** The three-subsystem design below is the target architecture. The deployment validated on 2026-09-16 implements **Phase 1 (`optic_web`) only**. Scheduler and in-process sync behavior described in later sections is not yet implemented. Capture transfer currently remains the independent Phase 6 systemd timer.

## Deployed Phase 1

The current source embeds the dashboard and implements:

* IMX477 probing and status telemetry.
* Validated rotation, flip, AWB, metering, exposure, EV, gain, shutter, and denoise controls.
* A bounded FIFO `optic_camera` actor that is the sole camera entry point for `optic_web`; it owns a persistent native `libcamera` backend and serializes preview and still pipeline transitions.
* A full-resolution `4056 × 3040` Master Archive MJPEG preview at 2 FPS, plus lower-bandwidth profile-correct 4K DCI and 2K previews at up to 8 FPS.
* Test shots rendered with the selected capture profile and returned directly from `/dev/shm`.
* Profile-specific JPEG and optional DNG captures published as `optic-web-<profile>-*` in `/mnt/capture` for the existing verified transfer timer.

The installed service remains on the previously validated CLI backend until the
native source completes soak testing and deployment.

The canonical LAN URL is [http://optic.local:8000/](http://optic.local:8000/). Phase 1 runs as an unprivileged systemd user service. On the validated Pi, `net.ipv4.ip_unprivileged_port_start=1024`, no process listens on TCP port `80`, and noninteractive sudo is unavailable; port `80` is therefore not part of this deployment.

---

## 1. High-Level Architectural Vision

Instead of tight in-memory coupling (shared message buses or cross-thread actor channels), the daemon is partitioned into three independent workers:

1. **Web Service (`optic_web`):** The operator control plane. Serves the embedded dashboard, provides direct-to-hardware optical calibration/focus streaming, and performs atomic mutations to `/mnt/capture/config.toml`.
2. **Timelapse Scheduler (`optic_scheduler`):** The autonomous capture engine. Watches the configuration file for changes, calculates solar and interval cadences, claims the camera sensor, executes `rpicam-still`, and drops finished frames into `/mnt/capture/queue/`. It contains zero networking logic.
3. **Data Sync Manager (`optic_sync`):** The offline-resilient transport pipeline. Watches `/mnt/capture/queue/` and `/mnt/capture/config.toml` using filesystem events (`inotify`). It drains files to the iMac host over SSH/rsync and pulls configuration updates on boot. It has zero awareness of camera state or exposure logic.

```
                              [ iMac Host (Master Archive) ]
                                      ▲              │
                      Push frames &   │              │ Boot config pull
                      config updates  │              │ (scp / rsync)
                                      │              ▼
┌─────────────────────────────────────┼─────────────────────────────────────────────┐
│ optic-daemon                        │                                             │
│                                     │                                             │
│  ┌───────────────────────────────┐  │   ┌──────────────────────────────────────┐  │
│  │ 1. Web Service (Axum)         │  │   │ 3. Data Sync Manager                 │  │
│  │  • UI Dashboard & Tuning      │  │   │  • inotify directory watcher         │  │
│  │  • Focus Video Stream         │  │   │  • Sequential FIFO offload           │  │
│  │  • Hardware Mutex Arbiter     │  │   │  • Offline retry & backoff           │  │
│  └──────────────┬────────────────┘  │   └──────────────────▲───────────────────┘  │
│                 │                   │                      │                      │
│                 │ Atomic rewrite    │                      │ Watches queue/ via   │
│                 │ on UI Save        │                      │ inotify              │
│                 ▼                   │                      │                      │
│       ┌───────────────────┐         │            ┌─────────┴──────────┐           │
│       │    config.toml    ├─────────┘            │  /mnt/capture/     │           │
│       │   (SSD Storage)   │                      │        queue/      │           │
│       └─────────┬─────────┘                      └─────────▲──────────┘           │
│                 │                                          │                      │
│                 │ Reloads on mtime change                  │ Drops captured       │
│                 ▼                                          │ frames (JPEG/DNG)    │
│  ┌───────────────────────────────┐                         │                      │
│  │ 2. Timelapse Scheduler        ├─────────────────────────┘                      │
│  │  • Solar ephemeris engine     │                                                │
│  │  • Autonomous interval timer  │                                                │
│  │  • Executes rpicam-still      │                                                │
│  └───────────────────────────────┘                                                │
│                                                                                   │
└───────────────────────────────────────────────────────────────────────────────────┘

```

---

## 2. Core Invariants & Hardware Constraints

* **Hardware Mutual Exclusion:** The Sony IMX477 CSI-2 bus cannot be opened concurrently. A shared `Arc<Mutex<()>>` (or lockfile `/run/optic/camera.lock`) arbitrates access between the Web Service (interactive calibration) and the Scheduler (automated shots).
* **Crash & Network Isolation:** If the iMac goes offline or local Wi-Fi stalls, `optic_sync` backs off. The `optic_scheduler` continues firing captures without blocking.
* **OverlayFS Boundary:** The Phase 1 binary lives at `/home/liam/.local/bin/optic-daemon`. Captures use the bounded `/mnt/capture` tmpfs and temporary test shots use `/dev/shm`.
* **Resource Limits:** The user unit enforces `MemoryHigh=350M`, `MemoryMax=500M`, and `TasksMax=32`. Idle RSS measured 4.0 MiB during deployment validation; long-duration memory behavior is not yet established.

---

## 3. Technology Stack & Crates

```toml
[package]
name = "optic-daemon"
version = "0.1.3"
edition = "2024"

[dependencies]
async-stream = "0.3"
axum = { version = "0.8", features = ["json"] }
bytes = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["fs", "io-util", "macros", "net", "process", "rt-multi-thread", "signal", "sync", "time"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
```

---

## 4. Subsystem 1: Web Service (`optic_web`)

The Web Service acts as the interactive portal for operator configuration and optical lens adjustment.

```
                   Browser Client
                         │
        ┌────────────────┴────────────────┐
        │                                 │
  GET / (Embedded UI)         POST /api/stream/start
        │                                 │
        ▼                                 ▼
   include_str!                Acquire Camera Hardware Mutex
  Static Assets                           │
                                          ▼
                              Queue optic_camera Actor
                                         │
                                         ▼
                            Native libcamera Preview Pipeline

```

### Responsibilities

* **Single-Page Web Application:** Serves HTML5, CSS, and Vanilla JavaScript embedded at compile time with `include_str!`.
* **Hardware Tuning & Calibration:**
* Starts a live low-latency preview stream automatically when the dashboard opens (`POST /api/stream/start`) to align the 6mm CS-mount lens rings.
* Applies camera-control changes to a running dashboard preview through a debounced `POST /api/stream/reconfigure`; the daemon transparently reconfigures the native camera pipeline and the browser reconnects after a brief frame gap.
* Reports control-to-painted-frame latency, a rolling ten-sample median, and AE/AWB metadata stability in the live dashboard for before/after responsiveness comparisons.
* Keeps the preview active while the dashboard is open, restores it after still captures, and sends a best-effort stop request only when the page is left.
* Provides an immediate single-shot JPEG test capture using the selected profile (`POST /api/test-shot`).
* Publishes a manual profile-specific capture (`POST /api/capture`) to the RAM transfer stage with hidden temporary files and atomic renames.

### Web UI design

```
┌─────────────────────────────────────────────────┬─────────────────────────────────────────────────┐
│ LIVE VIEW                                       │ CAPTURE PROFILE                                 │
│ Focus & Framing               Live 4056 × 3040  │ [●] Master Archive       [ ] 4K DCI Widescreen  │
│ ┌─────────────────────────────────────────────┐ │     4056 × 3040 (12-bit)     4056 × 2160 (12-bit) │
│ │                                             │ │                                                 │
│ │                                             │ │ [ ] 2K Binning (Low-Noise)                      │
│ │                                             │ │     2028 × 1520 (10-bit)                        │
│ │                                             │ ├─────────────────────────────────────────────────┤
│ │                                             │ │ SENSOR & EXPOSURE CONTROLS       Reset defaults │
│ │                                             │ │ White balance      Metering                     │
│ │                                             │ │ [ Daylight      ▾] [ Centre                  ▾] │
│ │                                             │ │ Exposure mode      Denoise                      │
│ │                                             │ │ [ Normal        ▾] [ Auto                    ▾] │
│ └─────────────────────────────────────────────┘ │ Analogue gain                              7.3  │
│                                                 │ ───────●─────────────────────────────────────── │
│ [ Profile test shot ] [ Capture & transfer ]     │ Shutter (µs)                               Auto │
│                                                 │ [ 0                                           ] │
│                                                 ├─────────────────────────────────────────────────┤
│                                                 │ SYSTEM & RUNTIME TELEMETRY                      │
│                                                 │ Sensor: IMX477 (HQ)  •  Storage: 683 GB / yr    │
│                                                 │ Interval: 5m         •  Queue: 0 files          │
└─────────────────────────────────────────────────┴─────────────────────────────────────────────────┘
```

Here is the complete engineering and operational specification for each capture profile on the Raspberry Pi 5 with the IMX477 HQ Camera.

---
## CAPTURE PROFILE

### 1. Master Archive (`4056 × 3040`)

Designed as an uncompromised digital negative archive for gallery-grade prints, horizon leveling, and multi-axis digital pans/zooms during post-production.

* **Sensor Resolution:** $4056 \times 3040$ (12.3 MP, native 4:3)
* **Sensor Mode:** `SBGGR12_CSI2P` (12-bit native Bayer, packed)
* **Crop Factor / FOV:** $1.0\times$ (Full diagonal field of view, no cropping)
* **Image Output:**
* **Primary:** Lossless / Near-lossless JPEG (Quality: **98–100**) or PNG.
* **Companion RAW:** Adobe Digital Negative (**`.dng`**, 12-bit un-demosaiced Bayer).


* **Average File Footprint:**
* JPEG only: **~6.5 MB – 8.0 MB** per shot
* JPEG + DNG: **~30 MB – 32 MB** per shot


* **1-Year Projection (@ 5-min interval, ~105,120 shots):**
* JPEG only: **~680 GB** (Requires a 1 TB NVMe SSD)
* JPEG + DNG: **~3.2 TB** (Requires a high-capacity external SSD or NAS synchronization daemon)


* **ISP Processing Cost:** ~400–600 ms per shot for 12-bit demosaicing, tone-mapping, and DNG serialization.
* **Best Use Case:** Outdoor landscapes, architectural construction, dynamic sky/cloud cycles where shadow recovery and highlight roll-off are critical.

---

### 2. 4K DCI Widescreen (`4056 × 2160`)

Optimized for direct integration into cinematic video editing workflows (DaVinci Resolve, Final Cut Pro, Premiere) without requiring manual letterboxing or batch aspect-ratio conversion.

* **Sensor Resolution:** $4056 \times 2160$ (8.8 MP, 17:9 DCI aspect ratio)
* **Sensor Mode:** `SBGGR12_CSI2P` (12-bit native Bayer)
* **Crop Factor / FOV:** Vertical crop applied by the ISP/sensor windowing (~$1.4\times$ vertical crop; horizontal FOV remains 100% full-width).
* **Image Output:**
* **Primary:** JPEG (Quality: **92–95**, visually indistinguishable from source in video playback).
* **Companion RAW:** Optional DNG (Cropped Bayer matrix).


* **Average File Footprint:**
* JPEG only: **~3.8 MB – 4.5 MB** per shot
* JPEG + DNG: **~21 MB – 23 MB** per shot


* **1-Year Projection (@ 5-min interval, ~105,120 shots):**
* JPEG only: **~420 GB** (Comfortably fits on a standard 512 GB NVMe drive)
* JPEG + DNG: **~2.3 TB**


* **ISP Processing Cost:** ~300–450 ms per capture cycle.
* **Best Use Case:** Projects intended exclusively for 4K / UHD video deliverables where you want the exact framing locked in advance without wasting storage on the sky/ground margins of a 4:3 frame.

---

### 3. 2K Binning (`2028 × 1520`)

Engineered for minimal storage consumption, high reliability, and superior signal-to-noise ratio in low-light environments.

* **Sensor Resolution:** $2028 \times 1520$ (3.1 MP, 4:3 aspect ratio)
* **Sensor Mode:** `SBGGR10_CSI2P` or `SBGGR12_CSI2P` ($2\times 2$ on-sensor analog pixel binning)
* **Crop Factor / FOV:** $1.0\times$ (Full sensor field of view is maintained; pixels are grouped rather than cropped).
* **Image Output:**
* **Primary:** Standard JPEG (Quality: **85–90**).
* **Companion RAW:** Typically disabled (saving DNG on a binned mode defeats its efficiency purpose).


* **Average File Footprint:**
* JPEG only: **~1.2 MB – 1.8 MB** per shot


* **1-Year Projection (@ 5-min interval, ~105,120 shots):**
* JPEG only: **~150 GB – 190 GB** (Easily fits on a modest 256 GB drive or robust High Endurance microSD card)


* **ISP Processing Cost:** ~100–180 ms per shot. Minimal CPU/ISP thermal dissipation.
* **Low-Light Advantage:** By grouping adjacent $2\times 2$ pixel clusters directly on the silicon before analog-to-digital conversion, the effective photosite area quadruples, substantially reducing read noise during dawn, dusk, and night shots.
* **Best Use Case:** Long-term weather logging, plant growth tracking, power-constrained/solar installations, or deployments where files are uploaded over a cellular/remote uplink.

---

### Profile Comparison Matrix

| Specification | Master Archive | 4K DCI Widescreen | 2K Binning |
| --- | --- | --- | --- |
| **Pixel Grid** | $4056 \times 3040$ | $4056 \times 2160$ | $2028 \times 1520$ |
| **Effective MP** | 12.3 MP | 8.8 MP | 3.1 MP |
| **Aspect Ratio** | 4:3 | 17:9 (~16:9) | 4:3 |
| **Bayer Bit Depth** | 12-bit Packed | 12-bit Packed | 10-bit / 12-bit |
| **Sensor Technique** | Full Readout | Sensor Window Crop | $2 \times 2$ Hardware Binning |
| **Default Format** | JPEG (Q100) + DNG | JPEG (Q95) | JPEG (Q85) |
| **Est. Frame Size** | ~6.5 MB (JPG) / ~30 MB (+RAW) | ~4.2 MB (JPG) / ~22 MB (+RAW) | ~1.5 MB (JPG) |
| **1-Yr Storage (5m)** | ~680 GB (JPG) / ~3.2 TB (RAW) | ~420 GB (JPG) / ~2.3 TB (RAW) | ~160 GB (JPG) |
| **Target Output** | Fine Art / Archival / Pan-Zoom | 4K Cinema Timelines | Web Dashboards / Storage-Limited |

The dashboard does not expose a separate JPEG-quality override. Quality is part of each preset. Master Archive always saves its companion DNG, 4K DCI offers DNG as an opt-in, and 2K Binning disables DNG to preserve its efficiency goal. Master Archive's live MJPEG preview uses the full `4056 × 3040` output at 2 FPS. The lower-bandwidth 4K DCI and 2K previews use `1352 × 720` and `1014 × 760` output while retaining their selected sensor modes and framing at up to 8 FPS. Changing profile while preview is live restarts the camera pipeline.


### Web API Endpoints

* `GET /`: Serves the embedded single-page application.
* `GET /healthz`: Returns `200 OK` while the HTTP service is available.
* `GET /api/status`: Returns version, uptime, IMX477 detection, camera ownership, stream state, and RAM-stage queue usage.
* `POST /api/stream/start`: Called automatically when the dashboard opens. Accepts camera `settings` plus `profile` and configures the persistent native camera for that profile's preview dimensions and sensor mode. Returns `409 Conflict` if the camera is in use.
* `POST /api/stream/reconfigure`: Accepts camera `settings` plus `profile` and serializes a native pipeline stop/reconfiguration/start. Returns `409 Conflict` if preview is not running.
* `POST /api/stream/stop`: Used by dashboard page teardown to stop the native request loop and leave the acquired camera ready for reconfiguration; it is not exposed as a manual UI control.
* `GET /api/stream/mjpeg`: Multipart MJPEG video feed for direct browser `<img>` rendering during lens tuning.
* `POST /api/capture`: Accepts camera settings plus `profile` (`master_archive`, `dci_4k`, or `binning_2k`) and `save_dng`. It captures to hidden files under `/mnt/capture`, applies mode `0640`, and atomically renames the JPEG and any DNG for the transfer timer. The response lists every queued file and the aggregate byte count.
* `POST /api/test-shot`: Accepts camera settings plus `profile`, claims the camera lock, captures a JPEG test frame with that profile to `/dev/shm`, and returns it directly without queuing it for sync.

---

## 5. Subsystem 2: Timelapse Scheduler (`optic_scheduler`)

The Scheduler is a completely autonomous loop whose only mission is to take pictures according to the rules in `config.toml` and write them to the storage queue.

```
       Read /mnt/capture/config.toml
                     │
                     ▼
          Calculate Next Execution
       (Solar Elevation / Interval)
                     │
                     ▼
           Wait for Scheduled Tick
                     │
                     ▼
       Acquire Camera Hardware Mutex
                     │
                     ▼
           Execute rpicam-still
                     │
                     ▼
         Save to /dev/shm/frame.jpg
                     │
                     ▼
       Atomic Move to /mnt/capture/queue/
                     │
                     ▼
       Release Camera Hardware Mutex

```

### Responsibilities

* **Config Re-reading:** Continuously tracks the modification time (`mtime`) of `/mnt/capture/config.toml`. If modified by the Web Service or pulled by the Sync Manager, it hot-reloads the capture profile on its next iteration.
* **Solar Ephemeris Calculations:** Computes the solar elevation angle for the station's latitude/longitude using the `spa` crate:
* **Golden Hour ($-6^\circ \le \text{elevation} \le +6^\circ$):** Captures at `golden_hour_interval_sec` (e.g., 60s).
* **Daytime ($\text{elevation} > +6^\circ$):** Captures at `default_interval_sec` (e.g., 300s).
* **Night ($\text{elevation} < -6^\circ$):** Sleeps or switches to `night_interval_sec`.


* **Hardware Execution:**
1. Requests the shared camera lock. If the Web Service is currently in a calibration stream, it yields, logs a contention warning, and retries after a short delay.
2. Spawns `rpicam-still` targeting the RAM disk: `/dev/shm/optic_temp.jpg`.
3. Releases the camera lock.
4. Moves the completed file to `/mnt/capture/queue/optic_YYYYMMDD_HHMMSS.jpg`.


* **Zero External Dependencies:** Never interacts with SSH, rsync, Wi-Fi, or remote hosts.

---

## 6. Subsystem 3: Data Sync Manager (`optic_sync`)

The Data Sync Manager is a dedicated file-transport loop that bridges the local SSD queue to the remote iMac.

```
         inotify Event on /mnt/capture/queue/
                         │
                         ▼
             Collect Unsynced Frames (FIFO)
                         │
                         ▼
               Network Link Check / Ping
                         │
            ┌────────────┴────────────┐
            ▼                         ▼
      [ iMac Online ]           [ iMac Asleep / Offline ]
            │                         │
            ▼                         ▼
      Push via scp/rsync        Backoff Exponentially
            │                   (Leave files in queue/)
            ▼
      Delete Local File
      from /mnt/capture/queue/

```

### Responsibilities

* **Boot-Time Master Pull:**
* When `optic-daemon` launches, `optic_sync` runs a pre-flight network fetch:
```bash
scp -o ConnectTimeout=3 liam@imac.local:/Volumes/Archive/optic/config.toml /mnt/capture/config.toml.tmp

```


* If successful, it renames the file to `/mnt/capture/config.toml`, allowing changes made on the iMac to take effect.
* If the iMac is asleep or unreachable, it logs a warning, skips the step, and lets the Scheduler use the existing `/mnt/capture/config.toml`.


* **Directory Monitoring (`inotify`):**
* Subscribes to write events inside `/mnt/capture/queue/` using the `notify` crate.
* When a new frame appears, it triggers an offload attempt.


* **FIFO Drain & Offline Spooling:**
* Iterates through `/mnt/capture/queue/` in chronological order.
* Sends files via `scp` or `rsync` over key-based SSH.
* On exit status `0`, deletes the local copy from the queue.
* On network error (connection refused, timeout), halts the drain loop and activates an exponential backoff timer (e.g., 30s, 60s, 120s, up to 15m). Frames accumulate safely on the SSD without missing scheduled shots.


* **Config Mirroring:**
* Watches `/mnt/capture/config.toml`. Whenever its `mtime` updates due to a Web UI change, it pushes the updated file back to the iMac master archive:
```bash
scp /mnt/capture/config.toml liam@imac.local:/Volumes/Archive/optic/config.toml

```





---

## 7. Storage Layout & Configuration Specification

### Filesystem Mounts

* `/` (MicroSD): Immutable, read-only via OverlayFS.
* `/mnt/capture` (RAM): Bounded 256 MiB `tmpfs` staging for captures awaiting verified transfer.
* `/dev/shm` (RAM): Volatile test-shot staging.

> The SSD-backed layout and `config.toml` schema below describe the future scheduler/sync architecture, not the deployed Phase 1 storage model.

### Directory Structure on `/mnt/capture`

```text
/mnt/capture/
├── config.toml           # Active operational configuration (read/write)
├── queue/                # FIFO outbound spool directory (watched by inotify)
│   ├── optic_20260915_143000.jpg
│   └── optic_20260915_143500.jpg
└── archive/              # (Optional) Local persistent backup if retained

```

### Configuration Schema (`/mnt/capture/config.toml`)

```toml
[station]
name = "Optic-01"
latitude = 49.2827
longitude = -123.1207
elevation_m = 110.0

[schedule]
paused = false                 # Toggled directly by Web UI buttons
mode = "solar_adaptive"       # "fixed" or "solar_adaptive"
default_interval_sec = 300     # Midday cadence (5 minutes)
golden_hour_interval_sec = 60  # Dawn/Dusk cadence (1 minute)
night_mode = "sleep"           # "sleep" or "slow_interval"
night_interval_sec = 1800      # 30-minute interval if night_mode = "slow_interval"

[exposure]
mode = "aperture_priority"
fixed_gain = 1.0               # Minimum analog gain for maximum dynamic range
min_shutter_us = 100           # 1/10000s
max_shutter_us = 2000000       # 2.0s
awb_mode = "daylight"          # "daylight", "auto", "cloudy"
metering = "centre"            # "centre", "spot", "matrix"

[storage]
jpeg_quality = 95
save_dng_raw = false
remote_host = "liam@imac.local"
remote_dir = "/Volumes/Archive/optic/frames"
remote_config_path = "/Volumes/Archive/optic/config.toml"

```

---

## 8. Rust Implementation Blueprint

### Process Entrypoint (`src/main.rs`)

```rust
use std::sync::Arc;
use tokio::sync::Mutex;
use std::path::PathBuf;

mod web;
mod scheduler;
mod sync;
mod config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let capture_dir = PathBuf::from("/mnt/capture");
    let config_path = capture_dir.join("config.toml");
    let queue_dir = capture_dir.join("queue");

    tokio::fs::create_dir_all(&queue_dir).await?;

    // Shared hardware lock between Web Service and Scheduler
    let camera_lock = Arc::new(Mutex::new(()));

    // 1. Start Data Sync Manager (Queue Watcher & Remote Sync)
    let sync_handle = tokio::spawn(sync::run_sync_manager(
        config_path.clone(),
        queue_dir.clone(),
    ));

    // 2. Start Timelapse Scheduler (Autonomous Engine)
    let scheduler_handle = tokio::spawn(scheduler::run_scheduler(
        config_path.clone(),
        queue_dir.clone(),
        camera_lock.clone(),
    ));

    // 3. Start Web Service (Management UI & Lens Calibration)
    let web_handle = tokio::spawn(web::run_web_service(
        config_path.clone(),
        camera_lock.clone(),
        8000,
    ));

    // Supervise subsystems
    tokio::select! {
        res = sync_handle => tracing::error!("Sync manager exited: {:?}", res),
        res = scheduler_handle => tracing::error!("Scheduler exited: {:?}", res),
        res = web_handle => tracing::error!("Web service exited: {:?}", res),
    }

    Ok(())
}

```

### Future Atomic Configuration Mutation Pattern

This example is for the future scheduler configuration API; Phase 1 does not expose `/api/config`.

```rust
use std::path::Path;
use tokio::io::AsyncWriteExt;

pub async fn atomic_save_config(path: &Path, content: &str) -> anyhow::Result<()> {
    let tmp_path = path.with_extension("toml.tmp");
    let mut file = tokio::fs::File::create(&tmp_path).await?;
    file.write_all(content.as_bytes()).await?;
    file.sync_all().await?;
    tokio::fs::rename(&tmp_path, path).await?;
    Ok(())
}
```

---

## 9. Deployment & Systemd User Service

Phase 1 uses the repository unit at `systemd/optic-daemon.service`. It installs as `/home/liam/.config/systemd/user/optic-daemon.service`, runs `/home/liam/.local/bin/optic-daemon`, binds `0.0.0.0:8000`, and is enabled in the user manager's `default.target`. `loginctl enable-linger liam` must already have been run by an administrator so the user manager starts without an interactive login.

### Installation Sequence

The authoritative native environment and troubleshooting runbook is
[`optic-daemon-build-environment.md`](optic-daemon-build-environment.md). The
ordering below is intentional: tests generate native build artifacts with
staged LLVM 19 available, Clippy runs without that library path, and the
release build restores it for `bindgen`.

1. Install the user-scoped Rust toolchain and required components on the Pi:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
rustup component add rustfmt clippy
```

2. From the versioned project directory on the Pi, validate and build natively:

```bash
export OPTIC_SYSROOT="$HOME/.local/optic-sysroot"
export OPTIC_NATIVE_LIB="$OPTIC_SYSROOT/usr/lib/aarch64-linux-gnu"
export PKG_CONFIG_PATH="$OPTIC_NATIVE_LIB/pkgconfig"
export PKG_CONFIG_SYSROOT_DIR="$OPTIC_SYSROOT"
export LIBCLANG_PATH="$OPTIC_NATIVE_LIB"
export BINDGEN_EXTRA_CLANG_ARGS="-I/usr/lib/gcc/aarch64-linux-gnu/14/include"
unset SYSROOT LD_LIBRARY_PATH
cargo fmt --all -- --check
LD_LIBRARY_PATH="$OPTIC_NATIVE_LIB" cargo test --locked --all-targets
env -u SYSROOT -u LD_LIBRARY_PATH cargo clippy --locked --all-targets -- -D warnings
LD_LIBRARY_PATH="$OPTIC_NATIVE_LIB" cargo build --locked --release
```

3. Install and start the service as `liam`, not with sudo:

```bash
./scripts/setup-optic-daemon-phase-01.sh
```

The installer refuses to proceed unless `/mnt/capture` is a tmpfs and `liam` belongs to both `video` and `render`.

4. Verify the deployed service and dashboard:

```bash
systemctl --user status optic-daemon.service
curl http://127.0.0.1:8000/healthz
curl http://127.0.0.1:8000/api/status
```

Open [http://optic.local:8000/](http://optic.local:8000/) from the LAN. Do not omit `:8000`; no port-80 redirect or proxy is installed.

### Live Validation Record

Validated on the Raspberry Pi 5 on 2026-09-16 UTC:

| Check | Result |
| --- | --- |
| Native format, tests, strict Clippy, release build | Passed; 10 tests passed |
| Installed artifact | Native AArch64 release hash matched the build artifact |
| Service | Enabled and active; IMX477 detected; controlled restart passed |
| Dashboard and API | Embedded profile selector, health, status, security headers, and invalid profile/RAW rejection passed |
| Preview | Master Archive `4056 × 3040`, 4K DCI `1352 × 720`, and 2K `1014 × 760` MJPEG frames verified; live profile reconfiguration passed and stop released the camera |
| Profile test shots | Valid `4056 × 3040`, `4056 × 2160`, and `2028 × 1520` JPEGs returned; temporary files removed |
| Profile captures | Master and 4K DCI JPEG/DNG pairs plus a 2K Binning JPEG produced at the requested modes with mode `0640` |
| Capture and transfer | `optic-web-1789542088039.jpg`, 1,343,700 bytes, transferred and verified to `/Users/admin/Pictures/Optic` |
| Idle resources | 4.0 MiB RSS, 6 tasks |
| Port 80 | Not configured; canonical endpoint is TCP `8000` |
| Reboot persistence | Unit is enabled and `Linger=yes`; an actual reboot test is pending because PolicyKit required interactive authorization |

An administrator can complete the final persistence check by rebooting the Pi, then confirming `systemctl --user is-active optic-daemon.service` and `curl http://127.0.0.1:8000/healthz` as `liam`.

---

## 10. Phase 1 Verification Matrix

| Failure Scenario | System Reaction | Recovery Action |
| --- | --- | --- |
| **iMac Offline / Sleep** | The external transfer timer leaves finished files in the bounded RAM stage. | Restore the iMac/network before the stage fills; queued files are lost if the Pi loses power. |
| **User Leaves Dashboard Open** | The native preview remains active by design. | Close or navigate away from the dashboard to stop it. |
| **Preview Client Disconnects** | The dashboard sends a stop request when the page is left; if that request cannot be delivered, the native preview continues. | Reopen and leave the dashboard to retry teardown, or restart the service. |
| **Web Service Crashes** | systemd restarts the process after three seconds. | Hidden partial files are ignored by the transfer timer; finished captures already transferred to the iMac remain durable. |