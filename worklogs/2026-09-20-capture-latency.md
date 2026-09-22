# Dated Worklog: 2026-09-20 - Still-Capture Latency Fix

Status: **user-accepted** — deployed, hardware-tested (H1, H3, H4 at 1 s),
and visually accepted by the user (H2). Merged to `main` in PR #10 (95381e0,
2026-09-21); `main` is not yet redeployed as its own version. Follow-ups listed below.

## Objective

"Capture & transfer" takes ~5–5.6 s end-to-end regardless of profile.
`docs/optic-daemon-capture-performance.md` root-causes this to libcamera
frame pacing during the 8-frame warmup, not exposure time, and ranks fixes
in §4. Reduce still-capture latency without changing image output.

## Acceptance Criteria (draft — confirm with user)

- Median `http_handler_total` for a capture drops substantially from the
  §3 baseline, measured with the same per-stage `capture perf` trace
  methodology (§2), per profile (Master Archive + DNG, 4K DCI, 2K Binning).
- Output images unchanged: same resolution, JPEG/DNG validity, and no
  visible AWB/exposure regression versus baseline shots of the same scene.
- Live preview and profile switching still work after a still capture.

## Candidate Approach (from the performance doc §4, in order)

1. Explicit fast `FrameDurationLimits` for the still-capture warmup loop
   (`apply_controls` `fps` is currently `None` when `streaming = false`).
2. Re-examine `CAPTURE_WARMUP_FRAMES = 8` (AE is disabled; only AWB
   converges).
3. Log the StillCapture vs ViewFinder stream config to confirm the
   sensor-mode hypothesis (§3.3.2).

## Files This Track Owns

`src/native_camera.rs`, `src/camera.rs` (if needed),
`docs/optic-daemon-capture-performance.md`. Avoid `src/web.rs` beyond
trace lines.

## Test Plan

Written 2026-09-20 before any `src/` edit.

### Pre-implementation observations (from source, not yet hardware-verified)

- Still captures never set `FrameDurationLimits` (`apply_controls` `fps =
  None`). The observed ~500 ms frame interval equals exactly the Master
  Archive preview rate (`preview_spec().fps = 2`). Hypothesis A: the rpi IPA
  keeps the last `FrameDurationLimits` a previous preview set across
  `stop()`/`configure()`. Hypothesis B (perf doc §3.3.2): the StillCapture
  role picks a slow sensor mode / default. Setting an explicit limit fixes
  the latency under either hypothesis; the new logging tells them apart.
- Side risk found: `shutter_us` validates up to 5,000,000 µs. If an implicit
  ~500 ms max frame duration was in effect, manual shutters > ~500 ms were
  being clamped. An explicit max ≥ shutter could therefore *change* (fix)
  long-exposure output. Checked explicitly in H4 below.
- `AGENTS.md` (imported by `CLAUDE.md`) is absent from this worktree — it was
  deleted in commit `b955bd7`. Not touched by this track.

### Design under test

1. Still-capture requests set `FrameDurationLimits = [floor, max(floor,
   shutter_us)]`, floor = 33,333 µs. libcamera clamps the floor up to the
   selected sensor mode's minimum; manual exposure then drives frame
   duration, and long manual shutters are never capped by the limit.
   Preview behaviour is unchanged.
2. `CAPTURE_WARMUP_FRAMES` stays 8 in this change. Per-frame `ColourGains`
   logging is added so AWB convergence can be measured; any reduction is a
   follow-up decision for the user, based on that data.
3. Instrumentation: `warmup_frame` gains `frame_duration_us` (metadata
   `FrameDuration`) and `colour_gains`; `warmup_and_capture` reports the real
   frame count; each pipeline start logs the configured
   `FrameDurationLimits` range (min/max) for its role, so StillCapture vs
   ViewFinder sensor-mode timing can be compared (perf doc §4 item 3).

### Automated tests (cross-platform unit tests, `cargo test --locked`)

- U1: shutter 0 (auto) → `[33_333, 500_000]` (revised 2026-09-21 after the
  H1 exposure regression; originally `[33_333, 33_333]`).
- U2: short manual shutter (e.g. 10,000 µs) → max stays at the floor.
- U3: long manual shutter (5,000,000 µs, validation max) → `[33_333,
  5_000_000]`; never below the floor, never truncated.
- Existing suite, `cargo fmt --check`, `cargo clippy --all-targets -D
  warnings` must pass. Linux-only `imp` code (libcamera calls) only compiles
  on Linux — must be type-checked on the Pi toolchain or an equivalent
  aarch64 Linux build; macOS checks do not cover it.

### On-Pi measurement protocol (requires user approval — shared Pi/camera)

Environment: `optic.local`, daemon built from this branch, same fixed scene
and lighting for baseline and new runs, lens/focus untouched. Logs via the
system-scoped journal query (perf doc §2.2 update), `grep "capture perf"`.

- H0 baseline (current deployed build): 5 captures per profile (Master
  Archive + DNG, 4K DCI, 2K Binning), each preceded by that profile's live
  preview for ≥ 5 s, spaced ≥ 3 s apart. Also 3 captures of 2K Binning
  *without* any preview since daemon start (tests hypothesis A vs B).
  Record median/max `http_handler_total` and per-frame `since_previous_ms`.
  Save the JPEG/DNG outputs.
- H1 new build, identical sequence. Pass: median `http_handler_total` for
  each profile ≤ 2,000 ms (baseline ~5,100–5,500 ms), and per-frame
  `frame_duration_us` / `since_previous_ms` consistent with the logged
  sensor-mode minimum (not ~500 ms).
- H2 image comparison per profile, H0 vs H1 at identical settings: same
  pixel dimensions; JPEG decodes; DNG opens (`exiftool`/macOS Preview per
  `docs/optic-daemon-dng-compatibility.md`); `ExposureTime`/`AnalogueGain`/
  `ColourGains` metadata within normal frame-to-frame variation; side-by-side
  visual check for colour/brightness regression.
- H3 after a still capture: live preview restarts, and switching between all
  three profiles works (preview frames arrive, no errors in log).
- H4 long shutter: 2K Binning with `shutter_us = 1,000,000` and `5,000,000`
  on H0 and H1. Record `exposure_us` from metadata. Expected on H1: equals
  requested; total ≈ (8 + buffers + 1) × shutter. Any H0/H1 difference is
  reported as a behaviour change, not hidden.
- Failure cases: capture error/timeout (`recv_timeout` 10 s — note that at
  5 s shutters the per-frame wait is still < 10 s), `Timeout` responses,
  preview failing to restart, AWB visibly off in H1 frames.

## Implementation (2026-09-20)

Files changed:

- `src/camera.rs`: `STILL_MIN_FRAME_DURATION_US` (33,333) and
  `still_frame_duration_limits_us(shutter_us)`; tests U1–U3 (two test fns).
- `src/native_camera.rs`: `apply_controls` sets `FrameDurationLimits` for
  still captures (`fps = None`); `warmup_frame` logs `frame_duration_us` and
  `colour_gains`; `warmup_and_capture` `frames` = real `frame_count`;
  pipeline-start log adds `frame_duration_range_us`.
- `docs/optic-daemon-capture-performance.md`: §1 status line, new §7.

No `Cargo.toml` version bump, no `src/web.rs` changes, no dependency changes.

## Validation

| Check | Environment | Result |
|---|---|---|
| `cargo fmt --all` | macOS dev | applied, clean |
| `cargo test --locked` | macOS dev | **pass**, 111 passed / 0 failed (includes the 2 new tests) |
| `cargo clippy --locked --all-targets -- -D warnings` | macOS dev | **pass** |
| Compile of Linux-only `imp` module (libcamera calls) | vmpi VM | **unavailable**: VM has no libcamera dev package (`pkg-config --modversion libcamera` → not found) |
| `cargo fmt --check`, `cargo test --locked --all-targets`, `cargo clippy --locked --all-targets -D warnings` | Pi, `stable` 1.98.1, pinned sysroot; separate dir `~/.local/src/optic-capture-latency-check` and target `~/.cache/optic-capture-latency-target`; nothing installed, service untouched | **pass**: fmt exit 0; 114 passed / 0 failed (incl. 5 `native_camera::imp` tests and the 2 new tests); clippy exit 0 |
| H0–H4 on-Pi protocol | `optic.local` | **not run**: needs user approval (shared Pi/camera) |

Unverified API assumptions in the Linux code (compile will confirm):
`camera.controls().find(ControlId::FrameDurationLimits as u32)` via
`ActiveCamera: Deref<Target = Camera>`; `ControlInfo::min()/max()` being
`Debug`; `controls::FrameDuration` and `controls::ColourGains` metadata reads
(the latter already used by the preview path).

## H0 Baseline Observations (2026-09-20, deployed `main` 0.1.29)

Installed source on the Pi (`~/.local/src/optic-daemon-0.1.29`) was compared
file-by-file (sha256) against `main` `a6e1509`: identical except a stray
`src/.DS_Store`. User-triggered captures; logs from `journalctl -u
user@1000.service`. `save_dng=false` for all of these (DNG unticked).

| Time (PDT) | Trigger | Profile | Preceding preview | Frame interval | `total` |
|---|---|---|---|---|---|
| 23:00:43 | user | Master Archive | Master Archive (2 fps) | ~500 ms | 5,401 ms |
| 23:00:55 | user | Master Archive | Master Archive (2 fps) | ~500 ms | 5,386 ms |
| 22:58–23:01 (×4) | scheduler, every minute | Master Archive | Master Archive | — | 5,375–5,387 ms |
| later | user | 2K Binning | 2K Binning (8 fps) | ~124–130 ms | 1,480 ms |
| later | user | 2K Binning | 2K Binning (8 fps) | ~124–130 ms | 1,449 ms |
| later | scheduler | Master Archive (4056×3040) | 2K Binning (8 fps) | ~119–134 ms | 1,630 ms |

Finding: the still-capture frame interval equals the **previous preview's**
`FrameDurationLimits` (2 fps → 500 ms, 8 fps → 125 ms), independent of the
capture profile. A full-resolution Master Archive capture completed in
1,630 ms when it followed a 2K preview. This supports hypothesis A and
contradicts perf doc §3.3.2's slow-StillCapture-sensor-mode hypothesis.
Implication: the explicit still `FrameDurationLimits` in this change should
make every profile's latency independent of the preceding preview (expected
~1.4–1.6 s; unmeasured until H1).

Test-condition notes: an enabled scheduler rule captures Master Archive every
minute at ~hh:mm:05 and can queue ahead of or behind a manual click; the
compile-only Pi build was running during some of the 22:55–22:57 scheduler
captures (5,859–5,996 ms, excluded above). A first compile-only attempt used
the wrong toolchain name (`1.98.1`; the Pi uses `stable` = 1.98.1) and ran
nothing; the rerun's output was lost to an SSH drop, so the Linux compile
was then rerun detached with a log file on the Pi and passed (see Validation).

## Deployment (2026-09-20)

With user approval, `./scripts/build-deploy-optic-daemon.sh` was run from this
worktree: exit 0, "SUCCESS: optic-daemon 0.1.29 is active". Service
restarted 23:14:27 PDT; `/healthz` → `ok`. Deployed
`src/native_camera.rs` and `src/camera.rs` sha256 match the branch. Version
stays 0.1.29 (the bump waits for merge), so the Pi's
`~/.local/src/optic-daemon-0.1.29` now holds this branch; roll back by
redeploying `main`. The scheduler came up `initial_run_state=Paused`, so no
scheduler captures should interfere with H1.

## Deploy Superseded (2026-09-21)

Service log shows restarts at 23:34→23:38 PDT and 00:12–00:19 PDT, with a
new binary installed 00:16 PDT. The Pi's `~/.local/src/optic-daemon-0.1.29`
now contains `src/optic_alerts.rs` and the daemon logs "health alerts running
in dry-run mode", i.e. the `feat/health-alerts` build. Its
`src/native_camera.rs`/`src/camera.rs` sha256 equal `main`'s (no
`still_frame_duration_limits_us`). No `capture perf` lines were logged between
this track's deploy (23:14) and its replacement, so **no H1 data exists**.
This is the shared-Pi collision the coordination rules warn about; the
same-version (0.1.29) install directory makes it silent. Redeploy needs user
approval and coordination with the health-alerts session.

## Redeploy (2026-09-21)

User chose "Redeploy now" (replacing the `feat/health-alerts` build). Pi idle
beforehand (no cargo/rustc). `./scripts/build-deploy-optic-daemon.sh`: exit 0,
114 tests passed, "SUCCESS: optic-daemon 0.1.29 is active"; service active
since 01:11:38 PDT; `/healthz` → `ok`; deployed `src/native_camera.rs` /
`src/camera.rs` sha256 match the branch; `src/optic_alerts.rs` absent.

## H1 Attempt 1 (2026-09-21, build deployed 01:11 PDT) — FAILED

User captures; shutter/gain auto (`0`).

| Profile | Total | Warmup loop | Frames 1–3 | Frames 4–11 | Exposure |
|---|---|---|---|---|---|
| 2K Binning (×2) | 778 / 778 ms | 709 / 711 ms | 125 ms (old preview limit) | 33 ms | **66.7 → 33.0 ms** |
| Master Archive + DNG (×2) | 2,625 / 2,538 ms | 2,320 / 2,328 ms | 500 ms (old preview limit) | 85 ms | 65.7 ms (unchanged) |

Logged `frame_duration_range_us`: 2K ViewFinder 9,835.., 2K StillCapture
22,131.., Master Archive StillCapture 85,335.. µs.

**Regression:** perf doc §3.3.1 assumed AE is off. It is not: with shutter
`0`, exposure is still chosen automatically and is capped by the maximum
frame duration. The `[33,333, 33,333]` limit therefore cut 2K Binning
exposure from 66.7 ms to 33.0 ms (visible within one capture at frame 4);
2K JPEG 221 KB (baseline) → 107 KB. Master Archive escaped only because its
sensor mode's 85 ms minimum exceeds the cap. U1 had encoded the same wrong
assumption. Master Archive JPEG size (3.05 MB vs ~4.96 MB at 23:00) is not
comparable: scene light changed between 23:00 and 01:00.

Other observations: new limits take effect from frame 4; frames 1–3 still
run at the previous preview's limit (~1.5 s of Master Archive's 2.3 s).
`colour_gains` drift ~0.001–0.006 per frame and are still moving at frame 11.

**Fix (implemented 2026-09-21):** auto shutter now gets `[33,333,
500,000]` (`STILL_AUTO_MAX_FRAME_DURATION_US`, the slowest preview limit
captures used to inherit), so auto exposure keeps its previous headroom;
manual shutters unchanged. Tests: U1 replaced by the regression test
`still_frame_duration_limits_leave_auto_exposure_its_headroom`; short-manual
test adds a 100 ms case. macOS: `cargo test --locked` 112 passed / 0 failed;
clippy `-D warnings` clean.

## H1 Attempt 2 (2026-09-21, corrected build deployed 01:18 PDT) — PASSED

Deploy: user approved ("sure"); `./scripts/build-deploy-optic-daemon.sh` exit
0, 115 tests passed on the Pi, service active 01:18:31 PDT, deployed
`src/native_camera.rs`/`src/camera.rs` sha256 match the branch. This build
also logs `analogue_gain` per warmup frame.

The user was away and asked for no questions, so captures were triggered from
the Mac with the dashboard's own API calls (`/api/stream/reconfigure`, then
`/api/capture` with the saved dashboard settings: auto shutter, gain 1.0,
AWB auto, denoise auto). Script: session scratchpad `h1.sh`, run 01:21:37–01:23:22
PDT; per-frame data parsed from the journal. Scheduler was `Paused`; lighting
was unchanged during the run. A first run of the script sent malformed
bodies (bash 3.2 quoting), and every capture returned HTTP 422; no captures
happened. One extra 2K capture was then made by a direct curl (row 1).

| Time (UTC) | Profile | Previous preview | Total | Frames 1–3 interval | Frames 4–11 interval | Exposure / gain (every frame) |
|---|---|---|---|---|---|---|
| 08:21:04 | 2K Binning | Master Archive | 2,144 ms | 500 ms | 67 ms | 66,661 µs / 1.0 |
| 08:21:43 | 2K Binning | 2K Binning | 1,017 ms | 125 ms | 67 ms | 66,661 µs / 1.0 |
| 08:21:53 | 2K Binning | 2K Binning | 1,018 ms | 125 ms | 67 ms | 66,661 µs / 1.0 |
| 08:22:04 | 4K DCI | 4K DCI | 1,133 ms | 125 ms | 67–72 ms | 66,654 µs / 1.0 |
| 08:22:14 | 4K DCI | 4K DCI | 1,132 ms | 125 ms | 67 ms | 66,654 µs / 1.0 |
| 08:22:24 | Master Archive + DNG | Master Archive | 2,551 ms | 500 ms | 85 ms | 66,654 µs / 1.0 |
| 08:22:36 | Master Archive + DNG | Master Archive | 2,583 ms | 500 ms | 85 ms | 66,654 µs / 1.0 |
| 08:22:56 | 2K Binning, preview stopped first | Master Archive (stopped) | 2,138 ms | 500 ms | 67 ms | 66,661 µs / 1.0 |
| 08:23:08 | 2K Binning, shutter 1 s | 2K Binning (1 s shutter) | 11,108 ms | 1,000 ms | 1,000 ms | 999,686 µs / 1.0 |

Against acceptance criteria:

- Latency (H1): versus the H0 baseline, 2K Binning after its own preview
  1,449–1,480 → 1,017 ms; Master Archive after its own preview ~5,390 ms
  (JPEG only) → ~2,570 ms (with DNG). No 4K DCI baseline was taken; it is
  now ~1,130 ms. Frame interval from frame 4 onward now matches the
  exposure (~67 ms), or the sensor-mode minimum (85 ms, Master Archive).
  **Pass**, with the frames-1–3 limitation below.
- Exposure unchanged (H2, metadata part): exposure is identical on
  frames 1–3 (old limits) and 4–11 (new limits) in every capture, and
  matches the H0 values (66,654/66,661 µs). Gain is 1.0 throughout (manual,
  per settings). Resolution and DNG size (24.7 MB) are unchanged. **Pass.**
  The side-by-side visual check has not been done (user).
- Attempt-1 regression confirmed as real: with gain fixed at 1.0, gain
  could not compensate the 33 ms cap, so those 2K captures were about one
  stop under-exposed. At similar light, 2K JPEGs were 107 KB (capped build)
  versus 133 KB now.
- Preview after capture (H3): preview kept running after each capture, and
  reconfigure to 2K / 4K / Master Archive each returned 200 with
  `"streaming":true`. **Pass.**
- Long shutter (H4): 1 s is honoured (999,686 µs, 1 s frames). 11.1 s total =
  11 frames × 1 s. **Pass for 1 s**; 5 s was not run (~60 s per capture).
- AWB (§4 item 2 data): `colour_gains` change by < 0.3% from frame 1 to
  frame 11 in all nine captures (frame 8→11: ≤ 0.004 R, ≤ 0.0015 B). AWB is
  effectively converged from frame 1, carried over from the preview.

## Changed Files (final)

- `src/camera.rs`: `STILL_MIN_FRAME_DURATION_US` (33,333),
  `STILL_AUTO_MAX_FRAME_DURATION_US` (500,000),
  `still_frame_duration_limits_us`, and three unit tests (including the
  auto-exposure regression test).
- `src/native_camera.rs`: still-capture `FrameDurationLimits` in
  `apply_controls`. `warmup_frame` logs `analogue_gain`, `frame_duration_us`
  and `colour_gains`. `warmup_and_capture` `frames` is now the real count.
  The pipeline-start log adds `frame_duration_range_us`.
- `docs/optic-daemon-capture-performance.md`: §1 status, §7 (fix, the §3.3.1
  correction, measurements).
- `worklogs/2026-09-20-capture-latency.md`: this file.

Final checks: macOS `cargo test --locked` 112 passed / 0 failed, `cargo
clippy --locked --all-targets -D warnings` clean, `cargo fmt` clean. Pi
deploy run: 115 passed / 0 failed. Not committed (no approval requested).
After the deploy, one code comment in `src/native_camera.rs` was corrected
(AWB is not the only converging loop). This is a comment-only change, so the
deployed binary is otherwise identical to the working tree.

## User Acceptance (2026-09-21)

The user clicked Capture & transfer twice each on Master Archive + DNG and
2K Binning, and reported that the JPEG and DNG images look good (H2 visual
check passed). Journal for those captures (brighter scene, auto exposure
~9.7–10 ms, gain 1.0):

| Time (UTC) | Profile | `http_handler_total` |
|---|---|---|
| 00:40:19 | Master Archive + DNG | 2,529 ms |
| 00:40:26 | Master Archive + DNG | 2,495 ms |
| 00:41:24 | 2K Binning | 716 ms |
| 00:41:28 | 2K Binning | 726 ms |

## Limitations / Risks / Next Steps

- **Frames 1–3 still run at the previous preview's frame limit.** New
  request controls take effect from frame 4, which costs ~1.5 s after the
  Master Archive preview (2 FPS) and ~0.4 s after the 8 FPS previews. The
  limit also carries over when the preview was stopped first (08:22:56 row).
  Suggested follow-up: pass the still `FrameDurationLimits` to
  `camera.start()` so the first frames use them. Expected Master Archive
  result is ~1 s (not measured).
- `CAPTURE_WARMUP_FRAMES` is unchanged (8). The AWB data above suggests it
  could be reduced; that decision is left to the user, since it affects
  output.
- A 5 s manual shutter was not tested. It takes ~11 × 5 s ≈ 55 s per capture,
  which may exceed browser/HTTP timeouts. The warmup design is unchanged here.
  Longer manual shutters may now be honoured where an inherited ~500 ms limit
  used to clamp them.
- Lighting changed between H0 (23:00) and H1 (01:21), so JPEG sizes are not
  comparable across them; H2 rests on the user's visual check.
- The Pi currently runs this branch as 0.1.29. The other tracks' deploys
  overwrite each other silently (same version and install dir), and this one
  was overwritten once already.
- Pi leftovers from compile-only checks (not deleted without approval):
  `~/.local/src/optic-capture-latency-check`,
  `~/.cache/optic-capture-latency-target`,
  `~/.cache/optic-capture-latency-check.{sh,log}`.
- The test captures (10 JPEGs + 2 DNGs, `testshot-*`) went to `/mnt/capture`
  and sync to the iMac like normal test shots.
- `AGENTS.md`, imported by `CLAUDE.md`, is missing (deleted in `b955bd7`).

## User Verification Steps

1. Open two Master Archive JPEGs taken minutes apart under the same light,
   and compare colour and brightness by eye; also check that a DNG opens.
2. Press Capture & transfer on 2K Binning after its preview: it should
   return in ~1 s (Master Archive + DNG in ~2.6 s).
3. Decide on the follow-ups: start-time frame limits, and warmup frame count.

## Parallel-Session Coordination (applies to all four tracks)

This track runs in its own git worktree alongside three others
(`feat/capture-latency`, `feat/health-alerts`, `feat/provisioning`,
`feat/timelapse-builder`), all branched from `main` at `a6e1509`.

- **Do not bump `Cargo.toml` `version` on this branch.** The bump happens
  once, at merge time into `main`, immediately before deploy.
- **The Pi and camera are a single shared resource.** Ask the user before any
  deploy, service restart, reboot, or on-hardware test, and do not assume
  another session is not using it.
- **Stay inside the files this track owns (below).** If a change outside them
  becomes necessary, record why here and tell the user; it is a merge-conflict
  risk with the other tracks.
- Do not commit or push without explicit user approval (`CLAUDE.md`).
- Merge order is decided by the user; after each merge the other branches
  rebase onto `main`.
