# Design Document: `optic-daemon` (Decoupled Systems Architecture)

`optic-daemon` is a modular, single-binary systems service written in **Rust** for the Raspberry Pi 5 under headless Raspberry Pi OS Lite and an immutable root filesystem (OverlayFS).

It organizes the operational lifecycle of a 365-day autonomous timelapse into **three strictly decoupled subsystems** that communicate primarily via the persistent filesystem (`/mnt/capture`) and a single shared hardware mutex.

> **Implementation status:** The three-subsystem design below is the target architecture. The deployment validated on 2026-09-16 implements **Phase 1 (`optic_web`) only**. `optic_scheduler` is not yet implemented. `optic_sync` (Data Sync Manager) **is now implemented in-process** (see [section 6](#6-subsystem-3-data-sync-manager-optic_sync)) — but its actual protocol and configuration differ from the target design below: it reuses the existing restricted-SSH `ping`/`put` transport (`scripts/optic-capture-receiver.sh`, unchanged) rather than `scp`/`rsync` to `imac.local`, and is configured via `OPTIC_SYNC_*` environment variables rather than `config.toml` (which does not exist — see `worklogs/2026-09-18-data-sync-manager.md` for the full rationale). The independent Phase 6 systemd timer (`scripts/optic-capture-transfer.sh`) still runs in parallel pending a separate, explicit cutover decision; both are safe to run concurrently since `optic_sync` only ever touches `optic-web-*.jpg`/`.dng` files.

## Deployed Phase 1

The current source serves the dashboard from disk (see [dynamic web asset loading](#web-asset-loading) below) and implements:

* IMX477 probing and status telemetry.
* Validated rotation, flip, AWB, metering, exposure, EV, gain, shutter, and denoise controls.
* A bounded FIFO `optic_camera` actor that is the sole camera entry point for `optic_web`; it owns a persistent native `libcamera` backend and serializes preview and still pipeline transitions.
* A full-resolution `4056 × 3040` Master Archive MJPEG preview at 2 FPS, plus lower-bandwidth profile-correct 4K DCI and 2K previews at up to 8 FPS.
* Profile-specific JPEG and optional DNG captures published as `testshot-<profile>-*` in `/mnt/capture` for `optic_sync`/the existing verified transfer timer. (Named `testshot-` — not `optic-web-` — to make manually-triggered captures from the dashboard easy to distinguish from real timelapse frames once `optic_scheduler` exists; see `worklogs/2026-09-18-data-sync-manager.md`. The dedicated "Profile test shot" preview-only button/`POST /api/test-shot` endpoint was removed as unused — "Capture & transfer" is now the only capture control.)

The installed service remains on the previously validated CLI backend until the
native source completes soak testing and deployment.

The canonical LAN URL is [http://optic.local:8000/](http://optic.local:8000/). Phase 1 runs as an unprivileged systemd user service. On the validated Pi, `net.ipv4.ip_unprivileged_port_start=1024`, no process listens on TCP port `80`, and noninteractive sudo is unavailable; port `80` is therefore not part of this deployment.

---

## 1. High-Level Architectural Vision

Instead of tight in-memory coupling (shared message buses or cross-thread actor channels), the daemon is partitioned into four independent workers:

1. **Web Service (`optic_web`):** The operator control plane. Serves the dashboard from disk, provides direct-to-hardware optical calibration/focus streaming, and performs atomic mutations to `/mnt/capture/config.toml`.
2. **Timelapse Scheduler (`optic_scheduler`):** The autonomous capture engine. Watches the configuration file for changes, calculates solar and interval cadences, claims the camera sensor, executes `rpicam-still`, and drops finished frames into `/mnt/capture/queue/`. It contains zero networking logic.
3. **Data Sync Manager (`optic_sync`):** The offline-resilient transport pipeline. Periodically scans `/mnt/capture` for capture files and drains them to the configured receiver over SSH. It has zero awareness of camera state or exposure logic. (Implemented; see section 6 for how this differs from the design below.)
4. **Capture History Log (`optic_capture_log`):** A passive observer of every capture request (from `optic_web` today, `optic_scheduler` once implemented) — settings, total duration, outcome, and output artifact metadata. Writes a small per-capture log file alongside the JPEG/DNG (synced to the receiver by `optic_sync` like any other capture file) and a row in a local SQLite database for fast dashboard queries. It never touches the camera or the network itself. (Implemented, unit-tested, deployed as part of `optic-daemon` 0.1.16, and hardware-validated on the Pi (success and failure captures, `optic_sync` transfer, restart-survives persistence). Per-stage timing breakdown is deferred, not yet persisted — see `docs/optic-daemon-capture-log.md` and `worklogs/2026-09-18-capture-history-module.md`.)

> The box diagram below shows all four subsystems as distinct boxes and how
> they relate (redrawn top-to-bottom for clarity, with 1 and 2 side-by-side
> since they're independent capture triggers that both feed the same shared
> `optic_camera` actor). It shows the *logical* relationships between
> subsystems, not literal current-vs-original-design implementation detail —
> see sections 4-6 below and each subsystem's own doc for that.

```
┌────────────────────────────┐        ┌──────────────────────────────────────────┐
│ 1. Web Service (optic_web) │        │ 2. Timelapse Scheduler (optic_scheduler) │
│   Dashboard UI + manual    │        │   Autonomous interval /                  │
│   "Capture & transfer"     │        │   solar-cadence shots                    │
└────────────────────────────┘        └──────────────────────────────────────────┘
              │                                            │
              ┬─────────────────────┴──────────────────────┬
                      capture request (from 1 or 2)
                                    ▼
                ┌──────────────────────────────────────┐
                │ optic_camera (actor)                 │
                │   Shared hardware owner (not one of  │
                │   the 4 subsystems); used by 1 and 2 │
                └──────────────────────────────────────┘
                  capture completed (incl. failures)
                                   ▼
          ┌─────────────────────────────────────────────────┐
          │ 4. Capture History Log (optic_capture_log)      │
          │   • writes <name>.log.json next to the JPEG/DNG │
          │   • writes a row into local history.db (SQLite) │
          └─────────────────────────────────────────────────┘
                     .log.json joins the JPEG/DNG
                                   ▼
        ┌─────────────────────────────────────────────────────┐
        │ 3. Data Sync Manager (optic_sync)                   │
        │   Drains /mnt/capture over SSH to the receiving Mac │
        └─────────────────────────────────────────────────────┘
                                   ▼
                        [ Receiving Mac Host ]
```

---

## 2. Core Invariants & Hardware Constraints

* **Hardware Mutual Exclusion:** The Sony IMX477 CSI-2 bus cannot be opened concurrently. A shared `Arc<Mutex<()>>` (or lockfile `/run/optic/camera.lock`) arbitrates access between the Web Service (interactive calibration) and the Scheduler (automated shots).
* **Crash & Network Isolation:** If the iMac goes offline or local Wi-Fi stalls, `optic_sync` backs off. The `optic_scheduler` continues firing captures without blocking.
* **OverlayFS Boundary:** The Phase 1 binary lives at `/home/liam/.local/bin/optic-daemon`. Captures use the bounded `/mnt/capture` tmpfs.
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
  GET / (Dashboard UI)        POST /api/stream/start
        │                                 │
        ▼                                 ▼
  Read from Disk               Acquire Camera Hardware Mutex
  (asset_dir sibling)                     │
                                          ▼
                              Queue optic_camera Actor
                                         │
                                         ▼
                            Native libcamera Preview Pipeline

```

### Responsibilities

* **Single-Page Web Application:** Serves HTML5, CSS, and Vanilla JavaScript loaded from disk on every request (see below).
* **Hardware Tuning & Calibration:**
* Starts a live low-latency preview stream automatically when the dashboard opens (`POST /api/stream/start`) to align the 6mm CS-mount lens rings.
* Applies camera-control changes to a running dashboard preview through a debounced `POST /api/stream/reconfigure`; the daemon transparently reconfigures the native camera pipeline and the browser reconnects after a brief frame gap.
* Reports control-to-painted-frame latency, a rolling ten-sample median, and AE/AWB metadata stability in the live dashboard for before/after responsiveness comparisons.
* Keeps the preview active while the dashboard is open, restores it after still captures, and sends a best-effort stop request only when the page is left.
* Publishes a manual profile-specific capture (`POST /api/capture`) to the RAM transfer stage with hidden temporary files and atomic renames.

### Web Asset Loading

Web assets are read from disk on every request instead of being embedded at compile time with `include_str!`. The route set is asset-agnostic: `GET /` always serves `index.html`, and `GET /{*asset}` serves whatever file exists at that relative path under the asset directory — no route needs to be registered per file. Dropping a new file (including inside a subdirectory, e.g. `icons/favicon.ico`) into the asset directory makes it servable immediately; editing an existing file takes effect on the next browser refresh, with no Rust rebuild.

* **Resolution order:** `OPTIC_WEB_ASSETS_DIR` (explicit override) takes precedence; otherwise the daemon looks for a `web/` directory next to its own executable, resolved via `std::env::current_exe()`. If neither exists, it falls back to the workspace's `src/web` directory (compiled in via `CARGO_MANIFEST_DIR`) so `cargo run`/`cargo test` hot-reload during local development.
* **Deployed layout:** `scripts/setup-optic-daemon-phase-01.sh` mirrors the entire `src/web/` directory into `~/.local/bin/web/`, alongside the `optic-daemon` binary at `~/.local/bin/optic-daemon`, rather than copying a fixed file list.
* **Path containment:** the requested path is decomposed into path components and only plain (`Normal`) components are accepted — a leading `/`, a `..` segment, or a `.` segment anywhere in the request is rejected before the filesystem is touched (`400 Bad Request`, no path echoed back). The resolved file's canonical path (after symlinks are followed) is then re-checked to still be inside the canonical asset directory before it is read, so a symlink placed inside the asset directory cannot be used to read a file outside it either. Traversal attempts are logged with `tracing::warn!`.
* **Content type:** inferred from the file extension (`html`, `js`/`mjs`, `css`, `json`/`map`, `svg`, `png`, `jpg`/`jpeg`, `ico`, `webp`, `woff`/`woff2`, `txt`), defaulting to `application/octet-stream` for anything else.
* **Missing or unreadable assets:** each request-time read failure is logged with the resolved path and OS error via `tracing::error!`, and the route returns `500 Internal Server Error` with a short plain-text body instead of panicking; the Content-Security-Policy, `X-Content-Type-Options`, and `Cache-Control: no-store` headers are still applied to that error response.
* **Headers unchanged:** successful responses keep the same `Content-Type`, CSP, `X-Content-Type-Options: nosniff`, and `Cache-Control: no-store` headers as the previous compile-time-embedded implementation.

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
│ [ Capture & transfer ]                           │ Shutter (µs)                               Auto │
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

The dashboard does not expose a separate JPEG-quality override. Quality is part of each preset. Master Archive saves its companion DNG by default but it can be unchecked (2026-09-20 — it was previously mandatory), 4K DCI offers DNG as an opt-in, and 2K Binning disables DNG to preserve its efficiency goal. The checkbox is a real staged/saveable setting (`POST /api/config/save-dng`), not just a one-off per-capture choice — Save Settings persists it, and scheduled (timelapse) captures use whatever was last saved, same as every other camera setting — see `docs/optic-daemon-scheduler.md` §6. Master Archive's live MJPEG preview uses the full `4056 × 3040` output at 2 FPS. The lower-bandwidth 4K DCI and 2K previews use `1352 × 720` and `1014 × 760` output while retaining their selected sensor modes and framing at up to 8 FPS. Changing profile while preview is live restarts the camera pipeline.


### Web API Endpoints

* `GET /`: Serves the single-page application, loaded from disk on each request (see [Web Asset Loading](#web-asset-loading)).
* `GET /{*asset}`: Serves any other file under the resolved asset directory by its relative path (e.g. `/app.js`, `/styles.css`, `/icons/favicon.ico`), confined to that directory (see [Web Asset Loading](#web-asset-loading)).
* `GET /healthz`: Returns `200 OK` while the HTTP service is available.
* `GET /api/status`: Returns version, uptime, IMX477 detection, camera ownership, stream state, and RAM-stage queue usage.
* `POST /api/stream/start`: Called automatically when the dashboard opens. Accepts camera `settings` plus `profile` and configures the persistent native camera for that profile's preview dimensions and sensor mode. Returns `409 Conflict` if the camera is in use.
* `POST /api/stream/reconfigure`: Accepts camera `settings` plus `profile` and serializes a native pipeline stop/reconfiguration/start. Returns `409 Conflict` if preview is not running.
* `POST /api/stream/stop`: Used by dashboard page teardown to stop the native request loop and leave the acquired camera ready for reconfiguration; it is not exposed as a manual UI control.
* `GET /api/stream/mjpeg`: Multipart MJPEG video feed for direct browser `<img>` rendering during lens tuning.
* `POST /api/capture`: Accepts camera settings plus `profile` (`master_archive`, `dci_4k`, or `binning_2k`) and `save_dng`. It captures to hidden files under `/mnt/capture`, applies mode `0640`, and atomically renames the JPEG and any DNG for the transfer timer. The response lists every queued file and the aggregate byte count.
* `POST /api/sync/pause`: Pauses `optic_sync`'s drain loop (it keeps scanning and reporting queue depth, but stops attempting transfers) and returns the fresh sync status.
* `POST /api/sync/resume`: Un-pauses `optic_sync` and returns the fresh sync status.
* `POST /api/sync/retry-now`: Clears any active backoff so the next scan attempts a transfer immediately, and returns the fresh sync status.

---

## 5. Subsystem 2: Timelapse Scheduler (`optic_scheduler`)

> **Not implemented.** The section below is the original single-mode
> target design (fixed interval *or* solar-adaptive bands, a
> `config.toml`/`spa`-crate design that doesn't match this codebase's
> real config storage). It's superseded by a composable-rules
> architecture proposed in `docs/optic-daemon-scheduler.md` — see that
> doc for the current design and its own list of open decisions before
> implementation starts. Kept here for historical/motivational context
> only.

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

### As Implemented

`optic_sync` (`src/optic_sync.rs`) runs in-process as a bounded-actor task,
following the same shape as `optic_camera`: one owning `tokio::task`, a
command channel for control, and a `watch` channel publishing a status
snapshot. It replaces the Phase 6 shell script's *logic* (not, yet, its
deployment — the systemd timer still runs too; see the implementation-status
note above) while deliberately reusing the Phase 6 script's already-deployed
*wire protocol*, so `scripts/optic-capture-receiver.sh` on the receiving Mac
needs no changes:

* **Scanning, not `inotify`:** every 5 seconds it lists `/mnt/capture` and
  filters to files matching the daemon's own capture naming pattern
  (`optic-web-*.jpg` / `optic-web-*.dng`) — an explicit allowlist, not "every
  non-dotfile" like the shell script, which could otherwise sweep up and
  delete `config.json`/`preview_config.json`. Chosen over real `inotify`
  watching for lower implementation risk; still faster than the shell
  timer's 15s interval. Files are drained oldest-first (by mtime).
* **Transport:** for each file, shells out to the system `ssh` binary with
  the same options the shell script uses (`BatchMode`, `IdentitiesOnly`,
  `StrictHostKeyChecking`, a pinned `UserKnownHostsFile`, a dedicated
  identity file, `ConnectTimeout=5`), sends `put <base64-name> <size>
  <sha256>` as the forced command, streams the file over stdin, and requires
  the exact `OK stored ...`/`OK existing ...` response the receiver already
  returns. SHA-256 is computed with the `sha2` crate; the base64 encoder is
  a small inline implementation (RFC 4648, standard alphabet) rather than a
  new dependency. Only deletes the local file after re-checking its size and
  mtime haven't changed since the upload started.
* **Backoff:** any transport failure halts the drain for that cycle and
  doubles the retry interval (30s → 60s → 120s → 240s → 480s → capped at
  15m), matching the numbers in the original design below. A successful
  drain (or an operator-triggered retry) resets it.
* **Configuration:** `OPTIC_SYNC_REMOTE_HOST` (the only required variable —
  its absence leaves the manager permanently `disabled`, which is expected
  and harmless for local development), `OPTIC_SYNC_REMOTE_PORT`,
  `OPTIC_SYNC_REMOTE_USER`, `OPTIC_SYNC_IDENTITY_FILE`,
  `OPTIC_SYNC_KNOWN_HOSTS_FILE`, and `OPTIC_SYNC_ENABLED=false` as an
  operator escape hatch. Defaults match the values the Phase 6 shell script
  already uses on the Pi, so pointing both at the same key/known_hosts file
  requires no new provisioning. There is no `config.toml`, no boot-time
  config pull, and no config-mirroring-to-remote — those depend on
  `optic_scheduler`'s remote-authoritative config concept, which doesn't
  exist yet (see the "As Originally Designed" section below).
* **API and UI:** `GET /api/status` includes a `sync` object (connectivity,
  paused, queued/transferred file counts and bytes, last error, backoff
  seconds, seconds until next retry). `POST /api/sync/pause`,
  `POST /api/sync/resume`, and `POST /api/sync/retry-now` provide operator
  control, each returning the fresh status. The dashboard's "Data Sync"
  panel (next to Runtime telemetry) renders this and wires the three
  buttons.

### As Originally Designed (not implemented)

The rest of this section is kept as the original target design for the
parts that remain unimplemented — a boot-time config pull/mirror tied to a
remote-authoritative `config.toml`, which depends on `optic_scheduler`
existing first.

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

### Responsibilities (original design)

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