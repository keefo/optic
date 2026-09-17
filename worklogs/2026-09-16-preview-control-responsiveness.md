# Preview Control Responsiveness Worklog

**Date:** 2026-09-16  
**Release:** `optic-daemon` `0.1.3`  
**Target:** Raspberry Pi 5 with IMX477 HQ Camera at `liam@optic.local`  
**Dashboard:** [http://optic.local:8000/](http://optic.local:8000/)  
**Status:** Baseline collection resumed; implementation and deployment remain complete.
## Objective

Add an end-to-end way to measure how quickly live-preview camera-control changes become visible without changing the existing preview behavior. The dashboard needed to report:

1. **First visible:** elapsed time from browser control input to a painted frame carrying that control revision.
2. **Median:** rolling median of the last ten first-visible measurements.
3. **3A stable:** elapsed time until relevant AE/AWB metadata has changed and remained stable for three painted frames.

The measurement had to identify the exact libcamera request revision represented by each MJPEG frame, rather than assuming that the newest global settings had already reached every queued request.

## Baseline Acceptance Criteria and Test Plan

This plan was recorded before continuing baseline collection.

### Acceptance criteria

- Record at least ten successful EV measurements in the target browser.
- Record at least ten successful white-balance measurements in the target browser.
- For each run, retain the control transition, capture profile, lighting
  conditions, first-visible latency, rolling median, 3A-stability result, and
  browser/device identity.
- Treat `Unavailable`, `No metadata change`, unchanged revisions, stream
  interruption, or a timeout as a failed sample and retain diagnostics rather
  than excluding it silently.
- Confirm the daemon remains healthy, uses `native_libcamera`, detects the
  IMX477, and has no queued camera commands before and after collection.

### Test sequence

1. Use one browser/device for the full baseline and record its versions.
2. Select one capture profile and keep framing and illumination fixed.
3. Let AE/AWB converge before the first sample and between transitions.
4. Alternate EV between two visibly different values until ten successful
   first-visible and 3A-stability results are recorded.
5. Alternate between two visibly different white-balance modes until ten
   successful results are recorded.
6. Record every result displayed by the dashboard; do not estimate values from
   visual observation.
7. On any failed sample, preserve the affected MJPEG metadata and relevant
   service logs before retrying.

### Required environment

- The deployed Raspberry Pi and real IMX477 camera.
- The LAN dashboard at `http://optic.local:8000/`.
- A target browser capable of reporting the dashboard paint measurements.
- Stable scene illumination for the duration of a run.

## Baseline Results

Pending user-operated browser measurements.

### Pre-collection runtime check

Observed on 2026-09-16 at 21:07 PDT from the development Mac:

- `GET /healthz`: `ok`.
- `GET /api/status`: version `0.1.3`, backend `native_libcamera`, camera
  detected, preview streaming, camera not busy, zero queued camera commands,
  and an empty capture stage.

## Work Completed

### 1. Control revision propagation

- Added `control_revision: u64` to `StreamRequest`, with a Serde default for compatibility.
- Added the accepted revision to stream start/reconfigure API responses.
- Added the revision and live camera metadata to `PreviewFrame`.
- The browser increments `controlRevision` for each camera-control input, capture-profile change, and reset-to-defaults action.
- Each stream request carries the revision that existed when the request was sent.
- The native pipeline stores both the latest accepted revision and a revision for every queued libcamera request cookie.
- On request completion, the frame receives the revision that was associated with that request when it was queued. The reused request is then given the newest settings and newest revision before it is queued again.

This per-request bookkeeping prevents in-flight frames from being incorrectly labeled with controls that had not affected them yet.

### 2. In-place control updates

- ISP-only changes update the running pipeline settings and revision without restarting the stream.
- Profile, rotation, and flip changes still require pipeline reconfiguration.
- Tests cover both paths.

### 3. Live IMX477 metadata

The native backend now reads these values from each completed libcamera request:

- AE state: `idle`, `searching`, or `converged`.
- AWB state: `inactive`, `searching`, `converged`, or `locked`.
- Exposure time in microseconds.
- Analogue gain.
- Red and blue colour gains.

Missing values are represented as unavailable rather than fabricated.

### 4. MJPEG frame metadata

Every MJPEG part now includes:

- `X-Optic-Sequence`
- `X-Optic-Control-Revision`
- `X-Optic-AE-State`
- `X-Optic-AWB-State`
- `X-Optic-Exposure-Us`
- `X-Optic-Analogue-Gain`
- `X-Optic-Colour-Gains`

The stream remains `multipart/x-mixed-replace` with no-store caching.

### 5. Browser-side MJPEG rendering and timing

- Replaced opaque direct `<img>` MJPEG consumption with a `fetch()` stream reader so per-frame multipart headers can be inspected.
- Added incremental multipart parsing using each part's `Content-Length`.
- Each JPEG is rendered through a Blob object URL and decoded before timing is recorded.
- Two nested `requestAnimationFrame` callbacks are used so first-visible time represents a browser paint opportunity, not merely network receipt.
- Previous Blob URLs are revoked to avoid accumulating browser memory.
- Abort controllers and stream generations prevent stale readers from updating the current preview.

### 6. Measurement behavior

- Measurement begins at `performance.now()` when the browser receives the control input.
- It includes the existing debounce interval and all request, camera, JPEG, network, decode, and paint time.
- A first-visible sample is accepted when a painted frame has a control revision greater than or equal to the measured revision.
- The rolling median retains at most ten samples.
- AE stability is measured for metering, exposure mode, EV, gain, and shutter changes.
- AWB stability is measured for white-balance changes.
- Reset defaults measures both AE and AWB.
- Profile, rotation, flip, and denoise still produce first-visible timing but do not wait for 3A stability when no relevant 3A metadata is required.
- Relevant metadata must first differ from its pre-change baseline, then remain within tolerance for three painted frames.
- Exposure stability tolerance is the greater of `50 µs` or `0.5%`.
- Analogue-gain and colour-gain tolerance is the greater of `0.005` or `0.5%`.
- The settle timeout is 15 seconds.
- The UI distinguishes unavailable metadata, no observed metadata change, timeout, and request failure.

### 7. Dashboard UI

Added a responsive **Preview responsiveness** panel containing:

- Current first-visible latency.
- Rolling median and sample count.
- Current 3A-stability latency.
- Control label, control revision, and resulting frame sequence.

The panel collapses from three columns to one on narrow screens and uses the dashboard's existing styles and accessibility labels.

### 8. Preserved preview behavior

The instrumentation intentionally left these values unchanged:

- Control reconfigure debounce: `350 ms`.
- Requested preview buffer count: `4`.
- Preview JPEG quality: `95`.
- Master Archive preview: `4056 × 3040` at `2 FPS`.
- 4K DCI preview: `1352 × 720` at `8 FPS`.
- 2K Binning preview: `1014 × 760` at `8 FPS`.

Therefore the measurements describe the existing behavior rather than a simultaneously tuned configuration.

## Files Involved

### Runtime and UI

- `/Users/admin/Documents/projects/optic/src/camera.rs`
  - Stream control revision and enriched preview-frame model.
- `/Users/admin/Documents/projects/optic/src/native_camera.rs`
  - In-place control updates, per-request revision tracking, and live 3A metadata extraction.
- `/Users/admin/Documents/projects/optic/src/web.rs`
  - Revision echoing, MJPEG metadata headers, and metadata tests.
- `/Users/admin/Documents/projects/optic/src/web/app.js`
  - MJPEG parser/renderer, revision matching, paint timing, median, and 3A-stability logic.
- `/Users/admin/Documents/projects/optic/src/web/index.html`
  - Responsiveness measurement panel.
- `/Users/admin/Documents/projects/optic/src/web/styles.css`
  - Desktop and mobile measurement-panel styling.

### Release, deployment, and documentation

- `/Users/admin/Documents/projects/optic/Cargo.toml`
  - Release version set to `0.1.3`.
- `/Users/admin/Documents/projects/optic/Cargo.lock`
  - Package version synchronized to `0.1.3`.
- `/Users/admin/Documents/projects/optic/scripts/build-deploy-optic-daemon.sh`
  - Deployment verifies the measurement code and UI are embedded in the served assets.
- `/Users/admin/Documents/projects/optic/optic-daemon.md`
  - Phase 1 responsibilities now document first-visible, median, and AE/AWB stability reporting.
- `/Users/admin/Documents/projects/optic/optic-daemon-build-environment.md`
  - Validated release source updated to `0.1.3`.
- `/Users/admin/Documents/projects/optic/setup.md`
  - Manual diagnostic deployment path updated to `0.1.3`.

## Tests and Validation

### Raspberry Pi build validation

The guarded `0.1.3` deployment ran on the AArch64 Pi with the pinned native environment and passed:

- `cargo fmt --all -- --check`
- `cargo test --locked --all-targets`
  - `4` probe-binary tests passed.
  - `17` daemon tests passed.
  - `0` failed.
- `cargo clippy --locked --all-targets -- -D warnings`
- Optimized release build.
- Shared-library resolution for staged `libcamera.so.0.7` and `libturbojpeg.so.0` with no unresolved libraries.

The measurement-specific automated coverage includes:

- Embedded responsiveness UI and JavaScript presence.
- Required browser measurement hooks.
- MJPEG control revision and camera metadata headers.
- ISP controls avoiding stream reconfiguration.
- Profile and transform changes requiring stream reconfiguration.

### Deployment and runtime validation

The rollback-protected deployment passed all checks:

- Installed release binary matched the built artifact.
- `optic-daemon.service` was enabled and active after restart.
- `/healthz` responded successfully.
- `/api/status` reported version `0.1.3`.
- Served `/app.js` contained frame-render measurement code and all profile FPS values.
- Served `/` contained the responsiveness panel and first-visible metric.
- No error-priority service journal entries were found during deployment.
- Browser UI checks passed.
- A live IMX477 MJPEG stream was inspected and confirmed to carry control revision plus AE/AWB metadata.

Deployment completed successfully at `http://optic.local:8000/`.

### Documentation-pass limitation

During creation of this worklog, the development Mac's current shell did not have `cargo` on `PATH`, so the Rust suite could not be rerun locally. The authoritative Pi run above already validated the exact deployed `0.1.3` source. This worklog adds Markdown only.

## Current Result

The measurement path is deployed and operational end to end:

