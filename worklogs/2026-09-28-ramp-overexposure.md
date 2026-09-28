# Dated Worklog: 2026-09-28 - AutoRamp Overexposed Frame at Dawn (08:00)

**Status:** cause identified and replayed offline; fix **implemented and
unit-tested on the Mac**. **Not compiled for Linux, not deployed, not
hardware-validated, not user-accepted.** (Investigation, acceptance
criteria and test plan were written before any `src/` change.)

## Objective

One scheduled capture came out white:
`scheduler-master-archive-everyhour-1790607601441.jpg` (2026-09-28 08:00:01
PDT, rule `everyhour`, `AutoRamp`). Find out why, fix it, and make any future
failure like it obvious in the logs.

## Evidence (read-only; gathered 2026-09-28)

Sources: the sidecar in `~/Pictures/Optic` (read only), the Pi's
`/api/status`, `/api/captures` and `journalctl -u optic-daemon` (read-only
SSH).

| What | Value |
|---|---|
| Rule | `everyhour`: interval 3600 s aligned to the wall clock, window 08:00–21:00 every day |
| Station | 49.2236 N, −123.0009 E, America/Vancouver |
| Sun at 08:00:00 PDT | +7.6° (above the +6° day threshold, so the target is the day level) |
| 08:00 requested (`settings`, i.e. the built `CaptureRequest`) | `shutter_us: 343529`, `gain: 1.0` |
| 08:00 actual | 342,916 µs, gain 1.0; meter L = 0.807, clipped 60.1% |
| Warmup frames | 2, both already at 342,916 µs (manual-match early exit) |
| Previous scheduled captures | a 1-minute rule until 00:58:09 PDT, last at 4,227,272 µs × 1.12 gain |
| Daemon restarts | 01:10:36 → 01:13:39 and 01:27:29 → **01:31:00 PDT** (ramp state is in memory, so it was reset to `None`) |
| Live preview | pipeline **started at 01:31:26 PDT and ran without a stop until 08:00:00**, when the scheduled capture's `stop_existing_preview` stopped it |
| Afterwards | 09:00–11:00 captures in `Dashboard` mode, `shutter 0` (auto) × gain 11.5: 552–635 µs × 11.38 |

Correction to the brief: "correct exposure was about 0.6 ms" leaves out the
11.38× gain. The 09:00 dashboard frame is 635 µs × 11.38 ≈ **7.2 ms at gain 1**,
so 343 ms at gain 1 was about **5.6 EV** over the dashboard frame an hour
later, not 9 stops. At 07:59 on 2026-09-25 (sun +8°), the ramp's mid-grey
exposure was at most 53 ms, which puts 343 ms about 2.7 EV over.

## Which code path ran

**It was a `Manual` plan, not a seed.** `build_capture_request` writes
`shutter_us = 0, gain = 0` for `RampPlan::Seed`, and the sidecar's `settings`
is that request. A seed would also have run all 11 warmup frames, because
`capture_this_frame` never exits early for auto exposure. The 08:00 capture
requested 343,529 µs × 1.0 and stopped at frame 2 on a manual match.

**Where 343,529 µs comes from.** After the 01:31 restart the ramp had no
state. No scheduled capture ran between 01:31 and 08:00: the 1-minute rule
had ended and `everyhour`'s window opens at 08:00. So every bit of ramp state
at 08:00 came from `observe_preview`:

- `observe_preview` with no state creates one with
  `planned_log2_exposure: None`, so the next capture plan has **no step
  limit**.
- Every preview frame sets `updated_at = now`, so `plan()`'s
  `now − updated_at ≤ 30 min` check passed at 08:00 even though no frame
  had been captured for 7 hours.
- In `AutoRamp` the preview runs the dashboard's `exposure_override`, and
  `previewEquivalent` clamps that to one frame time (118,750 µs at 8 fps × 0.95)
  and at most 16× gain. The night plan (~4.2 s × 1.1–1.3) sits at that cap:
  118,750 × 16 = 1.90e6 µs·gain.
- As the sky brightened, preview frames at that fixed exposure turned white.
  A saturated frame's trimmed log-mean is `linear(255)` = 1.0043, so the
  "scene" sample is `log2(1.0043) − log2(1.90e6)` ≈ −20.85. That is only a
  **lower bound** on the scene's brightness, but the estimate took it as a
  measurement and converged to it (5 s time constant).
- `plan` = `log2(0.18) − (−20.85)` = **18.39 → 2^18.39 ≈ 343 ms at gain 1**
  (shutter-first below the 5 s cap).

The prediction `0.18 × 118,750 × 16 / 1.0043` = 340.5 ms is within 1% of the
343,529 µs requested. The remaining 0.9% is the sensor's line quantisation of
the preview exposure, the same order as the +1.0% recorded in `camera.rs`.
Replayed in the unit test `replay_2026_09_28_dawn_overexposure` (below).

For the preview exposure to stay at the 16× cap through dawn, the client
holding the stream cannot have been sending new overrides. A live dashboard
would have followed the falling plan down: 343 ms plan → 118,750 µs × 2.9.
Which client held the stream (for example a backgrounded tab or a locked
phone) cannot be told from the logs.

### Verdict on the three hypotheses in the brief

1. **Reseed did not fire because preview activity keeps the state fresh:
   supported.** This is the enabling cause. The only reseed check is
   `now − updated_at`, and `observe_preview` refreshes `updated_at` at frame
   rate. The state was never anchored to a capture.
2. **Seed ran but auto exposure did not converge in 2 warmup frames:
   refuted for this incident.** The frame was a manual plan (see above). A
   seed keeps all 11 frames. On 2026-09-28 01:17, a dashboard auto-exposure
   capture was still moving about 0.2 EV between frames 9 and 11, so AE is
   roughly but not exactly converged by frame 11. That is not what happened here.
3. **Scene estimate learned from preview frames exposed by something other
   than the ramp: supported, and made more precise.** The estimate came from
   preview frames **saturated** at the preview override's 16× ceiling. A
   saturated meter is a lower bound, and with no capture anchor the plan
   jumped to it with no step limit.

The ramp logged nothing about the decision (only `highlight guard darkened the
ramp` exists), so this was proven from arithmetic and timelines, not from a
log line. Fixing that is part of this work.

## Clamp data (for the sanity bound)

Mid-grey exposure product `0.18 / 2^(log2 L − log2(µs × gain))`, over 2,269
scheduler frames with a usable meter (0.005 < L < 0.5), grouped by sun
elevation computed for the station. Only two dawns and dusks
(2026-09-24/25) cover the horizon:

| Sun | max mid-grey exposure (gain 1) |
|---|---|
| 0…+1° | 621 ms |
| +1…+2° | 390 ms |
| +2…+3° | 283 ms |
| +3…+4° | 154 ms |
| +4…+6° | 109 ms |
| +6…+7° | 79 ms |
| +7…+9° | 53 ms |
| ≥ +10° | ≤ 41 ms |

**This contradicts the brief's "sun above the horizon → refuse multi-hundred-ms
exposures".** On this station, 300–600 ms is normal with the sun at 0–2°, so a
flat sub-100 ms bound would clip real dawns. The bound has to follow
elevation. Proposal, subject to the user's decision (asked with the question
tool):

```
ceiling(el) = max(160 ms, 2.5 s · 2^(−el / 2)) · 2^day_bias_ev   for el ≥ 0°
no ceiling below the horizon or without a station
```

Measured frame by frame at each frame's own elevation, it is at least **1.6 EV**
above every observed mid-grey exposure (tightest: 52.8 ms at +8.2°). (An
earlier draft of this line said "≥ 2 EV" from bin edges. The unit test caught
it: 1.98 EV at +6°, and 1.6 EV per frame. The user-approved formula is
unchanged; the justification is corrected.) At +7.6° it is 180 ms, so on its own it
would have cut the incident by about 0.9 EV, not prevented it. It is a
backstop. The reseed rule is the fix.

## Acceptance criteria

A1. A `RampState` that has not learned from a **scheduled capture** within
    `RESEED_GAP` plans a `Seed` for the next capture, however recently preview
    frames updated it. The dashboard's live plan and preview override keep
    working from preview-learned state (§11 behaviour unchanged).
A2. Replaying the 2026-09-28 sequence (night captures, restart, 6.5 h of
    preview frames at 118,750 µs × 16 that saturate at dawn, hourly capture at
    +7.6°) plans ≈ 343 ms on the old rule (proves the cause) and a `Seed` on
    the new one.
A3. Every scheduled `AutoRamp` capture logs one INFO line with the decision:
    `seed` or `plan`, the reason for a seed (no state, no capture yet, gap),
    the age of the last capture, scene estimate, sun elevation, target bias,
    desired and planned log2 exposure, whether the step limit and the daylight
    ceiling bound it, the guard offset, the last frame's clipped fraction, and
    the resulting shutter/gain. Every observed result logs one INFO line too.
    A plan cut by the daylight ceiling logs a WARN.
A4. Daylight sanity ceiling (bound as decided by the user) applies to capture
    plans and preview plans alike, never below the horizon or without a
    station, and never brightens a plan.
A5. Existing ramp behaviour and tests are unchanged apart from these rules:
    step limits, guard, preview learning, interval budget.
A6. `cargo fmt --check`, `cargo test --locked`,
    `cargo clippy --locked --all-targets -- -D warnings` pass on the Mac.
A7. `docs/optic-daemon-exposure-ramping.md` describes the new reseed rule, the
    ceiling and the logging; this worklog records results.

## Decisions (user, 2026-09-28, via the question tool)

| Question | Answer |
|---|---|
| Sanity ceiling | **Elevation curve**: `max(160 ms, 2.5 s · 2^(−el/2)) · 2^day_bias_ev`, sun ≥ 0° only, WARN when it binds |
| Meter-then-shoot for long gaps | **Implement now** |
| Time-based step limit | **Implement now**: `max_step_ev × max(1, minutes since the last capture)` |

Added acceptance criteria:

A8. **Meter-then-shoot.** When a scheduled `AutoRamp` shot's plan is a seed
    (no capture within `RESEED_GAP`), the scheduler first takes an unsaved
    auto-exposure metering still (`OpticCamera::meter`: same pipeline and
    warmup as a seed still, no JPEG/DNG encode, no file, no capture-history
    row), folds it in as a seed, re-plans, and shoots the resulting manual
    plan. The target curve and ceiling therefore apply to long-gap shots too.
    If metering fails, it falls back to the old behaviour (the seed still is
    the output frame) with a WARN.
A9. **Time-based step limit.** The per-plan step limit is
    `max_step_ev × max(1, minutes since the anchoring capture)`, in both
    directions. The guard's extra downward allowance is unchanged. 1-minute
    and faster rules behave exactly as before.
A10. A ramped output frame with ≥ 25% of samples clipped logs a WARN
    (`ramped capture is overexposed`). `/api/status` → `schedule.exposure` also
    reports whether a metering pass ran, whether the ceiling bound, and the
    clipped fraction.

Added tests:

- `meter_then_shoot_plans_the_target_from_the_metering_frame`: a seed
  observation of an AE dawn frame followed by a re-plan gives a manual plan
  at the day target, and within the ceiling.
- `step_limit_scales_with_minutes_since_the_capture`: 1 min → 1 step; 10 min
  → 10 steps; 30 s → still 1 step.
- The replay test, after the fix: capture plan `Seed`; after a metering
  frame at the real dawn scene, the plan is within 0.1 EV of the day target
  (≈ 45 ms), not 343 ms.

Scope note: meter-then-shoot needs a small addition outside the files this
track owns: a `Meter` command in `src/optic_camera.rs` and the Linux/stub
backends in `src/native_camera.rs`. It reuses the existing `capture_frame`
(warmup, AE) and `meter_yuv420` unchanged.

## Test plan (written before implementation)

Host unit tests in `src/exposure_ramp.rs` (`cargo test --locked`, Mac):

- `replay_2026_09_28_dawn_overexposure`: builds the state as the Pi did
  (preview-born state, preview frames at 118,750 µs × 16 from 01:31 with a
  night scene, then a dawn scene that saturates them, i.e. meter
  L = 1.0043 / clipped ≈ 1.0). On the **pre-fix** rule it asserts a
  `Manual` plan of 343 ms ± 2% at gain 1 (proof); after the fix the same
  replay asserts `Seed`. Written first and run against unmodified code to
  show the old path.
- `preview_activity_does_not_keep_a_stale_capture_state_fresh`: captured
  state at T, preview frames every few seconds for 2 h, plan at T+2 h → Seed
  (fails today: today it plans `Manual`).
- `preview_only_state_seeds_the_capture_but_still_drives_the_preview`: state
  created by preview only → capture plan `Seed`; `preview_plans` still
  `Some` (§11 kept).
- `a_recent_capture_keeps_planning_with_preview_updates`: capture at T,
  preview frames, plan at T+10 min → `Manual`, step-limited from the capture.
- Ceiling tests: at +7.6° the plan is ≤ ceiling; below 0° and without a
  station no ceiling applies; the ceiling never brightens; `day_bias_ev`
  shifts it.
- `decision` tests: the decision struct reports reason/step-limit/ceiling
  flags that match the plan.

Scheduler (`src/optic_scheduler.rs` tests): `live_exposure_plan` still shows
a manual preview plan for preview-only state and reports that the next capture
seeds.

Failure cases covered: no state; preview-only state; stale capture with fresh
preview; fresh capture; saturated preview frames; no station.

Target environment for real validation: the Pi (needs user approval to
deploy), across a dawn with an hourly rule in `AutoRamp`, compared against the
numbers above. Not part of this session unless approved.

## Implementation (2026-09-28)

- `src/exposure_ramp.rs`
  - `RampState.captured_at`: set by `observe` (captures and metering
    passes), carried by `observe_preview`, `None` for preview-born state.
  - `decide()` → `RampDecision` (plan, `SeedReason`, capture age, scene,
    sun, target, desired E, step limit and whether it bound, ceiling and
    whether it bound, guard offset and previous excess, max shutter).
    `plan()` = `decide().plan`. Capture freshness: a capture within
    `RESEED_GAP`, otherwise `no_state` / `no_capture` / `capture_gap`.
  - `preview_plan()`: the old `updated_at` freshness, used only by
    `preview_plans` and the live plan (§11 preview behaviour unchanged).
  - `step_limit_ev()`: `max_step_ev × max(1, minutes since the capture)`.
  - `daylight_ceiling_log2()`, applied in `ExposureLimits::split` (capture
    and preview plans). The guard's `E_max` is unchanged.
  - `OVEREXPOSED_CLIPPED_FRACTION = 0.25`.
- `src/optic_scheduler.rs`: `RampShot` carries the decision;
  `fire_capture` logs every decision, runs `meter_then_plan` for seeds
  (unsaved metering still → `observe` as seed → re-plan → manual shot; on
  failure WARN and shoot the seed), and logs every observation plus the
  overexposure WARN. `LiveExposurePlan` gains `ramp_captured_at` and
  `capture_meters_first`; `RampSnapshot` gains `metered`, `seed_reason`,
  `ceiling_bound`, `clipped_fraction` (all additive JSON).
- `src/optic_camera.rs`: `CameraCommand::Meter` / `OpticCamera::meter`.
- `src/native_camera.rs`: `NativeCommand::Meter` (Linux: stop preview,
  `capture_frame` + `meter_yuv420`, no encode or write, `metering_pass`
  perf log); stub returns `Unavailable`. Warmup count and `set_controls`
  unchanged: the seed path's AE behaviour is the existing 11-frame still.
- `docs/optic-daemon-exposure-ramping.md`: status line, §5.3 time-based
  step, §5.4 rewritten (capture-anchored reseed, incident, meter-then-shoot),
  new §5.8 ceiling and §5.9 logging, §8 failure rows, §9 follow-ups, §11
  "starts from a converged estimate" withdrawn.
- No JS/UI change, no dependency or version change (0.1.33; the bump happens
  at merge).

## Validation (Mac, 2026-09-28)

| Check | Result |
|---|---|
| `cargo test --locked replay_2026_09_28` on **unmodified** code | passed: preview-only state, saturated dawn frames → `Manual` plan within 2% of 343,529 µs at gain 1.0 (reproduces the incident) |
| `cargo test --locked` after the core change, before updating old tests | 271 passed, 2 failed: exactly the two tests encoding the old rule (`replay_2026_09_28_dawn_overexposure` asserting `Manual`, `a_preview_seeded_ramp_plans_without_a_step_limit`) |
| `cargo test --locked` (final) | **279 passed, 0 failed** (273 before: +6 new tests, 2 rewritten, 1 scheduler test extended) |
| `cargo fmt --check` | passed |
| `cargo clippy --locked --all-targets -- -D warnings` | passed (Mac target) |
| Linux/aarch64 compile of `native_camera.rs` `Meter` arm | **not run.** The `vmpi` VM is stopped and lacks the RPi `libcamera-dev` 0.7.2+rpt; compiling on the Pi is not read-only. It mirrors the existing `Capture` arm's calls and was reviewed by hand. CI's `rust-arm64` job will compile it on the first push. |

Regression tests (fail on the old behaviour, pass now):
`replay_2026_09_28_dawn_overexposure` (old path 343 ms; ceiling alone 180 ms;
now `Seed/no_capture`, then a metering frame → plan within 0.1 EV of the 45 ms
day target), `preview_activity_does_not_keep_a_stale_capture_state_fresh`,
`a_preview_seeded_ramp_previews_without_a_step_limit_but_captures_meter_first`,
`a_recent_capture_keeps_planning_through_preview_updates` (10 min → 10 steps),
`step_limit_scales_with_minutes_since_the_capture`, `seeds_report_why`,
`daylight_ceiling_follows_the_sun`, `daylight_ceiling_only_ever_darkens_a_plan`,
and `live_exposure_plan_seeds_without_state_and_tracks_the_ramp_with_it`
(extended: `capture_meters_first`).

Review fix: after a metering pass, the snapshot's `seed_reason` was
overwritten by the re-plan. It now keeps the original reason.

## Limitations and risks

- Unverified on Linux and on hardware. The metering pass's timing and AE
  convergence (11 frames, 500 ms frame cap at night) are unmeasured. At
  night AE may meter dark, but the estimate is normalised by the actual
  exposure, so it stays correct unless the frame is crushed.
- A metering pass delays the output frame by one still: about 1 s by day,
  more at night. Only long-gap shots pay it.
- The ceiling comes from one station and two mornings (1.6 EV minimum
  margin). A very dark day could reach it; the WARN makes that visible.
- Preview learning still treats saturated frames as measurements. Captures
  no longer plan from preview-only state, but the preview's own plan can
  still sit on that bound when no client re-sends the override (UI
  follow-up). Which client held the stream overnight is unknown.
- WARNs go to the journal and `/api/status` only, not to system events or
  ntfy alerts (needs `main.rs` wiring; follow-up).
- The 30-minute `RESEED_GAP` is unchanged. With the time-based step, a rule
  every 10–30 min may now move up to 10–30 steps between frames.

## Proposed validation on the Pi (needs user approval)

1. Merge with the version bump (0.1.34), then deploy it with the existing
   script. Leave `Dashboard` mode on until deployed.
2. Before a dawn, switch `schedule.exposure` to `AutoRamp` (defaults) with
   the hourly `everyhour` rule, and optionally a 10-minute rule across
   06:30–09:00.
3. Watch `journalctl -u optic-daemon | grep -E "exposure ramp|metering_pass|overexposed|daylight ceiling"`.
   Expected at 08:00: a `seed_reason=no_capture` or `capture_gap` decision,
   a `metering_pass`, a second decision `plan` with `ceiling_bound=false`,
   and a frame of about 30–60 ms at gain 1 (compare: 343 ms / 60% clipped
   on 2026-09-28; 7.2 ms-equivalent dashboard frames at 09:00). No
   `overexposed` WARN.
4. Compare sidecars: `clipped_fraction` < 0.1 and YAVG not near 255.
5. Deliberately leave a preview open overnight, as on 2026-09-28, to confirm
   the dawn capture still meters first.

## User verification

After deploy: confirm the dawn frames look right, and that the dashboard
preview still shows the ramp plan before the first capture.
