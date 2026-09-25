# Dated Worklog: 2026-09-23 - Preview Blackout During Scheduled Captures

Status: **implemented, compiled and tested on the Pi (Linux gates), deployed
(branch build, still 0.1.31), and hardware-validated for B1/B4/B5; B3 not
compared; not user-accepted.** The test plan was written before any `src/` edit.

## Objective

User: "preview frame rate should be consistent." At night, with a scheduled
capture every minute, the live preview is live for only about half of each minute.
Shorten the blackout that each scheduled capture causes, without changing what
the capture records.

Context: found while verifying the Scheduled exposure preview fix
(`worklogs/2026-09-23-highlight-guard.md`, Part 3). Built on branch
`highlight-guard` (PR #18) at the user's request ("commit + push, then fix
frame rate").

## Evidence (Pi, 2026-09-23 23:53–23:56 PDT, read-only journal and a stream recording)

A scheduled capture runs `stop_existing_preview`, starts a still pipeline, and
waits `request_count (3) + CAPTURE_WARMUP_FRAMES (8)` = 11 frames. Then
the preview restarts. Per-frame `warmup_frame` trace of the 23:54 capture
(planned 3,440,629 µs × 1.0):

| Frame | exposure_us | gain | frame_duration_us | colour gains |
|---|---|---|---|---|
| 1–3 | 118,745 (the preview's) | 12.19 | 124,990 | 3.24015 → 3.24054 |
| 4 | 118,745 (the preview's) | 12.19 | 1,737,437 | 3.24072 |
| 5–11 | 3,474,267 (the still's) | 1.0 | 3,474,875 | 3.24089 → 3.23804 |

- New controls take effect from frame 5; frame 4 is transitional. This matches
  `docs/optic-daemon-capture-performance.md` §7 ("new request controls apply
  from frame 4") and its open follow-up: "pass the still limits to
  `camera.start()`".
- Colour gains move < 0.1% across all 11 frames (AWB preset Daylight). The
  captures at 23:55 and 23:56 look the same.
- `warmup_and_capture` took 25,072 ms, 22,758 ms and 10,952 ms (night
  frames of 3.47 s, 3.13 s and 1.46 s). Frames 6–11 are copies of frame 5 that
  cost 6 × the exposure: about 21 s at 3.5 s.
- After the capture, the preview restarts, and its first frames carry the still's
  exposure (the recording: 2 frames at 3,474,267 µs × 1.0, sequence 0–1), so there
  are about 7 more seconds at about 0.3 fps.
- An exposure change on a running preview is controls-only
  (`ReconfigureStream`: no pipeline restart). The blackout comes only from
  captures.

## Design

1. **A manual still captures the first frame that really has the requested
   exposure.** When the still request has a manual shutter and gain (both > 0,
   as every ramped frame does), the capture loop takes the first completed frame
   whose metadata `ExposureTime` and `AnalogueGain` are within 5% of the
   request (the sensor quantises: 3,440,629 requested gave 3,474,267, +1.0%)
   and whose colour gains are within 1% of the previous frame's (AWB settled;
   covers AWB Auto). If no frame qualifies by today's frame 11, it takes frame
   11 as today, and warns. Stills with auto shutter or gain keep today's
   11 frames.
2. **Pipelines start with their own controls.** `start_pipeline` passes the
   same controls it puts on each request to `camera.start(Some(&controls))`.
   The first frames after a capture should then use the preview's exposure
   (removing the slow tail), and a manual still's exposure should apply from frame 1.
   Whether libcamera/`rpi::pisp` honours start controls from frame 1 is
   exactly what the on-Pi test measures. Design 1 does not depend on it: it
   reads the metadata of each frame.

Expected at a night frame of about 3.5 s: capture 25 s → about 4 s (frames 1–4 at preview
pacing, about 0.6 s, plus frame 5) without design 2; about 3.5 s if start controls
apply from frame 1. Preview tail: about 7 s → about 0.

## Acceptance criteria

B1. A manual still is captured from a frame whose metadata exposure and gain match the
    request within 5% and whose colour gains are within 1% of the previous
    frame's; never from a frame at the preview's exposure (frames 1–4 above).
B2. If nothing matches, it falls back to today's frame 11, with a warning.
    Auto-exposure stills are unchanged (11 frames).
B3. The recorded frame is unchanged in content: the sidecar `exposure` (actual
    exposure/gain) matches the request as before, and the meter luminance and clipped
    fraction of an early-captured frame match a same-scene frame-11 capture within
    noise (on the Pi).
B4. Start controls: a pipeline starts with the same controls as its requests.
    Measured on the Pi: which frame first carries the new exposure, for preview
    and still.
B5. On the Pi at night: `warmup_and_capture` ≪ 22–25 s, preview frames resume
    at the preview exposure immediately after a capture, and a 60 s stream
    recording shows the preview live for most of each minute.
B6. The timelapse is unaffected: captures stay once per minute and the ramp and guard
    keep working (sidecar exposure, guard log).
B7. fmt, test, clippy (Mac and Linux/aarch64), Node and Biome pass.

## Test plan (written before implementation)

- Rust unit tests (platform-neutral pure logic, so they run on the Mac): a
  `WarmupPlan`/`frame_is_capturable` decision function.
  - Manual request 3,440,629 µs × 1.0 against the measured frames 1–11 above
    captures frame 5.
  - The frame-4 transitional case is rejected (exposure still the preview's).
  - Colour gains jumping > 1% delays capture by one frame.
  - Quantisation within 5% is accepted; 6% off is rejected.
  - With no match, it falls back at frame 11.
  - Auto shutter or gain uses the fixed 11-frame target.
- Linux/aarch64 compile and clippy of `native_camera.rs` (cfg linux) in the
  vmpi build VM, or by the Pi deploy script's gates.
- On the Pi at night (needs deploy approval): the journal `warmup_frame` trace for
  B1/B4, a 60 s preview recording for B5, sidecars and the guard log for B3/B6.

## Implementation (2026-09-24)

- `src/camera.rs`: `WarmupFrame`, `capture_this_frame` (5% exposure and gain
  tolerance, 1% colour-gain settle, fallback at `STILL_CAPTURE_FRAMES`), and
  4 tests with the measured 23:54 warm-up as the fixture.
- `src/native_camera.rs` (Linux-only): the capture loop builds a `WarmupFrame` from
  each completed request's metadata, keeps a frame when `capture_this_frame`
  says so or at `target`, **breaks** after keeping it (without the break a later frame
  could replace an early keep; found in review before any run), and warns
  when a manual still falls back. `apply_controls` is split into
  `set_controls(&mut ControlList, …)`, and `start_pipeline` passes those
  controls to `camera.start(Some(&start_controls))`.
- Docs: `docs/optic-daemon-capture-performance.md` §8 and
  `docs/optic-daemon-exposure-ramping.md` §9.

A test fixture mistake on the first run: the "AWB settling" case put frame 7's gains
back at the original value (a 4.4% jump back), so the function correctly kept frame
8. The fixture now settles at the new value, and frame 7 is kept.

## Validation so far

| Check | Result |
|---|---|
| `cargo fmt --check` | pass |
| `cargo test --locked` (Mac) | pass: 268 |
| `cargo clippy --locked --all-targets -- -D warnings` (Mac) | pass; **the Linux-only `native_camera.rs` is not compiled on the Mac** |
| Linux/aarch64 compile | The vmpi build VM was not usable: it doesn't mount this worktree and has no Raspberry Pi libcamera arm64 headers. Compiled instead by the deploy script's on-Pi gates (below): **pass** |
| Hardware | see "On the Pi" |

## On the Pi (user-approved "deploy now + measure")

**Deploy** (2026-09-24 00:26:50–00:30:01 PDT): `scripts/build-deploy-optic-daemon.sh`,
exit 0. On-Pi gates: Biome, fmt, `cargo test --locked --all-targets` (263 in
the main binary), strict Clippy (this is the first Linux compile of the
`native_camera.rs` change), release build, `SUCCESS`.

**Captures after the deploy** (journal `warmup_frame` trace, night):

| Capture | Exposure (frame 1 metadata) | Kept | `warmup_and_capture` |
|---|---|---|---|
| Before (2026-09-23 23:54) | frames 1–4 at the preview's, 5–11 at 3,474,267 µs | 11 | 25,072 ms |
| #1 | 4,216,199 µs × 1.0 from **frame 1** | 2 | 12,775 ms |
| #2 | 4,226,589 µs × 1.25 from **frame 1** | 2 | 12,817 ms |
| #3 | 3,049,580 µs × 1.0 from **frame 1** | 2 | 6,230 ms |

- B4: start controls apply from frame 1 for still pipelines (no more
  preview-paced frames 1–4).
- B1: the first frame at the requested exposure (±5%) with settled white balance is kept;
  here frame 2, since frame 1 has no previous frame to compare white balance with.
  Colour gains moved ≤ 0.03% between frames 1 and 2.
- Frame 1 took about 2× its exposure in captures #1 and #2 (since_previous 8,535 and
  8,557 ms at 4.2 s) but not in #3 (3,153 ms at 3.05 s): the sensor's first
  frame after start. Possible further gain: allow frame 1 when white balance
  can be compared against the preview's last gains. Not done.
- The guard kept working across the change (after the restart reset: pull
  1.0 EV at 4.6% clipped, then 1.0 EV at 2.3%).

**Preview** (2026-09-24 20:10 PDT, dusk, user's visible dashboard with the preview on,
70 s recording as a read-only viewer, plus the journal):

- 486 frames in 70 s (≈ 7 fps at 8 fps nominal), with one gap for the scheduled
  capture: preview stopped at 20:10:00.389, capture done at 02.801 (frame 2
  kept, 2,331 ms), preview restarted at 04.423, so a **4.0 s** gap.
- B5: the restarted stream's first frames (sequence 0…) ran at the preview's
  exposure (118,745 µs × 4.90), not the still's. The slow tail seen before
  (2 frames at 3,474,267 µs) is gone.
- About 1.6 s of the gap is after the capture (JPEG encode/publish and pipeline start),
  not exposure.

**Not done:**
- B3: an A/B comparison of an early-kept frame against a same-scene frame-11 capture
  (content equivalence). The kept frames' metadata matches the request, and white
  balance is settled by construction, but no side-by-side image comparison was
  made.
- B6: an overnight check of timelapse continuity with this build (only a
  few captures observed).
- At a 1-minute interval with a night frame of about 4 s, the blackout is still about 13 s per
  capture (2 × E plus the first-frame overhead). That is inherent to stopping the
  preview for the still; a combined preview-and-still configuration would be
  needed to remove it.

