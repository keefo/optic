# Dated Worklog: 2026-09-21 - Exposure Ramping for Scheduled Captures

Status: **Part 1 (ramp engine): deployed 2026-09-22 and partly
hardware-validated** (4 frames: seed, shutter-first jump, 1/3 EV steps,
manual WB; night convergence not observed). **Part 2 (dashboard UX
redesign): implemented and tested on the Mac (unit, node and browser
checks); not deployed, not hardware-verified.** Awaiting user acceptance. The test plan was written before any `src/` edit.

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

### Deploy and hardware test (2026-09-22, user-approved)

- The user discarded the pending staged dashboard edit first. Then
  `./scripts/build-deploy-optic-daemon.sh` (rebased branch = main + ramp)
  ran and exited 0: fmt, 232 tests on the Pi (service stopped), clippy
  `-D warnings`, release build, install with rollback. The service is active.
  `notifications` config was kept and applied; the exposure mode defaulted to
  Dashboard.
- The test committed `{"mode":"AutoRamp"}` (defaults; station, rules and
  notifications resent unchanged) and resumed the existing `every1min` rule
  (60 s, so the budget capped the shutter at 4,227,272 µs). Night, sun ≈ −40°,
  target −2 EV (0.045).

| Frame (UTC) | Kind | Shutter (metadata) | Gain | ColourGains R/B | Luminance |
|---|---|---|---|---|---|
| 08:30:02 | seed (AE/AWB) | 66,654 µs | 15.52 | 2.648 / 1.827 | 0.0096 |
| 08:31:32 | ramped | 4,226,589 µs | 1.145 | 2.634 / 1.823 | 0.089 |
| 08:32:46 | ramped | 3,843,022 µs | 1.000 | 2.662 / 1.827 | 0.066 |
| 08:33:36 | ramped | 3,050,132 µs | 1.000 | 2.684 / 1.828 | 0.0156 |

Observed on hardware:
- A manual 4.2 s shutter is honoured; the previous tested maximum was 1 s.
- `AwbEnable(false)` + `ColourGains` holds on the output frame. The easing
  moved at most 1.05% per frame.
- The first ramped frame after the seed jumped straight to shutter-first
  (the seed's AE used gain 15.5). Later steps were exactly −1/3 EV, with gain
  taken out before the shutter was shortened.
- Captures completed within the 48 s budget (the slowest took 45.5 s; the
  11-frame warmup dominates).
- The loop overshot by 1 EV after the seed jump (0.089 against 0.045): the
  shadow tone curve is steeper than the γ 2.2 model. The loop was correcting
  as designed.
- Frame 4 metered 2.1 EV darker from a 1/3 EV cut. This is most likely a scene
  change (a light going off at 01:33 local); unconfirmed, because the frames
  had already synced off the Pi.
- **Not observed:** night steady state and convergence. Someone (not this
  session) paused the scheduler during frame 4. `schedule_run_state.json` was
  written at 08:33:36, the second the capture finished, because commands wait
  while a capture is in progress.
- Finding: during long ramped captures, Pause/Resume replies wait for the
  capture to finish (up to about 45 s at night). This comes from the existing
  actor design, and is now visible.
- Finding: the Scheduler card rounds `max_step_ev` for display, and
  re-staging wrote 0.33 back (seen staged on the Pi). To be fixed in the
  dashboard redesign.
- Restored afterwards: exposure mode Dashboard committed (a staged-only
  `max_step_ev` diff was superseded), the scheduler left Paused as the pauser
  wanted, and rules, station, notifications and gain 11.8 unchanged.

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

## Part 2: Dashboard UX Redesign (2026-09-22) — plan written before code

### Request (user, 2026-09-22)

The Scheduler-page card is "too professional", not intuitive, and can't be
verified visually. Move scheduled exposure onto the dashboard's camera
controls: a **Scheduled exposure** toggle below Shutter. When it's on, show
the ramp settings, make the manual exposure fields read-only, and have them
show the live values calculated from the ramp, with a subtle colour hint that
they change over time. The user approved adding this to PR #16, then went
AFK: "please use your best judgement going forward". Judgement calls are
listed below. No new deploy or Pi use happens without the user.

### Design (best judgement, recorded for the user's review)

1. **Toggle = `ScheduleConfig.exposure.mode`.** It is staged through a new
   `POST /api/schedule/exposure` (mirrors `/api/config/save-dng`) and
   saved or discarded by the dashboard's existing Save Settings / Discard
   Changes.
2. **Live plan from the server.** `GET /api/status` gains `exposure_plan`,
   computed on each poll from the staged-or-committed schedule, the
   scheduler's in-memory ramp state, and the sun elevation *now*: the next
   planned shutter, gain and colour gains (or `seeding: true`), target
   bias, sun elevation, and shutter cap. It is `null` in Dashboard mode. The
   ramp only learns from captured frames, so the values drift with the sun
   target between frames and jump when a frame is observed.
3. **Locked fields show the plan.** With the toggle on, the Shutter and Gain
   inputs (and White balance) are disabled and show the planned values in an
   amber "ramp" style. A short pulse plays whenever the values change. While
   seeding, they read "Auto (seeding)". The user's manual values are kept
   in the inputs' staged settings and come back when the toggle goes off.
4. **Brightness-equivalent live preview.** The stream request gains an
   optional, never-staged `exposure_override`. With the toggle on, the
   preview uses the plan's total exposure with the shutter clamped to the
   preview frame time and the rest moved into gain (max 16), plus the plan's
   colour gains. The preview then shows the next frame's brightness and
   colour at preview frame rate (noisier). A seeding plan previews with auto
   exposure and AWB, which is what the seed frame will use.
   `reconfigure_stream` stages only `settings`, so the override never
   reaches the saved config. Manual "Capture & transfer" keeps using the
   manual settings, unchanged.
5. **Fewer knobs.** Night look (a slider from dark to bright, mapped to
   `night_drop_ev` 4…0), Max shutter, and Max gain (noise limit). The other
   five go in a collapsed "Advanced". Each input writes only its own field,
   which fixes the Scheduler card's `max_step_ev` rounding write-back.
6. **The Scheduler page card becomes read-only.** It shows the mode, the last
   ramped frame, and "Configure on the Dashboard". `scheduler.js` stops
   sending `exposure`, so it can't overwrite the dashboard's value.
7. New UI code lives in `src/web/scheduled-exposure.js` (the focus-tools
   pattern: pure functions tested with `node --test`). `app.js` only gets
   small hooks: `streamRequest` and `refreshStatus`, plus excluding the new
   inputs from the generic control listener.

### Acceptance criteria (Part 2)

A1. Toggling on the dashboard stages `{"mode":"AutoRamp",…}`, and Save
    Settings commits it. Toggling off stages `Dashboard`.
A2. With the toggle on, the Shutter, Gain and White balance inputs are
    disabled and show `exposure_plan` values (or "Auto (seeding)"),
    refreshed with every status poll, with a visible hint when a value
    changes.
A3. With the toggle on, the preview request carries a brightness-equivalent
    `exposure_override`; the staged `settings` keep the manual values.
    Toggling off removes the override and restores the manual fields.
A4. `exposure_plan` is `null` in Dashboard mode; `seeding: true` with no
    ramp state; otherwise it matches `exposure_ramp::plan` for now.
A5. The three simple knobs plus Advanced stage only the field that was
    edited (no rounding write-back).
A6. The Scheduler page shows ramp status read-only and never sends
    `exposure`.
A7. All existing checks pass (fmt, test, clippy, Biome, node web tests), plus
    new Rust and node tests.

### Test plan (Part 2)

- Rust: `live_exposure_plan` is None for Dashboard, seeding without state,
  and equal to `plan()` with a state; `StreamRequest::effective_settings`
  applies the override (seed → auto/AWB auto; manual → shutter/gain/gains)
  and validates; `exposure_override` defaults to None, so old request JSON
  still parses; `POST /api/schedule/exposure` validation (reuses
  `RampSettings::validate`); the `web.rs` asset test checks the new element
  IDs.
- Node: `previewEquivalent` (a clamp to frame time moves exposure into gain;
  capped at 16; seeding → auto), the night-look mapping in both directions,
  shutter formatting.
- Local daemon plus Chrome on the Mac (no camera): toggle staging, locked
  fields, Advanced, Save/Discard, and the Scheduler page read-only card. The
  preview override on real frames needs the Pi, so it is left for the user.

### Implementation (Part 2)

- `src/camera.rs`: `StreamRequest.exposure_override: Option<ExposureOverride>`
  (`#[serde(default)]`, so older clients still parse); `effective_settings()`
  (override applied; no colour gains → AWB auto); `validate()` checks the
  effective settings.
- `src/native_camera.rs`: preview start/reconfigure uses
  `request.effective_settings()`. Still capture is unchanged.
- `src/optic_scheduler.rs`: `SchedulerStatus.ramp_state` (`#[serde(skip)]`,
  published after each capture); `LiveExposurePlan` and
  `live_exposure_plan(schedule, ramp_state, now)`, which reuses
  `exposure_ramp::plan`, the sun elevation now, and the interval budget
  (6 h lookahead).
- `src/web.rs` (outside the owned files): `StatusResponse.exposure_plan`;
  `POST /api/schedule/exposure` (staging like `stage_save_dng`, 422 via
  `RampSettings::validate`); a wiring test like focus-tools'.
- `src/web/scheduled-exposure.js` (new): the toggle, the three simple
  controls plus Advanced, the locked amber live values with a pulse, the
  caption (target, sun, shutter cap, last learned, and preview shortfall),
  and the preview override. The pure functions are exported for Node.
- `src/web/index.html`, `src/web/app.js` (outside the task brief's owned
  files; focus-tools, their owner, merged in #13): the toggle block below
  Shutter and the live-value outputs; `app.js` adds 28 lines (the override in
  `streamRequest`, `applyConfig`/`onStatus` hooks, a
  `scheduled-exposure-change` listener, and the manual-control selector
  excluding `data-ramp-input`).
- `src/web/scheduler.{html,js}`: the card is read-only (mode pill, last
  ramped frame, link to the Dashboard); `exposure` is no longer sent.
- `src/web/styles.css`: the unused card grid rules were removed and a
  dashboard block appended. Gain is pinned full-width, because the new block
  shifts the existing `nth-last-child(-n+3)` span rule.
  `prefers-reduced-motion` disables the pulse.
- `biome.json`: added `src/web/scheduled-exposure.js` (outside the owned
  files). `tests/web/scheduled-exposure.test.js` (new).
- Docs: `docs/optic-daemon-exposure-ramping.md` §4/§7/§10;
  `docs/optic-daemon.md` API list (`exposure_plan`,
  `/api/schedule/exposure`, `exposure_override`).

### Validation (Part 2, Mac)

| Check | Result |
|---|---|
| `cargo fmt --check` | pass |
| `cargo test` | 238 passed (+4: override parsing/effective settings, live plan ×2, dashboard wiring) |
| `cargo clippy --all-targets -- -D warnings` | pass |
| Biome 2.5.14 `ci` (13 files) | pass (`biome format` applied to the touched web files) |
| `node --test tests/web/*.test.js` | 22 passed (9 new) |

Local daemon (no camera, Vancouver station, 60 s rule) in Chrome:
- `POST /api/schedule/exposure` with `night_drop_ev: 9` → 422 `night
  darkening must be between 0 and 6 EV`.
- Toggle on → staged `AutoRamp`, Save enabled; `exposure_plan` = seeding,
  sun −39.4°, target −2.0 EV, cap 4,227,272 µs (A1, A4). Shutter, Gain and
  White balance hidden/disabled, showing "Auto (seeding)" in amber
  (A2). `settings()` still returns the manual values; the stream override is
  `{0, 0, null}` (auto, like the seed) (A3).
- Simulated plan 3.84 s → 3.05 s: the live fields read "3.8 s" / "1.00×" /
  "Eased · R 2.63 / B 1.82" and then "3.1 s" with the pulse class; the
  override is 118,750 µs × 16 (8 FPS, caption "preview ≈ 1.0 EV darker"),
  and 475,000 µs × 8.09 for the 2 FPS Master Archive preview; two change
  events.
- Night look → Bright staged only `night_drop_ev: 2 → 0` (A5; the rounding
  write-back is fixed).
- Discard reverts the toggle and fields to the committed mode and leaves
  nothing staged; Save commits `AutoRamp` (A1).
- Scheduler page: an "Auto-ramp" pill and a link to `/`, with no editable
  inputs; staging rules leaves `exposure` unchanged (A6).
- Side observation (not from this change): in one run, `config_staged`
  read `true` right after Discard. This matches the existing behaviour of
  `start_stream`/`reconfigure_stream` staging the current camera `settings`
  whenever the preview (re)starts. A later reproduction staged nothing.

GitHub CI on PR #16 head `1db6a39`: Detect changed scope pass, Biome
pass, **Rust (Debian 13 arm64) pass** (Linux build and tests, including the
`native_camera.rs` preview-override path), Timelapse tool skipped (not
touched).

Not verified (needs the Pi and user approval, so not done while the user
is away): the preview override on real frames, and whether the preview's
brightness matches the scheduled frames.

### Limitations / next steps (Part 2)

- At night the preview can't fully match the frame: an 8 FPS preview tops
  out at about 119 ms × 16 gain, so the caption shows the shortfall
  (≈ 1 EV for a 4 s frame). Master Archive's 2 FPS preview gets 8× more
  headroom.
- The live plan only changes when a scheduled frame is observed, or when the
  sun target drifts. It doesn't learn from the preview.
- Starting a preview with the toggle on uses the override, so another tab
  viewing the preview sees the ramp exposure too. This is intended, since the
  preview is shared.
- Deploying Part 2 and checking it on the camera is left for the user
  (steps below).

### Deploy (Part 2, 2026-09-22, user-requested)

- Pre-flight: the worktree was clean at `aeb0ad9`; no build was running on the
  Pi; nothing was staged; no preview was streaming; the scheduler was paused;
  the mode was Dashboard.
- `./scripts/build-deploy-optic-daemon.sh` exited 0: Biome, fmt, 234
  tests on the Pi, clippy `-D warnings`, release build, install with
  rollback. The service is `active` and the journal shows no warnings since
  the restart.
- Verified live: `/api/status` has `exposure_plan` (`null` in Dashboard
  mode); `/scheduled-exposure.js` is served (200, 10,999 B); the dashboard HTML
  has `#scheduled-exposure`; `POST /api/schedule/exposure` with
  `max_gain: 99` → 422, with nothing staged. Rules, station, notifications
  and gain 11.8 are unchanged; still paused; still Dashboard mode.
- **Deployed, not yet hardware-verified in the UI.** The live preview
  override on real frames is the user's acceptance step below.

### User verification (Part 2)

1. Approve a deploy (`./scripts/build-deploy-optic-daemon.sh`), or merge
   PR #16 and deploy `main`.
2. On the dashboard, turn on **Scheduled exposure**. Shutter, Gain and White
   balance turn amber and read "Auto (seeding)". The preview switches to auto
   exposure. Press **Save Settings**.
3. Resume the scheduler for a few frames. The amber values should change to
   the ramp's shutter, gain and WB and pulse, and the preview's brightness
   should follow them (see the caption for any preview shortfall).
4. Move **Night look** and watch the target in the caption. Turn the toggle
   off and check that your manual values come back.

## Part 3: Follow-ups on the Pi (2026-09-22, user-requested)

### Live values while seeding

- Request: "instead of showing Auto (seeding), can we show real calculated
  value? coloring is good."
- Change (web only): while the plan is seeding, the locked fields show the
  live preview's own auto exposure. The preview runs the same auto
  exposure/AWB the seed frame uses, and every MJPEG part already carries
  `X-Optic-Exposure-Us`/`-Analogue-Gain`/`-Colour-Gains`. Ramped plans still
  show the planned values, ignoring the preview's brightness-equivalent
  override values. Gain is shown to one decimal while seeding, and pulses are
  throttled to one per 2 s per field (auto exposure jitters at preview frame
  rate). Without a running preview the fields read "Auto"/"AWB". Files:
  `src/web/scheduled-exposure.js`, `src/web/app.js` (+2 lines:
  `onPreviewFrame(frameMetadata(headers))`, `clearPreview()`),
  `tests/web/scheduled-exposure.test.js` (+2 tests), `src/web.rs` wiring test.
- Checks (Mac): Biome pass; node 24 passed; `cargo test` 238 passed;
  clippy clean.
- Deployed (user-approved, assets only:
  `./scripts/build-deploy-optic-daemon.sh --assets`, no service restart):
  "SUCCESS: static web assets deployed and verified"; the served `app.js`
  and `scheduled-exposure.js` contain the new hooks.
- Hardware check in Chrome on `optic.local` (daylight, sun +25.9°): with the
  toggle on (staged only), the locked fields read **1/683 s · 1.0× · AWB ·
  R 3.66 / B 1.46** in amber with the glow; caption "showing the camera's
  live auto exposure". Toggle then turned off: mode Dashboard, nothing
  staged, manual fields editable.
- Observed while testing: (1) in one run, the toggle and a pre-existing
  staged edit were reverted within seconds of my click, which fits a Discard
  from another dashboard tab (the user was active); a retrace showed the
  toggle staying on. (2) With the full-resolution Master Archive preview
  (2 FPS, ~2.7 MB/frame over Wi-Fi), no frames arrived for >6 s after the
  toggle's reconfigure, and the tab's renderer stopped answering DevTools
  for 45 s. With downsampled preview on (as it then was), frames and values
  flowed normally. This is a pre-existing cost of the full-resolution
  preview, not specific to this change.

### Rotation moved under Transform

- Request: "I think we could put Rotation under Transform". Rotation's
  select moved into the Transform fieldset (its own line, the two flips
  below); it stays inside `.control-grid`, so app.js' control listener and
  `#rotation` lookups are unchanged. Removing it from the grid also removes
  the half-width row it sat on. `src/web/index.html` + 3 CSS lines appended.
- Checked on a local daemon in Chrome: layout (Rotation / H flip + V flip);
  selecting 180° updates `settings().rotation` and starts a preview
  measurement ("Rotation · revision 2").
- Deployed (user-approved, assets only, no service restart): the served
  `index.html` and `styles.css` contain `transform-rotation`.

### Result: exposure compensation does not work on this pipeline (reverted)

Implemented as designed (`ExposureValue` + `AeEnable(true)` +
`ExposureTimeMode::Auto`/`AnalogueGainMode::Auto`, only for the biased-auto
case), deployed to the Pi (full deploy, 235 tests on the Pi, clippy, release,
exit 0), then measured on the camera in daylight.

**libcamera accepts the control and ignores it.** No "unsupported control"
warning appears in the journal, but neither the reported exposure nor the
image changes:

| `ev` requested | Reported shutter × gain | Mean image luma (160×120) |
|---|---|---|
| −3 | 193 µs | 29.0 |
| 0 | 193 µs | 29.1 |
| +3 | 193 µs | 29.1 |

Method: `/api/stream/reconfigure` with `exposure_override {shutter_us: 0,
gain: 0, ev}`, 9–12 s settle, then one frame pulled from
`/api/stream/mjpeg`; headers for the exposure, and a canvas decode for the
luma. AE reported `converged` throughout.

Measurement pitfall worth recording: a first sweep *seemed* to work
(386 → 939 → 3924 µs). That was the dashboard racing the test — it
re-applies its own override on every 3 s status poll, so the page was
overwriting the value under test. Re-run from `capture-history.html` (same
origin, no preview logic) with the stream started by the test itself, the
values are flat. Any future camera-control experiment must run with no
dashboard tab open.

Reverted in `101798b` so the capture path keeps its proven control set
rather than an unverified AE branch. `RampPlan::Seed` is a unit variant
again. The stepped Day brightness slider (separate commit) is kept.

**Consequence:** the Part 4 requirement ("unsaved settings must show in the
live preview, whatever the scheduler is doing") is still unmet while the
ramp has no state. The remaining options are in the next section.
