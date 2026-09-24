# Dated Worklog: 2026-09-23 - Highlight Guard (Clipping Budget) for the Exposure Ramp

Status: **implemented and host-tested (Mac); not deployed, not
hardware-validated, not user-accepted.** The test plan was written before any
`src/` edit. On-Pi validation needs the user's approval and a deploy of `main`
plus this branch (see "Proposed overnight comparison").

## Objective

In overnight footage, street lamps and building lights bloom into large
blown-out white areas. The exposure ramp (`src/exposure_ramp.rs`, PR #16)
meters a trimmed log mean and opens up until it reaches the night target. It
already measures `FrameMeter.clipped_fraction` but never uses it. Give
clipping a **budget**: a small fraction of samples may clip (emissive lamps
should), and above that the ramp backs off. Design:
`docs/optic-daemon-exposure-ramping.md` §5.6.

## Premise check against real data (2026-09-23)

Source: `/Users/admin/Pictures/Optic` (read-only; nothing moved, renamed or
deleted). 1,339 scheduled captures, `master_archive`, one per minute,
2026-09-22 15:12 → 2026-09-23 13:30 PDT. Each has a `.log.json` sidecar.

**Correction to the task brief.** The brief said exposure ramping was not yet
deployed on the Pi (0.1.31), so last night's frames were not ramped. That is
wrong:

- `GET /api/status` on the Pi (read-only, port 8000) reports `version:
  0.1.31`, `config.schedule.exposure.mode: AutoRamp` with default settings
  (`night_drop_ev 2`, `max_step_ev 1/3`, `smoothing 0.5`, `max_gain 8`), and a
  live ramp snapshot (`seed: false`, `max_shutter_us: 4227272`).
- `optic-daemon.service` has been active since 2026-09-22 14:14 PDT (before the run).
- The sidecars' shutter varies smoothly frame to frame. At night it caps at exactly
  4,227,272 µs, which is `max_shutter_for_gap` for a 60 s interval,
  `(0.8·60 s − 1.5 s)/11`. Above that cap the gain rises (1.03–1.30).
- `worklogs/2026-09-21-exposure-ramping.md` already records Part 1 as deployed
  on 2026-09-22. The design doc's status line still said "Not deployed". It is
  corrected in this change.

So these frames **are ramped** frames: they show what the current ramp does,
which is the right baseline for the guard. They carry no actual-exposure
metadata (0.1.31 predates `worklogs/2026-09-22-capture-exposure-metadata.md`),
so "exposure" below means the *planned* shutter × gain from the sidecar.

**Method.** A scratch Python script (scratchpad venv: Pillow, NumPy, SciPy;
not committed) replicates `meter_yuv420` on the JPEG luma. That means a 1/16
sample grid, 2% trims, γ 2.2 log mean, and clipped = Y ≥ 250. It ran on all
frames at 1/4 resolution. Connected-component blob sizes were measured at
full resolution (4056×3040) on selected frames. The meter reproduces the
daemon's target: night L = 0.045 = 0.18 · 2⁻², exactly the −2 EV night target.

### Findings

| Period | Exposure | Mean L | Clipped | p95 / p99 luma | Y ≥ 200 |
|---|---|---|---|---|---|
| Day (sun > +3°) | 0.6–5 ms | 0.14–0.30 | 0–7% (sky, glass) | 190–250 / 205–255 | 2–19% |
| Sun +2° … −3° | 28–240 ms | 0.09–0.13 | 0.01–0.08% | 145–172 / 177–220 | < 1% |
| 19:42–21:15 (room lights on) | 0.63–0.95 s | 0.044–0.086 | 0.35–0.72% | 154–183 / 218–244 | 1.5–2.5% |
| Night 21:27–06:00 (n = 514) | 3.1–4.2 s | ≈ 0.045 | median 2.65%, p90 2.96%, max 3.7% | 229 / 254 | 7–11% |
| Dawn, sun −9° … −4° | 4.2 s × 1.0–1.3 | 0.06–0.09 | 3.2–4.0% | 245 / 255 | 10–12% |

**Mechanism (21:16 and 21:22).** The camera shoots through a window. Room
lights reflected in the glass were part of the metered scene. When they went
off, the scene fell 1.5 EV and then 1.9 EV (2.2 EV net). The ramp opened
0.64 s → 3.1 s (+2.3 EV) and brought the mean back to target. The clipped
fraction went from 0.35% to 2.5%, and the largest blob from ~330 to ~3,200 px at
1/4 resolution. The store entrance, parking-lot floodlights and canopy bloom.

**Natural experiment (the −1.6 EV frames).** Dozens of night frames
(21:47–05:25, often every 3–5 minutes) came out about 1.6 EV darker than their
neighbours at the same planned shutter. The cause is not determined; it is
recorded as a follow-up. They show the same scene at about −1.6 EV and look
visibly better:

| Full-resolution frame | Clipped (Y ≥ 250) | Largest blob | Blobs ≥ 1000 px | Y ≥ 230 |
|---|---|---|---|---|
| 21:12, 0.64 s (room lights on) | 0.41% | 5,504 px (0.045%) | 8 | 0.85% |
| 01:26, 3.5 s (ramp at target) | 2.82% | 56,374 px (0.457%) | 46 | 5.47% |
| 01:27, same shutter, −1.6 EV | 1.19% | 35,214 px (0.286%) | 13 | 2.00% |

**Clip response.** Clipped area roughly doubles per EV. Measured slopes: the
21:22–21:26 sweep gives +1.3 EV → ×2.9 (1.18 doublings/EV). The −1.6 EV frames
give ×2.4 (0.78 doublings/EV).

**Transients.** Excluding the dark frames, the largest single-frame rise over
the neighbouring frames' median was +0.84 percentage points (×1.3). The
median frame-to-frame deviation was 0.07 points. Headlights are small next to
the lamps in this scene.

**Percentiles.** At night p99 = 254 (the lamps) and p95 = 229 (lit lot
surfaces). This confirms the rejection of a p99 target. A clipped-fraction
budget alone bounds the p95 region, so no percentile target is added.

## Decisions (user, 2026-09-23, via the question tool)

| Question | Decision |
|---|---|
| Default budget | **1%** of metered samples (≈ −1.5 EV at night on this scene). |
| When active | **Sun below 0°**; no station → inactive. |
| Back-off | **≤ 1 EV per frame after two consecutive over-budget frames**. Recovery 1/12 EV per frame, only below budget/2. |
| UI | **Under Advanced** on the dashboard (percent, 0 = off). |

Also chosen as defaults, without separate questions: a −3 EV floor for the
guard, and the guard learns only from captured frames, not preview frames
(design §5.6).

## Acceptance criteria

G1. **Darker with lamps, and stable.** A night scene replayed from the measured
    2026-09-22 sequence, with the default 1% budget, converges to an exposure at
    least 1 EV darker than the same replay with the guard off. After
    convergence the simulated clipped fraction stays ≤ 1.5 × budget, and the
    planned exposure moves < 0.2 EV frame to frame (no hunting).
G2. **Transients ignored.** A single frame with 3× the clipped fraction moves
    the planned exposure by 0 EV. A sustained rise is pulled within 3 frames.
G3. **Never brightens.** For any state, observation and sun elevation,
    `H ≤ 0`, and the guarded plan is ≤ the plan with the guard off.
G4. **Daylight unchanged.** With the sun ≥ 0°, no station, or a budget of 0,
    plans are identical to the pre-guard behaviour, even with 5% clipping.
G5. **Pull speed and floor.** A pull is at most 1 EV per frame and is not
    slowed by `max_step_ev`. `H` never goes below −3 EV. Recovery is at most
    1/12 EV per frame, and only below budget/2.
G6. **Robust to exposure faults.** A frame the camera exposed 1.6 EV darker
    than planned does not trigger recovery or a pull by itself.
G7. **Config.** `clip_budget_percent` defaults to 1.0. Older configs without
    the field deserialize to it. `validate` rejects values outside 0..=10 with
    422 through the existing endpoints, and `sanitized` clamps them.
G8. **Preview.** `observe_preview` leaves the guard state unchanged. The live
    plan includes `H` and reports it (`LiveExposurePlan.highlight_ev`). The
    dashboard caption shows it when it is non-zero.
G9. **UI.** An Advanced field "Highlight clip budget (%)" round-trips the
    setting.
G10. `cargo fmt --check`, `cargo test --locked`, `cargo clippy --locked
    --all-targets -- -D warnings`, and `node --test tests/web/*.test.js` pass.

## Test plan (written before implementation)

Rust unit tests in `src/exposure_ramp.rs`:

1. `guard_pulls_after_two_over_budget_frames` (G2, G5): the first frame over
   budget leaves `H = 0`; the second pulls by min(excess, 1 EV).
2. `guard_ignores_a_single_bright_frame` (G2): steady at budget, one ×3
   frame, then steady → planned exposure unchanged.
3. `guard_pull_is_not_slowed_by_the_step_limit` (G5): after a 1 EV pull the
   next plan drops about 1 EV even with `max_step_ev = 1/3`.
4. `guard_floor_and_slow_recovery` (G5): sustained heavy clipping stops at
   −3 EV; clipping below budget/2 recovers 1/12 EV per frame; the hold band
   between budget/2 and budget leaves `H` alone.
5. `guard_never_brightens` (G3): a grid of states, clip fractions, sun
   elevations and budgets → `H ≤ 0` and the guarded plan ≤ the unguarded plan.
6. `guard_is_inactive_in_daylight_without_station_or_with_zero_budget` (G4):
   bit-identical plans versus a guard-free state, with 5% clipping.
7. `guard_ignores_an_exposure_fault_frame` (G6).
8. `preview_frames_leave_the_guard_alone` (G8).
9. `replay_2026_09_22_night_with_and_without_guard` (G1): a closed-loop replay of
   the measured 21:10–22:40 sequence (per frame: measured scene EV and
   clipped fraction at the planned exposure). The simulated clipped fraction
   at another exposure scales as 2^(ΔE), the measured ≈ 1 doubling per EV.
   Guard off reproduces the runaway (≥ 2% clipped). Guard on converges ≥ 1 EV
   darker, clipped ≤ 1.5%, and stable.
10. Settings tests extended: default, round-trip of an old config without the
    field, `validate` bounds, `sanitized` clamp (G7).

Scheduler (`src/optic_scheduler.rs`): the existing `live_exposure_plan` test
is extended to check `highlight_ev` (G8).

Node (`tests/web/scheduled-exposure.test.js`): the caption mentions the
guard only when `highlight_ev` ≤ −0.05 (G8/G9).

Target environment: Mac for all of the above. On the Pi (needs user approval
and a deploy of `main` + this branch; the version is bumped at merge), one
guarded night compared against the 2026-09-22 frames measured here.

## Scope note

This track owns `src/exposure_ramp.rs` and
`docs/optic-daemon-exposure-ramping.md`, plus the minimum in
`src/optic_scheduler.rs` / `src/web/scheduled-exposure.js` to surface the
setting. Surfacing the field also needs one `<label>` in `src/web/index.html`
(outside the listed files, recorded here). No `Cargo.toml` version bump.

## Implementation (2026-09-23)

- `src/exposure_ramp.rs`
  - `RampSettings.clip_budget_percent` (default 1.0, `validate` 0..=10,
    `sanitized` clamps; NaN falls back to 1.0; older configs get the default
    through the existing `#[serde(default)]`).
  - `HighlightGuard { offset_ev, last_pull_ev, previous_excess_ev }` in
    `RampState.highlight`, and `HighlightGuard::next`, which implements the
    pull, hold, recover, floor and gate rules. Also `guard_active(sun)`
    (sun < 0°, never without a station).
  - `plan`: `E_desired = min(target − Ŝ, E_max) + H`. The downward step limit
    is widened by `last_pull_ev`.
  - `observe` takes the shot's `sun_elevation_deg`. The excess is
    `log2(clipped/budget) + (E_planned − E_actual)`. Seeds reset the guard.
  - `observe_preview` leaves the guard unchanged.
- `src/optic_scheduler.rs`: passes the shot's sun elevation to `observe`,
  logs each pull (`highlight guard darkened the ramp`), and adds
  `LiveExposurePlan.highlight_ev`. Two test literals and the
  `live_exposure_plan` test are updated.
- `src/web/scheduled-exposure.js`: `RAMP_DEFAULTS.clip_budget_percent`, the
  Advanced field binding, and `describeHighlightGuard`, used in the caption.
- `src/web/index.html`: the *Highlight clip budget (%)* input under Advanced
  (outside the listed files; see "Scope note").
- `tests/web/scheduled-exposure.test.js`: caption and defaults tests.
- `docs/optic-daemon-exposure-ramping.md`: §3 decision row, §4 field, §5.3
  plan formula, new §5.6, §7 status/log, §9 follow-ups, and the §10 Advanced list.
  The status line was corrected (the ramp *is* deployed as 0.1.31), and a
  stale "white-balance rate" Advanced field was removed from the §10 list.

A defect found during implementation review, before any run: the first
version added `H` *before* the shutter × gain ceiling clamp. In a scene too
dark for the ceiling, the offset would have disappeared into the headroom
above it, so the guard could reach its floor without changing the frame. It
is fixed by applying `H` below the ceiling. Regression test:
`guard_darkens_from_the_exposure_ceiling_in_a_very_dark_scene`; the old
ordering plans `ceiling − 0.67` instead of `ceiling − 1`.

Two test-setup mistakes were fixed during the first run: the plan rounds the
shutter to whole µs (so it is compared with `planned[0]`, and within 1e-5 of
the reference), and the fault test's guarded state now has a matching
guarded anchor. Neither was a guard defect.

## Validation (Mac, 2026-09-23)

| Check | Result |
|---|---|
| `cargo fmt --check` | pass |
| `cargo test --locked` | pass: 256 passed (was 246 before this change; 31 in `exposure_ramp`) |
| `cargo clippy --locked --all-targets -- -D warnings` | pass |
| `node --test tests/web/*.test.js` | pass: 26 tests |
| `npx @biomejs/biome@2.5.14 ci` (CI's pinned version) | pass: 13 files, no fixes |
| Linux/aarch64 compile (vmpi or Pi) | **not run**. No Linux-only code was touched, and the new items are all used on every platform. |
| On-Pi / overnight | **not run** (needs approval and a deploy) |

New tests and what they showed:

| Test | Criterion | Observed |
|---|---|---|
| `guard_pulls_after_two_over_budget_frames` | G2, G5 | Frame 1 at 2.65%: `H = 0`. Frame 2: `H = −1` (capped from 1.41 EV) |
| `guard_pull_is_not_slowed_by_the_step_limit` | G5 | The plan drops 1.0 EV in one frame with `max_step_ev = 1/3` |
| `guard_darkens_from_the_exposure_ceiling_in_a_very_dark_scene` | G5 | The plan is at ceiling − 1 EV with 5 EV of unguarded headroom |
| `guard_ignores_a_single_bright_frame_but_catches_a_sustained_rise` | G2 | One ×3 frame: 12 identical plans. Sustained ×3: third frame > 0.5 EV darker |
| `guard_stops_at_its_floor_holds_in_band_and_recovers_slowly` | G5 | Stops at −3 EV. Holds at 0.7%. +1/12 EV per frame at 0.1%, capped at 0 |
| `guard_never_brightens` | G3 | 2,880 combinations of state, clipping, sun, budget and fault: `H ∈ [−3, 0]`, guarded plan ≤ unguarded plan |
| `guard_is_inactive_in_daylight_without_a_station_or_with_zero_budget` | G4 | Sun +10°, 0°, none, budget 0: plans bit-identical to a no-clip reference at 5% clipped. At dawn a leftover −1.5 EV eases +1/12 EV |
| `guard_judges_clipping_at_the_planned_exposure` | G6 | Frames −1.6/+1/0/−1.6 EV off plan leave `H` at −1 |
| `preview_frames_leave_the_guard_alone` | G8 | Guard state unchanged at 0% and 50% clipped |
| `replay_2026_09_22_night_with_and_without_the_guard` | G1 | See below |
| Settings tests (extended) | G7 | Bounds, NaN, clamp, old-config default |
| `live_exposure_plan` test (extended) | G8 | `highlight_ev` −1.25 reported; 0 while seeding |

**Replay of 2026-09-22 21:10–22:40 (91 measured frames, closed loop; clipped
fraction scaled at 1 doubling per EV).** The numbers came from a temporary
`eprintln!`, which was removed afterwards; the committed test asserts the
bounds:

| | Settled exposure, mean of last 30 frames (log2 µs) | Clipped, settled (every 5th frame printed) |
|---|---|---|
| Measured on the Pi | 21.702 (≈ 3.4 s) | 2.4–2.7% |
| Replay, guard off | 21.703 | 2.54–2.70% |
| Replay, guard on (1%) | 20.267 (≈ 1.3 s), **1.44 EV darker** | 0.92–1.02% |

The test asserts the bounds over all 30 settled frames: clipped ≤ 1.5%, and a
frame-to-frame change < 0.2 EV.

With the guard off, the replay reproduces the Pi's exposure to within
0.001 EV, which supports the replay model. With the guard on, the first pull
lands on the second over-budget frame after the 21:22 switch-off (frame 16,
−1.1 EV). The guard settles within 2 frames and does not hunt. It is never
brighter than the unguarded run at any frame.

## Limitations and risks

- **The model is from one night and one scene.** The 1 doubling-per-EV clip
  response and the constants (1 EV pull, 1/12 EV recovery, −3 EV floor, 0°
  gate, hold band budget/2 to budget) are fitted to 2026-09-22. They need
  checking after a guarded night.
- **The replay is a model, not the camera.** It scales clipping analytically.
  Real bloom also depends on the ISP tone curve and the lens (haze through the
  window glass).
- **The guard learns from captured frames only.** A ramp started from preview
  frames shows the unguarded exposure until two night captures have run.
- **Frames 1.6 EV darker than planned (not caused by this change).** Dozens
  of frames on 2026-09-22 (21:47–05:25) came out about 1.6 EV darker than
  planned at the same requested shutter. The guard now discounts them. The
  cause is unknown: 0.1.31 sidecars have no actual-exposure metadata. The
  capture-exposure-metadata work will show whether the camera used a shorter
  exposure. This is a separate issue and should get its own worklog.
- **Dawn.** At sun −4° the ramp was already at the 4.2 s cap with 4% clipped.
  The guard will darken dawn twilight too; that is intended, but it is
  unobserved.
- The dashboard's Advanced field has not been exercised in a browser
  (it is covered by the pure-function tests and the generic field binding only).

## Proposed overnight comparison (needs user approval)

1. Merge this branch into `main` via a PR. The version is bumped at merge
   (not here). Then deploy the merged build to the Pi. The Pi is shared and
   the daemon is a system service, so the user schedules the deploy and
   restart.
2. Keep the same rule (`every1min`, `master_archive`), the same camera
   position and AutoRamp defaults, so the budget is 1%.
3. The next morning, rerun the scratch measurement on the new frames and
   compare with the 2026-09-22 tables above. Expected at night: clipped ≈ 1%
   (was 2.65%), exposure ≈ 1.2–1.5 s (was 3.1–4.2 s), largest blob ≲ 0.3% of
   the frame (was 0.46%), mean L ≈ 0.016 (about −1.5 EV). Daytime frames
   should match 2026-09-22 frame for frame in exposure behaviour.
4. Check `journalctl -u optic-daemon | grep "highlight guard"` for the pull
   times, and the new sidecar `exposure.meter.clipped_fraction` values.
5. Watch for hunting (the frame-to-frame exposure should stay < 0.2 EV at night)
   and for a dawn that looks too dark.

## User verification

1. Dashboard → Scheduled exposure → Advanced: *Highlight clip budget (%)*
   shows 1. Change it, then Save Settings, and confirm it persists.
2. After an approved deploy, while the guard is active, the caption shows
   "highlight guard −x EV" at night.
3. Review the overnight footage for lamp bloom.

