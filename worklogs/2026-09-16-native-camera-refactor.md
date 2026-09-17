# Native Camera Refactor Worklog

**Date:** 2026-09-16  
**Implementation snapshot:** `optic-daemon` `0.1.1`  
**Confirmed deployed release:** `optic-daemon` `0.1.3`  
**Target:** Raspberry Pi 5 with Sony IMX477 HQ Camera at `liam@optic.local`  
**Dashboard:** [http://optic.local:8000/](http://optic.local:8000/)  
**Status:** Native implementation, hardware qualification, and deployment complete. Preview-control responsiveness measurement is tracked separately.

## Objective

Replace the original command-line camera integration with an in-process native
camera subsystem while preserving the dashboard API, capture profiles, camera
controls, MJPEG preview, test-shot flow, and publication to `/mnt/capture`.

The refactor needed to establish one long-lived owner of the HQ Camera, serialize
all camera operations, reuse libcamera requests and buffers, and remove normal
runtime dependence on `rpicam-vid` and `rpicam-still` child processes.

## Legacy Baseline

The retained design record describes the old implementation as follows:

- Preview was supplied by an external `rpicam-vid` child process at
  `1280 × 960`.
- Changing an ISP control or requesting a high-resolution inspection image
  terminated the preview child and started a new process with updated CLI
  arguments.
- Still capture used the companion command-line camera path.
- Repeated process replacement tore down and rebuilt the camera pipeline,
  interrupted preview delivery, reset 3A state, and exposed camera ownership to
  process-level timing and `EBUSY` races.

The pre-native source revision is not retained in this checkout or in the Pi's
versioned source snapshots, and this directory has no Git metadata. Consequently,
this worklog records the legacy behavior from the contemporaneous design document
rather than reconstructing unverified command arguments or code details.

## Work Completed

### 1. Single camera entry point

- Added the `OpticCamera` actor as the only camera entry point used by
  `optic_web`.
- Added a bounded FIFO command channel with capacity `16`.
- Serialized probe, stream start/reconfiguration/stop, subscription, test shot,
  staged capture, and shutdown operations.
- Exposed backend, busy, streaming, and queued-command state through
  `/api/status`.
- Added explicit shutdown handling so the camera pipeline stops and the native
  worker joins during daemon termination.

### 2. Persistent native libcamera owner

- Added `NativeCameraBackend`, running on the dedicated `optic-libcamera` OS
  thread.
- Created one `CameraManager`, selected the camera whose model contains
  `imx477`, acquired it once, and retained ownership for the worker lifetime.
- Replaced process spawning with typed commands and one-shot responses between
  the async actor and the synchronous camera worker.
- Added a non-Linux stub so host-side checks can compile without pretending that
  camera hardware exists.

### 3. Native request and buffer lifecycle

- Generated role-specific libcamera configurations for preview, JPEG-only
  capture, and JPEG-plus-RAW capture.
- Configured YUV420 processed streams and profile-specific PiSP compressed Bayer
  RAW streams.
- Allocated and memory-mapped frame buffers through `FrameBufferAllocator`.
- Created multiple cookie-tagged requests, queued them together, processed
  completion callbacks, reused their buffers, reapplied controls, and requeued
  them.
- Used a bounded four-frame broadcast channel. Slow MJPEG consumers can lag and
  drop frames without blocking the camera request loop.

### 4. Capture profiles and native controls

The existing profile policy was preserved:

| Profile | Preview | Still | JPEG | DNG policy |
| --- | --- | --- | --- | --- |
| Master Archive | `4056 × 3040`, 2 FPS | `4056 × 3040` | Q100 | Required |
| 4K DCI | `1352 × 720`, up to 8 FPS | `4056 × 2160` | Q95 | Optional |
| 2K Binning | `1014 × 760`, up to 8 FPS | `2028 × 1520` | Q85 | Disabled |

- Rotation and horizontal/vertical flip map to libcamera orientation.
- AWB, metering, exposure mode, EV, analogue gain, shutter, and denoise map to
  typed libcamera controls.
- Frame-duration limits enforce each profile's preview rate while allowing a
  longer configured shutter.
- ISP-only changes update settings on reused requests without changing stream
  configuration.
- Profile or transform changes stop and recreate the configured native pipeline
  because they affect stream layout or orientation.

### 5. In-process JPEG and DNG output

- Added direct YUV420-to-JPEG encoding through TurboJPEG.
- Added PiSP `COMP1` Bayer decompression into 16-bit samples.
- Added DNG encoding with IMX477 CFA order, black and white levels, exposure,
  gain, colour gains, colour matrix, crop, and active-area metadata.
- Kept profile validation and the Master Archive/4K/2K RAW policy at the shared
  request boundary.

No `rpicam-*` executable is invoked by the current source.

### 6. Safe capture publication

- Test shots are encoded in memory and returned directly to the HTTP client.
- Persistent captures are encoded before publication to the Phase 6 RAM stage.
- JPEG and optional DNG files are first written as hidden `.part` files, assigned
  mode `0640`, and atomically renamed to `optic-web-<profile>-*` names.
- Failure cleanup removes partial and already-renamed members of an incomplete
  capture set.
- The existing independent transfer timer remains responsible for moving final
  files from `/mnt/capture` to the iMac.

### 7. Native build and deployment path

- Added Linux-only `libcamera 0.7`, `tiff 0.11`, and `turbojpeg-sys 0.2.3`
  dependencies.
- Added the `libcamera_probe` qualification binary.
- Built against a user-owned AArch64 sysroot because system-wide development
  packages could not be installed.
- Pinned the validated Pi runtime/build libraries, including Raspberry Pi
  `libcamera 0.7.2+rpt20260817-1` and TurboJPEG `2.1.5`.
- Updated the systemd user service to resolve staged native libraries through
  `LD_LIBRARY_PATH` while retaining its filesystem, privilege, task, and memory
  restrictions.
- Added rollback-protected source upload, Pi-native build, installation, health,
  version, asset, and journal checks.

## Resulting Architecture

```text
Browser / HTTP handlers
        |
        v
bounded optic_camera FIFO actor
        |
        v
dedicated optic-libcamera worker
        |
        +--> persistent CameraManager + acquired IMX477
        +--> configured streams + mmap buffers
        +--> reusable libcamera requests + typed controls
        |
        +--> YUV420 -> TurboJPEG -> MJPEG / test shot / capture JPEG
        +--> PiSP COMP1 -> Bayer16 -> DNG
        |
        `--> .part files -> atomic rename -> /mnt/capture
```

## Validation Record

### Native qualification harness

The isolated `libcamera_probe` run on the Pi confirmed:

- `libcamera v0.7.2+rpt20260817` detected exactly one `imx477` through the
  Raspberry Pi PiSP pipeline.
- The camera reported a `4056 × 3040` active pixel array.
- Profile stream configurations were accepted by the hardware.
- Four reusable requests completed twelve 2K-binning frames:
  `REUSE PASS profile=binning_2k completed_frames=12 requests=4`.
- A full-resolution sequence produced `18,677,760` YUV bytes and `12,451,840`
  RAW bytes.
- The generated `4056 × 3040` JPEG was `4,104,816` bytes.
- The generated DNG was `24,661,360` bytes and contained the expected CFA,
  16-bit sample, black-level, exposure, ISO, colour, crop, and active-area tags.
- The harness ended with `NATIVE LIBCAMERA PROBE PASS`.

The qualification log is retained on the Pi at
`/tmp/optic-native-capture.log`; the DNG inspection is retained at
`/tmp/optic-tiffinfo.log` for as long as `/tmp` persists.

### Integrated native-source validation

The implementation-era validation record reports:

- Formatting passed.
- Linux compilation passed.
- `18` tests passed.
- Clippy passed with warnings denied.
- Live preview start, reconfiguration, and stop passed on the IMX477.
- JPEG test shot, JPEG-only capture, and JPEG-plus-DNG capture passed.
- Final capture permissions were `0640`.
- Partial-file cleanup passed.

The native files are already present in the retained Pi `0.1.1` source snapshot,
timestamped before the `0.1.2` and `0.1.3` snapshots. The exact first deployment
of that source is not recoverable from the available journal or local Git history.

### Confirmed deployment

The guarded `0.1.3` deployment is the first deployment for which the retained log
provides complete confirmation. It passed:

- `cargo fmt --all -- --check`.
- `cargo test --locked --all-targets`: `4` probe tests and `17` daemon tests,
  with no failures.
- `cargo clippy --locked --all-targets -- -D warnings`.
- Optimized AArch64 release build.
- Dynamic-library resolution for `libcamera.so.0.7` and `libturbojpeg.so.0` with
  no unresolved dependencies.
- Rollback-protected service installation and restart.
- Health and version checks at `http://127.0.0.1:8000`.

The currently running API reports version `0.1.3`, camera detection true, backend
`native_libcamera`, and the IMX477 device path. This confirms that the native
backend—not the old command-line backend—is deployed at
[http://optic.local:8000/](http://optic.local:8000/).

## Files Involved

- `/Users/admin/Documents/projects/optic/src/camera.rs`
  - Validated settings, profile specifications, and request/result models.
- `/Users/admin/Documents/projects/optic/src/optic_camera.rs`
  - Bounded FIFO actor and the sole camera-facing application API.
- `/Users/admin/Documents/projects/optic/src/native_camera.rs`
  - Persistent IMX477 owner, stream configuration, requests, controls, capture,
    and atomic publication.
- `/Users/admin/Documents/projects/optic/src/native_codec.rs`
  - TurboJPEG encoding, PiSP RAW decompression, and DNG serialization.
- `/Users/admin/Documents/projects/optic/src/bin/libcamera_probe.rs`
  - Isolated hardware and output qualification harness.
- `/Users/admin/Documents/projects/optic/src/main.rs`
  - Native actor startup, probe, web state, and graceful shutdown.
- `/Users/admin/Documents/projects/optic/src/web.rs`
  - HTTP endpoints backed by `OpticCamera` rather than child processes.
- `/Users/admin/Documents/projects/optic/Cargo.toml`
  - Linux-native camera and codec dependencies.
- `/Users/admin/Documents/projects/optic/scripts/build-deploy-optic-daemon.sh`
  - Pi-native build, validation, deployment, rollback, and runtime checks.
- `/Users/admin/Documents/projects/optic/scripts/setup-optic-daemon-phase-01.sh`
  - Service installation and version readiness check.
- `/Users/admin/Documents/projects/optic/systemd/optic-daemon.service`
  - Native-library path and hardened runtime policy.
- `/Users/admin/Documents/projects/optic/optic-daemon-camera.md`
  - Original native-pipeline rationale, design targets, and implementation notes.

## Known Limitations and Follow-Up

1. **Still capture remains a serialized transition.** The deployed backend has a
   single persistent camera owner, but test shots and staged captures stop the
   preview configuration, run a still configuration, and rely on the dashboard
   to restore preview. The proposal's fully concurrent preview-plus-still
   topology is not implemented.
2. **Historical CLI source is unavailable.** The worklog can verify the legacy
   behavior and the native result, but it cannot provide a line-by-line diff from
   the removed command backend.
3. **Long-duration transition soak results are not preserved as a quantitative
   dataset.** Functional transition checks passed, but no formal memory/latency
   time series was archived for this refactor.
4. **Control responsiveness is a separate phase.** Per-request revision tracking,
   MJPEG metadata, and browser-painted latency measurement were added afterward
   and are documented in
   `/Users/admin/Documents/projects/optic/worklogs/2026-09-16-preview-control-responsiveness.md`.

## Where We Left Off

**Done:** The command-line camera path has been replaced by the native
`optic_camera`/`NativeCameraBackend` implementation, qualified against the real
IMX477, and confirmed deployed in `0.1.3`.  
**In progress:** Real browser/camera responsiveness baselines, covered by the next
worklog.  
**Blocked:** Nothing.

**Next steps:**

1. Complete the EV and white-balance responsiveness measurements recorded in the
   preview-control worklog.
2. If uninterrupted still capture becomes a requirement, implement and qualify a
   true concurrent preview-plus-still pipeline instead of the current serialized
   transition.
3. Preserve future deployment and soak logs outside `/tmp` so release boundaries
   and long-running measurements remain auditable.