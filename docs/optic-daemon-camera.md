# Design Proposal: Low-Latency Multi-Stream Camera Pipeline Architecture

**Component:** Optic Core Camera Engine (`optic_camera`)
**Consumers:** `optic_web` & `optic_scheduler` Subsystems
**Target Platform:** Raspberry Pi 5 (RP1 I/O Controller) + Sony IMX477 (HQ Camera)
**Status:** Approved for Implementation

> **Implementation status (2026-09-16):** The source now connects `optic_camera`
> to a persistent native `libcamera` owner with bounded preview delivery,
> serialized still reconfiguration, JPEG/DNG encoding, and staged publication.
> Linux compilation and an end-to-end IMX477 capture cycle pass. Deployed
> since 0.1.3 (`worklogs/2026-09-16-native-camera-refactor.md`).

### Implementation sequence

1. **Actor boundary (complete):** Route `optic_web` camera operations through one
   in-process `optic_camera` owner; expose backend, busy, streaming, and queue
   telemetry; preserve deployed capture output and profile-aware previews.
2. **Native probe:** Install `libcamera-dev` and `libjpeg62-turbo-dev` on the Pi,
   enable the Rust `libcamera` crate's `vendor_rpi` feature, and validate camera
   enumeration, acquisition, two-stream configuration, buffer mapping, request
   reuse, controls, cancellation, and clean shutdown in an isolated harness.
3. **Native preview:** Replace the legacy preview child with the persistent
   `CameraManager`/camera lease and Stream 0 buffer loop. Add bounded JPEG
   encoding so slow browser clients can only drop frames, never stall capture.
4. **Concurrent still path:** Add Stream 1 capture requests, atomic JPEG/DNG
   publication, and scheduler priority without stopping Stream 0.
5. **Legacy removal:** Remove `rpicam-vid`/`rpicam-still` only after soak,
   latency, memory, output, recovery, and shutdown tests pass on the IMX477.

The current native source passes formatting, host checks and tests, plus Linux
compilation, 18 tests, and Clippy with warnings denied on the Pi. A live isolated
run validated preview start/reconfiguration/stop, JPEG test shots, JPEG-only
capture, JPEG+DNG capture, `0640` publication, and partial-file cleanup. Extended
preview/capture transition soak testing remains before deployment.

### Preview/profile decision gate

Milestone 1 deliberately retains the deployed profile-aware preview dimensions.
Before Milestone 3, the native harness must resolve the proposal's fixed
`1280 × 960` Stream 0 against sensor-mode changes required by Master Archive,
DCI 4K, and true 2K binning. A fixed transport canvas must not silently crop a
profile or replace true sensor binning with software scaling. This decision does
not block the actor boundary or native API qualification.

---

## 1. Executive Summary & Problem Statement

### 1.1 Context

The legacy camera implementation spawns `rpicam-vid` as an external CLI child process to deliver a low-resolution MJPEG/WebRTC viewfinder feed (`1280×960`) to the browser. Whenever an operator modifies an Image Signal Processor (ISP) control (e.g., exposure duration, analogue gain, EV compensation, white balance) or triggers a high-resolution inspection frame, the daemon forcefully terminates the running `rpicam-vid` process and spins up a fresh instance with revised arguments.

### 1.2 Identified Failure Modes

* **Pipeline Tear-Down Penalty:** Process termination triggers full teardown of the Video4Linux2 (`v4l2`) subdevices and RP1-CFE/Unicam bus bindings.

* **Hardware Re-initialization Overhead:** The Linux kernel must renegotiate MIPI CSI-2 bus timings and re-commit the IMX477 I2C sensor register configurations.

* **3A Re-convergence Glitch:** The Image Processing Algorithm (IPA) state machines (Auto-Exposure, Auto-White-Balance) lose historical convergence data. The sensor takes 15–30 frames to reach baseline luminance and chromaticity, producing a **1,000–3,000 ms viewfinder freeze or black screen** in the web interface.

* **Device Lock Contention (`EBUSY`):** Because the V4L2 device node is exclusively claimed by the preview process, on-demand high-resolution captures cannot run concurrently, forcing an interruption of the live view.


---

## 2. Shared Core Architecture & Subsystem Integration

The camera hardware interface is unified inside a single, long-running core module (`optic_camera`) running as an in-process actor/daemon. Both the interactive dashboard (`optic_web`) and the autonomous scheduling loop (`optic_scheduler`) interface with this central coordinator via non-blocking channels rather than competing for direct device access.

```
┌─────────────────────────────────────┐         ┌─────────────────────────────────────┐
│         optic_web subsystem         │         │      optic_scheduler subsystem      │
│  • Viewfinder WebSocket consumer    │         │  • 5-minute cron/interval triggers  │
│  • Dynamic slider mutations         │         │  • Astronomical / solar schedules   │
│  • Manual Shutter (focus check)     │         │  • Archival batch writes to NVMe    │
└──────────────────┬──────────────────┘         └──────────────────┬──────────────────┘
                   │                                               │
                   │ (IPC / Dynamic Controls)                      │ (Event Triggers / Capture Req)
                   ▼                                               ▼
┌─────────────────────────────────────────────────────────────────────────────────────┐
│                                optic_camera Module                                  │
│                             (Persistent Camera Lease)                               │
│                                                                                     │
│  • Maintains long-lived, uninterrupted V4L2/libcamera session                       │
│  • Per-frame ControlList mutation queue (atomic updates)                            │
│  • Serialized capture queue (arbitrates web test shots vs. scheduled ticks)         │
│  • Dual-stream buffer dispatcher:                                                   │
│      - Stream 0 (Viewfinder): Continuous fixed 1280×960 preview feed                │
│      - Stream 1 (Archival): On-demand captures governed by active Capture Profile   │
└──────────────────────────────────────────┬──────────────────────────────────────────┘
                                           │
                                           ▼
                            Raspberry Pi 5 ISP (libcamera)

```

### 2.1 Subsystem Responsibilities

**A. `optic_web` (Interactive Control & Diagnostics)**

* **Viewfinder Streaming:** Subscribes to the continuous **Stream 0** buffer feed. Compresses raw frames via SIMD TurboJPEG and streams them to the browser client via WebSockets.

* **Zero-Downtime Adjustments:** Translates frontend UI interactions (gain sliders, AWB profiles) into `libcamera::ControlList` delta packets submitted directly to the running session.

* **Manual Shutter / Focus Inspection:** Requests an on-demand frame on **Stream 1** using the parameters of the active Capture Profile, returning a high-resolution buffer for instant browser verification without dropping preview frames.



**B. `optic_scheduler` (Autonomous Long-Term Operations)**

* **Cadence Execution:** Fires automated capture triggers (e.g., 5-minute interval ticks, astronomical golden-hour events) against **Stream 1**.

* **Zero Viewfinder Contention:** Live monitoring sessions in `optic_web` continue streaming smoothly when scheduled captures occur.

* **Metadata Correlation:** Queries live exposure telemetry (converged shutter speed, analog gain, scene lux estimates) directly from the module to package detailed EXIF tags alongside saved images.



### 2.2 Concurrency & Hardware Arbitration

* **Serialized Capture Queue:** Capture requests submitted to Stream 1 pass through an internal FIFO queue to eliminate driver re-entrancy bugs.

* **Collision Resolution:** If a manual inspection shot and a scheduled interval tick coincide, the module sequences them across adjacent frame cycles (< 100 ms separation), avoiding `EBUSY` kernel panics.



---

## 3. Pipeline Configuration & Capture Profiles

### 3.1 Dual-Stream Hardware Topologies

The system configures two concurrent output streams out of the Raspberry Pi 5 ISP:

| Property | Stream 0 (Viewfinder / Live View) | Stream 1 (Capture / Archival Still) |
| :--- | :--- | :--- |
| **Consumer** | `optic_web`[cite: 1] | `optic_scheduler` / `optic_web` (Manual Shutter)[cite: 1] |
| **Role** | Continuous framing, live focus, web preview[cite: 1] | Master digital negative / Timelapse production frame[cite: 1] |
| **Resolution** | Fixed **$1280 \times 960$** (4:3)[cite: 1] | **Dynamic: Defined by active Capture Profile** |
| **Pixel Format** | `YUV420`[cite: 1] | **Dynamic: Defined by active Capture Profile** |
| **Framerate** | 30 fps (continuous)[cite: 1] | On-demand (interleaved trigger)[cite: 1] |
| **Buffer Strategy** | 4 ping-pong ring buffers[cite: 1] | 1–2 on-demand capture buffers[cite: 1] |
| **Output Sink** | SIMD/Hardware JPEG $\rightarrow$ WebSocket[cite: 1] | Local NVMe SSD (`.jpg` / `.dng`)[cite: 1] |

---

### 3.2 Stream 1 Capture Profile Matrix

Stream 1 dynamically reconfigures its ISP scaling, cropping, and RAW Bayer stream depending on the active profile selected in the UI:

| Profile | Stream 1 Resolution | Aspect Ratio | Sensor Readout Mode | Primary Output | Companion RAW | Est. Frame Size |
| --- | --- | --- | --- | --- | --- | --- |
| **Master Archive** | 4056 × 3040 | 4:3 Native | Full Uncropped Readout | JPEG (Quality 98–100) | 12-bit Adobe `.dng` (`SBGGR12_CSI2P`) | ~6.5 MB (JPG) / ~30 MB (+RAW) |
| **4K DCI Widescreen** | 4056 × 2160 | 17:9 DCI | ISP Windowing / Crop | JPEG (Quality 92–95) | Optional 12-bit `.dng` (Cropped) | ~4.2 MB (JPG) / ~22 MB (+RAW) |
| **2K Binning** | 2028 × 1520 | 4:3 | 2×2 Hardware Pixel Binning | JPEG (Quality 85–90) | None (Disabled) | ~1.5 MB (JPG) |

---

## 4. Dynamic Control Flow

### 4.1 Per-Frame Parameter Mutation

When the operator adjusts an exposure slider or changes white balance in the UI:

```
[Web Client] ──(WS: {"gain": 7.3, "ev": -0.1})──> [optic_web]
                                                         │
                                               [optic_camera Queue]
                                                         │
                                               libcamera::ControlList
                                                         │
                                        ┌────────────────┴───────────────┐
                                        ▼                                ▼
                          controls::AnalogueGain = 7.3    controls::ExposureValue = -0.1
                                        │                                │
                                        └────────────────┬───────────────┘
                                                         │
                                               Apply to Next Request
                                                         │
                                                         ▼
                                                Sensor Frame N + 1
                                             (Latency: ~16 - 33 ms)

```

No sensor reset, process restart, or I2C clock re-initialization occurs. The hardware ISP applies updated gains and exposure targets at the start of the next vertical blanking interval.

### 4.2 Non-Blocking Dual Capture Sequence

1. The viewfinder cycles buffers continuously on **Stream 0** ($1280 \times 960$)[cite: 1].
2. A trigger arrives via `optic_scheduler` (interval timer) or `optic_web` (manual shutter)[cite: 1].
3. The engine dispatches a multi-stream request configured with the **active Capture Profile**:
   * **Stream 0 buffer** is forwarded to `optic_web` to keep the live preview uninterrupted[cite: 1].
   * **Stream 1 buffer** receives the target resolution and format dictated by the profile (e.g., $4056 \times 3040$ 12-bit for Master Archive, or $2028 \times 1520$ binned).
4. Stream 1's buffer is handed off to an asynchronous background worker pool for JPEG compression and optional DNG serialization to storage[cite: 1].
5. The live feed continues at 30 fps throughout the capture sequence[cite: 1].

---

## 5. Performance Targets & Milestones

| Metric | Legacy CLI Arch (`rpicam-vid` restart)[cite: 1] | Target Multi-Stream Module[cite: 1] |
| :--- | :--- | :--- |
| **Parameter Mutation Latency** | $1,200\text{ ms} - 3,000\text{ ms}$[cite: 1] | $\mathbf{\le 33\text{ ms}}$ (1 frame interval)[cite: 1] |
| **Preview Dropout During Capture** | Complete freeze / stream restart[cite: 1] | $\mathbf{0\text{ ms}}$ (No frame drops)[cite: 1] |
| **Native Capture Latency** | $2,500\text{ ms} - 4,000\text{ ms}$[cite: 1] | $\mathbf{< 120\text{ ms}}$[cite: 1] |
| **Auto-Exposure Stability** | Re-computes from black[cite: 1] | **Continuously converged**[cite: 1] |
| **Subsystem Contention** | Device lock collisions (`EBUSY`)[cite: 1] | **Zero collisions (Serialized Queue)**[cite: 1] |

---

## 6. UI Simplifications Post-Implementation

Once `optic_camera` is deployed:

* **Page-Managed Preview:** The dashboard starts the live feed automatically, restores it after still captures, and leaves it active until the page closes or the service shuts down.


* **Eliminate Preview Stalls on Capture:** Remove UI loading spinners or preview freezes during manual shots and "Capture & transfer".


* **Consolidated Capture Profile Selector:** Replace the manual JPEG quality slider with the top-level Profile selector cards, ensuring resolution, aspect ratio, and companion RAW flags stay locked to tested presets.
* **Relocate Secondary Controls:** Move fine-grained ISP controls (EV compensation, manual gain/shutter) under an explicit `EXPOSURE & SENSOR` group, placing raw hardware diagnostic strings into a secondary telemetry drawer.