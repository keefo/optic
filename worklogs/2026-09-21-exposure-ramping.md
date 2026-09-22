# Dated Worklog: 2026-09-21 - Exposure Ramping for Scheduled Captures

Status: **implemented and tested (Mac host + Linux/aarch64 compile and
camera-free tests on the Pi); not deployed, not hardware-validated.** The test plan was written before any `src/` edit.

Design: `docs/optic-daemon-exposure-ramping.md`.

## Objective

Smooth day-to-night ("Holy Grail") exposure for scheduled timelapse captures:
meter each captured frame, follow a sun-elevation target, limit per-frame
exposure change, put exposure into shutter before gain, and replace per-frame
AWB with eased white balance.

## Decisions (user, 2026-09-21, via question tool)

- Max shutter: keep the 5 s cap.
- Night look: brightened night (day level to +6°, 2 EV darker by −18°).
- Scope: **global only**, one exposure mode for all scheduled captures. The
  user first leaned towards per-rule ramps, then asked how merged rule
  captures could work and how professional controllers do it. After that
  discussion they chose global only, with no per-rule presets. The original
  task's "per-rule exposure presets" item is therefore dropped. The Scheduler
  page gets a global "Scheduled exposure" card instead.
- WB: eased grey-world, seeded from AWB.
- Metering: trimmed log-mean luma.

## Acceptance Criteria

1. `ScheduleConfig.exposure` defaults to `Dashboard`. Old `config.json`
   files load unchanged, and scheduled captures in `Dashboard` mode build
   exactly the same `CaptureRequest` as before.
2. In `AutoRamp` mode, the first scheduled capture (or the first after a gap
   of more than 30 min) is an auto-exposure/AWB seed. Every later capture
   uses manual shutter, gain and colour gains from the ramp.
3. Consecutive ramped frames differ by at most `max_step_ev` in planned
   exposure (except the first frame after a seed), and colour gains change
   by at most `wb_max_step_pct` per frame.
4. Exposure goes into shutter first and gain only once the shutter is at
   its effective maximum. When brightening, gain drops back to 1 before
   the shutter shortens.
5. The target follows sun elevation: day level at ≥ +6°, day − `night_drop_ev`
   at ≤ −18°, and monotonic in between. Without a station it stays at the
   day level.
6. The shutter never exceeds `min(max_shutter_us, interval budget)`, and the
   5 s validation cap is unchanged.
7. On a synthetic sunset (scene brightness falling about 17 EV over the
   sun-elevation range), the simulated ramp converges without oscillating
   and ends within 0.5 EV of the night target, unless it is limited by
   max shutter × max gain.
8. The metering result is independent of image content ordering, ignores
   the extreme 2% tails, and gives the same answer for a stride larger
   than the width.
9. `POST /api/schedule/preview` rejects out-of-range exposure settings
   (422). The Scheduler page stages and saves the exposure mode with the
   rules, and shows the live ramp status.
10. `cargo fmt --check`, `cargo test`, `cargo clippy --all-targets -D warnings`
    pass on the Mac. Linux-only `native_camera.rs` code compiles on the Pi
    or build VM (only with the user's approval if it runs on the Pi).

## Test Plan (written before implementation)

### Host unit tests (`cargo test`, Mac)

`src/exposure_ramp.rs`:
- `meter_*`: a uniform grey frame measures its own linear luminance; a frame
  with 1% pure-white and 1% pure-black pixels measures the same as without
  them (trim); padded stride equals the unpadded result; a colour-cast frame
  gives grey-world ratios > 1 in the right channel; an all-black frame has
  no usable grey-world estimate.
- `target_bias_ev`: the values at +10°, +6°, −6°, −18°, −30°, and
  monotonicity across the curve; `None` elevation → day level.
- `split_exposure`: shutter-first, gain-last; the gain ceiling holds; a
  product below the minimum clamps to the minimum shutter at gain 1.
- `plan`/`observe`: no state → seed; stale state (> 30 min) → seed; the first
  frame after a seed is unlimited; later frames are step-limited; a
  simulated sunset converges (criterion 7) with monotonic exposure;
  smoothing damps a single bright outlier frame (a car's headlights);
  WB converges toward neutral with ≤ 3% steps and is held on dark frames.
- `max_shutter_for_gap`: 30 s → about 2.0 s, 1 min → about 4.2 s, no gap →
  no cap, tiny gap → min shutter.
- `RampSettings::validate` rejects each out-of-range field; `sanitized`
  clamps.

`src/camera.rs`: `colour_gains` defaults to `None`; old settings JSON
still deserializes; out-of-range gains are rejected.

`src/optic_scheduler.rs`: `ScheduleConfig` without `exposure` → `Dashboard`;
an `AutoRamp` JSON round trip; the Dashboard request equals the committed
settings (regression for criterion 1); a ramped request carries the plan's
shutter, gain and colour gains.

### Build checks

- `cargo fmt --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`
  on the Mac (non-Linux: `native_camera` compiles to its stub).
- The Linux compile of `native_camera.rs` (metering hook, `ColourGains`
  control) happens in the build VM (`vmpi` skill) if available, otherwise
  unverified. The Pi is used only with the user's approval.

### Browser check

- Serve the page against a local daemon if feasible on the Mac, or check
  the JS syntax with `node --check`. Load the Scheduler page and switch the
  mode; the card fields show and hide; staging sends `exposure`.

### Hardware (only with user approval; not part of this session unless approved)

- Deploy and set `AutoRamp` with a short interval rule. Check that the
  first frame is a seed, then manual frames with `colour_gains` in the
  capture log. Observe a real sunset: no visible flicker; the shutter
  rises to its cap before gain rises.

## Files This Track Owns

`src/camera.rs`, `src/native_camera.rs`, `src/optic_scheduler.rs`,
`src/ephemeris.rs` (read only), `src/exposure_ramp.rs` (new),
`src/web/scheduler.html`, `src/web/scheduler.js`,
`docs/optic-daemon-exposure-ramping.md`, and the changed sections of
`docs/optic-daemon-scheduler.md` and `docs/optic-daemon-camera.md`.

### Planned edits outside the owned files (merge-conflict risk)

- `src/main.rs`: `mod exposure_ramp;` (one line).
- `src/web.rs`: `SchedulePreviewRequest` gains an optional `exposure` field,
  validated and staged (about 10 lines). Without this, the Scheduler page
  cannot stage the setting.
- `src/optic_capture_log.rs`: the test helper builds a `CaptureResult`
  literal and needs the new `exposure: None` field (one line, test only).
- `src/web/styles.css`: one small block appended at the end.

## Implementation

- `src/exposure_ramp.rs` (new): `ScheduleExposure` {Dashboard | AutoRamp},
  `RampSettings` (`validate`, `sanitized`), `meter_yuv420`,
  `target_bias_ev`, `split_exposure`, `max_shutter_for_gap`, `plan`,
  `observe` and the eased grey-world WB. Pure, with 21 unit tests.
- `src/camera.rs`: `CameraSettings.colour_gains: Option<[f32; 2]>` (validated
  0.5..8); shared limit constants (`MIN/MAX_SHUTTER_US`, `MIN/MAX_GAIN`,
  `COLOUR_GAIN_MIN/MAX`) now used by `validate` (same ranges and messages);
  `STILL_CAPTURE_FRAMES = 11`; `CaptureResult.exposure: Option<CaptureExposure>`.
- `src/native_camera.rs` (Linux only): `apply_controls` sets
  `AwbEnable(false)` + `ColourGains` when `colour_gains` is `Some`, and
  otherwise behaves as before. The output frame's `ExposureTime`,
  `AnalogueGain` and `ColourGains` metadata go into `CaptureExposure`. The
  frame is metered after capture, with a new `stage="meter"` perf log line.
- `src/optic_scheduler.rs`: `ScheduleConfig.exposure`; `build_capture_request`
  (Dashboard is unchanged; Seed = auto shutter/gain/AWB; Manual = the plan's
  shutter/gain/gains); `plan_ramp_shot`; the actor keeps one in-memory
  `RampState` and passes the gap to the next shot; `SchedulerStatus.exposure:
  Option<RampSnapshot>`; the capture log records the ramped settings actually
  requested.
- `src/web/scheduler.html`, `scheduler.js`: a "Scheduled exposure" card
  (mode plus 8 limits, staged with the rules, and a live "Last ramped frame"
  readout).
- Outside the owned files (merge-conflict risk, all small):
  - `src/main.rs`: `mod exposure_ramp;`.
  - `src/web.rs`: `SchedulePreviewRequest.exposure: Option<_>`, validated
    (422) and staged only when present, so older clients don't reset it.
  - `src/optic_capture_log.rs` (test helper, `exposure: None`) and
    `src/optic_alerts.rs` (test literal, `..ScheduleConfig::default()`):
    needed to compile the tests after the new struct fields. Test code only.
  - `src/web/styles.css`: 7 lines appended at the end, plus the missing
    final newline.
- Docs: `docs/optic-daemon-exposure-ramping.md` (new);
  `docs/optic-daemon-scheduler.md` (header note on Decision #3, `ScheduleConfig`
  sketch, §5 exposure bullet); `docs/optic-daemon-camera.md` (new §4.1.1).

## Validation

Mac (darwin, host target; `native_camera.rs` compiles to its non-Linux stub):

| Check | Result |
|---|---|
| `cargo fmt --check` | pass |
| `cargo test` | pass, 197 tests (27 new: 21 in `exposure_ramp`, 1 in `camera`, 5 in `optic_scheduler`) |
| `cargo clippy --all-targets -- -D warnings` | pass |
| `npx @biomejs/biome@2.5.14 ci` (CI's pinned version) | pass after `biome format` wrapped one line |
| `node --check src/web/scheduler.js` | pass |

Local daemon (`target/debug/optic-daemon`, `127.0.0.1:18000`, isolated
`HOME`/cache in the session scratchpad, sync disabled, no camera):

- `GET /api/status` on a fresh config → `schedule.exposure = {"mode":"Dashboard"}`,
  status `exposure: null` (criterion 1).
- `POST /api/schedule/preview` with `max_gain: 20` → 422 `invalid scheduled
  exposure: max gain must be between 1 and 16`; with `max_gain: 4` → 200 and
  staged with defaults filled; a body without `exposure` → 200 and the staged
  exposure kept (criterion 9).
- Scheduler page in Chrome: the staged AutoRamp values load into the card;
  editing max shutter to 2 s and gain to 6 stages `2000000`/`6` and enables
  Save rules; a blank field shows a client notice; `day bias 5` shows the
  backend 422 message; switching to Dashboard hides the fields and stages
  `{"mode":"Dashboard"}`; the readout renders ramped and seed snapshots.
- Bug found and fixed: switching to Dashboard and back reset edited limits to
  their defaults (`renderExposure` rewrote every input). Inputs are now only
  overwritten in AutoRamp mode or when empty. Re-tested: values survive the
  toggle (`2`, `6`).
- Bug found and fixed by the sunset simulation test: `split_exposure` gave
  gain ≈ 1.0004 below the shutter ceiling (the shutter rounded down). Gain is
  now exactly 1 until the shutter is at its ceiling (regression test added).
- End to end: committed AutoRamp with a Vancouver station and a 60 s interval
  rule, then resumed. At 06:47:00Z the actor planned a seed, with
  `sun_elevation_deg: -37.66`, `target_bias_ev: -2.0`, `max_shutter_us:
  4227272` (the 60 s gap budget). The capture failed (`camera actor is
  unavailable`, expected on the Mac) and the ramp state stayed unset. The
  scheduler was paused and the daemon stopped afterwards.

Pi, compile-only (user-approved 2026-09-22; no install, no service change,
daemon stayed `active`). The vmpi build VM can't be used: it doesn't mount
this worktree and has no libcamera sysroot.
- The worktree source was uploaded to `~/.local/src/optic-exposure-ramping-check`
  and built into a separate `~/.cache/optic-exposure-ramping-target` (seeded
  from `optic-capture-latency-target`) with the deploy script's sysroot
  environment, `stable` toolchain (1.98.1 not installed; the deploy
  script's own fallback), `CARGO_BUILD_JOBS=2`, `nice -n 10`.
- The first two launches did nothing: the script uploaded empty (a trailing `&`
  backgrounded the `cat > file` too). Fixed with an `scp` upload and a
  `setsid` launch, and recorded in `AGENTS.md`. `src/optic_scheduler.rs` was re-synced
  before the run, so the Pi built the final source.

| Pi step | Result |
|---|---|
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --locked --all-targets -- -D warnings` | pass (Linux `native_camera.rs` compiles: `AwbEnable(false)` + `ColourGains`, metadata capture, meter hook) |
| `cargo test --locked --all-targets --no-run` | pass |
| `cargo test ... exposure_ramp` | 21 passed |
| `cargo test ... native_camera::` | 5 passed |
| `cargo test ... camera::tests::colour_gains` | 1 passed |

Rebase onto `origin/main` (2026-09-22, after PRs #13 focus-tools, #14
pi-services-audit and #15 digest-heartbeat merged):
- Conflicts in `CLAUDE.md` (Environment and Tooling Constraints bullets) and
  `src/web/styles.css` (focus-tools block): both were appends at the end.
  Both sides kept, with this track's lines after upstream's.
- `src/optic_digest.rs` (new upstream, test module only): its
  `ScheduleConfig` literal gets `..ScheduleConfig::default()`. This is outside
  the track, and is needed to compile the tests.
- Re-run on the rebased tree (Mac): `cargo fmt --check` pass;
  `cargo test` 234 passed; `cargo clippy --all-targets -- -D warnings` pass;
  Biome 2.5.14 `ci` pass (12 files); `node --test tests/web/*.test.js` 13
  passed. The earlier Pi compile check ran on the pre-rebase source.

Not deployed, because another track was on the Pi: before the planned deploy
on 2026-09-22, the Pi was running the digest-heartbeat build (committed
`config.json` has a top-level `notifications` block, and `digest_state.json`
had been written minutes earlier). A preview stream was open, and
`preview_config.json` held uncommitted staged edits. The pre-rebase branch had
no `notifications` field, so a deploy plus a test commit would have dropped
that config. The rebased branch includes it. The deploy was put on hold
until the user confirms.

Not run on the Pi, on purpose: the full `cargo test`. `optic_camera.rs:290`
and `optic_scheduler.rs:2138` spawn the real camera on Linux, and would contend
with the running daemon.

Not run: deployment, hardware captures, sunset observation (need user
approval).

## Limitations / Next Steps

- **Unverified on hardware** (it now compiles on Linux). Unknowns: whether `AwbEnable(false)`
  + `ColourGains` is honoured on the output frame after the 11-frame warmup;
  what the real metering cost is on 12 MP frames (expected a few ms); and
  whether γ 2.2 linearization is close enough to the Pi's tone curve (the
  loop is closed, so bias only shifts the steady-state brightness, which
  `day_bias_ev` can offset).
- Long shutters: every capture still waits about 11 frames. The interval
  budget caps a 30 s night interval at about a 2 s shutter. Reducing warmup
  for fully manual frames, and passing the still controls to `camera.start()`,
  needs an on-Pi test.
- Grey-world pulls vivid sunsets and sodium-lit scenes toward neutral
  (slowly, at 3% per frame). The DNG keeps the raw data.
- Ramp state is in memory only; a restart reseeds (by design).
- The original task item "per-rule exposure presets" was dropped by the
  user's decision (global only).
- `Cargo.toml` version not bumped (it is bumped at merge time).
- Pi leftovers from the compile check (not deleted without approval):
  `~/.local/src/optic-exposure-ramping-check`,
  `~/.cache/optic-exposure-ramping-target` (1.5 GB),
  `~/.cache/optic-exposure-ramping-check.{sh,log}`.

## User Verification Steps (after approval to build and deploy)

1. Build and deploy (`./scripts/build-deploy-optic-daemon.sh`), then on the
   Scheduler page choose Auto-ramp, keep the defaults, and Save rules.
2. With a rule of 30–60 s through a sunset: check that "Last ramped frame" shows a
   seed first, then manual frames; that the shutter rises to its cap before the
   gain rises above 1; and that WB changes slowly.
3. Build a timelapse from the frames and check for flicker, and that night
   is darker than day but not black.
