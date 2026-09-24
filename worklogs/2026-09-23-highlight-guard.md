# Dated Worklog: 2026-09-23 - Highlight Guard (Clipping Budget) for the Exposure Ramp

Status: **implemented, host-tested (Mac), and deployed to the Pi on
2026-09-23 20:27 PDT (branch build, still 0.1.31); overnight validation
pending; not user-accepted.** The test plan was written before any
`src/` edit. On-Pi validation needs the user's approval and a deploy of `main`
plus this branch (see "Proposed overnight comparison"). Committed and opened
as PR #18 (https://github.com/keefo/optic/pull/18) with the user's approval on
2026-09-23; not merged.

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

## Deploy (2026-09-23, user-requested: "now please build and deploy")

- What was deployed: branch `highlight-guard` at `9e81009` (= `main` 3b8cd1a +
  the guard). PR #18 is **not merged**, and the version is **not bumped**, so
  the Pi still reports `0.1.31`. The guard build is identified by the
  `exposure_plan.highlight_ev` field in `/api/status`.
- Command: `scripts/build-deploy-optic-daemon.sh` (full path) from the Mac
  worktree. Exit 0. The script's on-Pi gates all passed: Biome, `cargo fmt
  --check`, `cargo test --locked --all-targets` (4 + 251 passed),
  strict Clippy, and the release build (1 m 34 s). It installed with rollback
  protection and printed `SUCCESS: optic-daemon 0.1.31 is active`.
- The service was stopped for the build from about 20:24 to 20:27:42 PDT, so
  the every-minute timelapse has a gap of about 3–4 frames there. `NRestarts=0`.
- Observed after the restart, read-only:
  - `/api/status`: `exposure_plan.highlight_ev: 0.0` and
    `config.schedule.exposure.clip_budget_percent: 1.0`. The ramp is not
    seeding; it was already seeded by preview frames.
  - Scheduled captures at 20:28:07 and 20:29:06 succeeded and were ramped
    (0.88 s and 0.79 s, gain 1.0, sun −13.9°).
  - The synced sidecar `…1790220546688.log.json` records the actual exposure
    (786,285 µs against 786,907 µs requested) and the daemon's own meter:
    `clipped_fraction` 0.645%. That is inside the hold band (0.5–1%), so
    `H = 0` is the expected state; last night at this hour it was 0.4–0.7%.
- Still to do: the overnight comparison (see above). Pulls, if any, appear in
  `journalctl -u optic-daemon | grep "highlight guard"`.


## Part 2: making the budget visible in the live preview (2026-09-23)

Status: **implemented, host-tested (Mac), deployed to the Pi on
2026-09-23 20:46 PDT (uncommitted working tree, still 0.1.31), and verified
live at night in a browser tab (see "Part 2 on the Pi"); not user-accepted.** The
test plan was written before any `src/` edit for Part 2.

**Why.** After the deploy, the user asked whether the Highlight clip budget
shows in the preview. It did not, or not usefully: the guard moved only on
scheduled captures, once a minute, and never while the scheduler was paused. User: "if a feature can not
preview the see the impact, it is useless." Of the ideas brainstormed, the user chose all four
(question tool): (1) the preview uses the guard's target, (2) a clipping
overlay, (3) a live readout, (4) hold-to-compare. Design:
`docs/optic-daemon-exposure-ramping.md` §5.7.

**Facts checked before designing.** The preview is MJPEG decoded into an `<img>`.
Focus tools already paint an overlay canvas from each decoded frame
(`focus-tools.js`). Every preview frame is already metered on the Pi
(`PreviewFrame.meter`), but only exposure, gain and colour gains reach the
browser (`X-Optic-*` headers, `web.rs::mjpeg_part`). The dashboard polls
`/api/status` every 3 s, and staging an exposure change triggers an immediate
refresh (`app.js` `scheduled-exposure-change`). The preview override is
recomputed when the plan key changes.

**Scope note.** Beyond the files listed for this track, Part 2 also touches
`src/web.rs` (one header), `src/web/app.js` (reading it, plus the compare
button wiring), `src/web/focus-tools.js` (the Clipping tool), and
`src/web/index.html` (buttons and the guard line). Capture behaviour (§5.6
`H`) is unchanged.

### Acceptance criteria

P1. **The budget moves the preview without a capture.** With a fixed ramp
    state (no new frames), changing `clip_budget_percent` changes
    `highlight_target_ev` and `LiveExposurePlan.preview` on the next status
    call.
P2. **Target formula and gates.** `H* = clamp(log2(budget) − ĉ − min(u, E_max),
    −3, 0)`. It is 0 with the sun ≥ 0°, without a station, with budget 0, or
    without a clip estimate. It is never above 0, and `preview` is never brighter than
    `preview_unguarded`.
P3. **What learns.** `ĉ` learns from captures and preview frames, normalised
    by the actual exposure (a frame exposed 1 EV differently with the
    correspondingly scaled clipping gives the same `ĉ`). Preview frames
    still leave `H`, `last_pull_ev`, `previous_excess_ev` and
    `planned_log2_exposure` alone.
P4. **Preview closed loop converges.** With a simulated preview loop, where each
    iteration measures clipping at the preview exposure with a true slope of
    0.8, 1.0 or 1.2 doublings per EV, clipping converges to within ±20% of the budget
    within 8 iterations and stays there. With lamp cores that always clip
    (a floor above the budget), it stops at −3 EV and never goes below it.
P5. **Captures unchanged.** All Part 1 guard tests and the 2026-09-22 replay
    pass unchanged.
P6. **API.** `LiveExposurePlan` gains `preview`, `preview_unguarded`,
    `highlight_target_ev` and `highlight_active`. The JS override uses
    `preview`, falls back to the plan for an older daemon, and uses
    `preview_unguarded` while compare is held. A change of either value or of the
    compare state re-requests the stream.
P7. **Readout.** `X-Optic-Clipped` is on every MJPEG part. The guard line
    formats active, inactive (sun) and off states, and the live clipped
    percentage.
P8. **Overlay.** The *Clipping* tool marks exactly the pixels whose BT.601
    luma ≥ 250 (pure function, tested), and toggles like the other tools.
P9. `cargo fmt --check`, `cargo test --locked`, `cargo clippy --locked
    --all-targets -- -D warnings`, `node --test tests/web/*.test.js`, and
    Biome 2.5.14 `ci` pass.
P10. **On the Pi** (after an approved deploy): changing the budget visibly changes
    the preview brightness at night within a few seconds; hold-to-compare
    shows the bloom; the overlay and readout agree with the frame.

### Test plan (written before implementation)

Rust (`src/exposure_ramp.rs`):

1. `clip_estimate_is_normalised_by_actual_exposure` (P3).
2. `preview_frames_learn_the_clip_estimate_but_not_the_capture_guard` (P3;
   extends the Part 1 preview test).
3. `highlight_target_follows_the_budget_without_new_frames` (P1, P2).
4. `highlight_target_is_zero_when_the_guard_cannot_act` (P2): sun ≥ 0,
   no station, budget 0, no estimate.
5. `preview_plans_bracket_the_capture_plan` (P2, P6): `preview` ≤
   `preview_unguarded`; equal to the plan when `H* = H = 0`.
6. `simulated_preview_loop_converges_to_the_budget` (P4): slopes 0.8, 1.0 and 1.2,
   plus a clipping floor case.
7. The existing Part 1 tests and the replay, unchanged (P5).

Rust (`src/optic_scheduler.rs`): extend the `live_exposure_plan` test for
the new fields (P6). `src/web.rs`: extend the MJPEG part test for
`X-Optic-Clipped` (P7).

Node (`tests/web/`): `describeGuardLine` states (P7); preview override
source selection including compare and fallback (P6); `clippedMask` on
synthetic RGBA with BT.601 threshold edges (P8).

Target environment: the Mac for all of the above, then the Pi at night for P10
(deploy needs user approval).

### Part 2 implementation

- `src/exposure_ramp.rs`: `HighlightGuard.clip_scene_ev` (the smoothed,
  budget-independent clip estimate), learned from every frame by `observe`
  and `observe_preview`. It survives a budget-0 guard reset and is floored at
  1e-4. `highlight_target_ev` (`H*`). `ExposureSetting`, `PreviewPlans`,
  `preview_plans` (`preview = clamp(E_plan − H + H*)`, `unguarded =
  clamp(E_plan − H)`). `plan` is refactored onto `ExposureLimits` and
  `unguarded_log2_exposure` with the same arithmetic; all Part 1 tests and
  the replay pass unchanged. The capture guard's `H` logic is untouched.
- `src/optic_scheduler.rs`: `LiveExposurePlan.highlight_target_ev`,
  `highlight_active`, `preview` and `preview_unguarded`.
- `src/web.rs`: the `X-Optic-Clipped` MJPEG part header.
- `src/web/app.js`: `frameMetadata().clippedFraction`.
- `src/web/scheduled-exposure.js`: `previewSource` (target, compare, or a
  fallback for an older daemon) feeds the preview override. The plan key also
  covers the preview plans. `describeGuardLine` renders the guard line,
  refreshed at most every 500 ms from preview frames. Hold-to-compare works
  with the pointer (captured) and with Space/Enter. The Part 1
  `describeHighlightGuard` caption fragment is replaced by the guard line.
- `src/web/focus-tools.js`: the `clippedMask` pure function and the
  *Clipping* toggle, drawing zebra stripes on the shared overlay canvas.
- `src/web/index.html` and `styles.css`: the Clipping button, the guard
  line and the compare button.
- Docs: `docs/optic-daemon-exposure-ramping.md` §5.6 (a pointer to the new
  section), the new §5.7, and a §10 bullet. `docs/optic-daemon-focus-tools.md`
  gets the controls list, the function table and a new §3.3.1.

A mistake caught in review, before any run: the first draft of the
`live_exposure_plan` test extension had a tautological assertion. It was
replaced with the real property (no clip estimate → `preview ==
preview_unguarded`).

### Part 2 validation (Mac)

| Check | Result |
|---|---|
| `cargo fmt --check` | pass |
| `cargo test --locked` | pass: 262 (Part 1 had 256; `exposure_ramp` 36, all Part 1 tests unchanged, replay included) |
| `cargo clippy --locked --all-targets -- -D warnings` | pass |
| `node --test tests/web/*.test.js` | pass: 29 |
| `npx @biomejs/biome@2.5.14 ci` | pass: 13 files, no fixes |
| Browser check of the new controls | **not run**: there is no local camera, so it needs the Pi |
| P10 on the Pi at night | **not run**: needs deploy approval |

New or changed tests: `clip_estimate_is_normalised_by_actual_exposure`
(P3), `highlight_target_follows_the_budget_without_new_frames` (P1/P2: 1% →
−1.406 EV and 0.5% → −2.406 EV at 2.65%; 0.1% hits the floor; the preview moves
1.406 EV between budgets with no new frame),
`highlight_target_is_zero_when_the_guard_cannot_act` (P2),
`preview_plans_bracket_the_capture_plan` (P2/P6),
`simulated_preview_loop_converges_to_the_budget` (P4: slopes 0.8/1.0/1.2 ×
shortfall 0/0.8 EV all within ±20% of the budget from iteration 8 to 20;
2% lamp-core floor → stops at −3 EV), `preview_frames_leave_the_guard_alone`
(now also checks that the estimate learns), `live_exposure_plan` (P6),
`mjpeg_part_carries_the_clipped_fraction` and the header in the existing
part test (P7). Node: `previewSource`, `describeGuardLine` (P6/P7), and
`clippedMask` threshold edges at Y = 248.7 and 250.4 (P8).

### Part 2 limitations

- The preview target and the timelapse `H` converge to the same goal by
  different routes (a solver versus rate-limited steps with a hold band), so they can differ by
  up to about the hold band (≈ 1 EV) until clipping settles. The guard line shows both.
- The preview meters at lower resolution than a still, which is not yet
  measured on the Pi (P10). If the preview reads systematically lower, the
  target will be too bright relative to the timelapse.
- The overlay runs on the ≤ 960 px working copy. The daemon's number is
  authoritative.
- As before, every status poll that changes the preview plan re-requests
  the stream, and the target adds one more source of such changes.

### Part 2 on the Pi (2026-09-23, user-approved "deploy now + verify live")

**Deploy.** Ran `scripts/build-deploy-optic-daemon.sh` from the worktree
(uncommitted Part 2 changes). Exit 0. On-Pi gates: Biome, fmt, `cargo test`
(257 passed in the main binary), strict Clippy, release build. It printed
`SUCCESS`. The daemon was down from 20:43:46 to 20:46:40 PDT, so the
timelapse lost about 3 frames. Afterwards `/api/status` exposed `highlight_active`,
`highlight_target_ev`, `preview` and `preview_unguarded`. Scheduled captures
continued (for example 20:51:07, success).

**Live check** (Chrome automation tab on `http://optic.local:8000/`, sun
about −17°, Master Archive preview downsampled to 8 fps):

| P | Observed |
|---|---|
| P7 header | A raw MJPEG part carried `X-Optic-Clipped: 0.004496…` (0.45%) |
| P7 readout | Guard line: "Highlight guard · budget 1.00% · clipped now 0.53% · preview 0.0 EV · timelapse 0.0 EV". A target of 0 is correct: tonight is under the 1% budget |
| P1 budget → preview | Setting 0.25% in the Advanced field moved `highlight_target_ev` to −0.96 EV and `preview` to 0.356 s (against 0.692 s unguarded) within 1.5 s, with no captured frame in between |
| P10 convergence | The live readout then showed clipped 0.23–0.30% at a 0.25% budget, and the target settled at −0.69 to −0.81 EV as the scene changed. The room-light reflections visible in the window went on and off during the test, and the unguarded exposure moved from 0.69 s to 1.07 s |
| P6/P10 compare | Space held on *Hold to compare*: `aria-pressed=true`; the camera's frames went to 1.58× the exposure (+0.66 EV), clipped 0.27% → 0.61%. After release: `aria-pressed=false`, and the frames returned to the guarded side (0.40 s, 0.16%) |
| P8/P10 overlay | *Clipping* on: red zebra marks on the store entrance, the parking-lot floodlights and the lamp posts, the areas that bloomed on 2026-09-22 |

**Findings and caveats from the live check:**

- **The automation tab was hidden** (`visibilityState: hidden`, 0 rAF
  callbacks/s). `renderMjpegFrame` awaits two animation frames before calling
  the frame hooks, so in a hidden tab the guard line, the overlay and the
  clipped readout only updated while a screenshot was being taken. The hidden tab's timer
  throttling also delayed the 3 s status poll. That explains an observed gap
  of 0.8× between the planned preview and the frames' actual exposure: the UI's
  override (535k µs) was about 0.96× the latest plan (559k µs), and the rest
  was stream lag. This is existing dashboard behaviour, not caused by this change. It is
  **not verified that a visible dashboard tracks the plan within one poll**;
  that is left to the user.
- **The pointer path of hold-to-compare was not exercised** (only the keyboard
  path). A review of it found that `setPointerCapture` ran *before*
  `setComparing(true)`, so a refused capture would have stopped compare
  from engaging. **Fixed after the deploy** (compare first, capture as best effort).
  Node 29/29 and Biome pass. **This fix is not deployed.**
- The budget was restored to exactly 1% afterwards. The staged exposure settings are
  identical to before (`config_staged` was already true from edits that
  aren't ours). Closing the automation tab stopped the camera preview through
  `pagehide` → `/api/stream/stop`. Scheduled captures are unaffected.
- A probe of a nonexistent `/api/config` route logged one harmless `ERROR
  failed to load web asset … api/config` at 20:51:15.
- The preview-to-still clipped-fraction ratio (a Part 2 limitation) is still
  unmeasured.


## Part 3: Scheduled exposure preview hunts, and its colour shifts (2026-09-23)

Status: **implemented, host-tested (Mac), deployed to the Pi on 2026-09-23
23:52 PDT (uncommitted working tree, still 0.1.31), and verified on the camera
for Q7 (see below); not user-accepted.** The test plan was written before any
`src/` edit for Part 3.

**Report (user, about 21:55).** With *Scheduled exposure* on, the live preview's
colour keeps shifting "like WB is changing": warmer when the Shutter field
drops to about 600 ms, cooler at 700–800 ms. Unchecking Scheduled exposure
stops it. The user identified it as a Scheduled exposure bug, not a
clip-budget one, and chose to fix it on this branch.

**Evidence** (a 40 s recording of the live MJPEG stream as a second
read-only viewer; 319 frames; Master Archive preview downsampled to 8 fps,
AWB preset Daylight, Denoise Auto, sun −27°):

- **Not white balance.** `X-Optic-Colour-Gains` was identical in all 319
  frames (3.2061546, 1.4414065).
- **The exposure never settles.** Shutter was fixed at 118,745 µs (frame-time
  limited), and analogue gain took **40 distinct values between 5.22 and
  6.87** (0.40 EV peak to peak, sd 0.12 EV). **86 control revisions in 40 s**:
  the dashboard re-requested the stream about every 0.47 s.
- **Colour follows gain.** Frame R/G ranged 1.08–1.41 and B/G 0.40–0.49,
  with corr(gain, B/G) = 0.81 and corr(gain, R/G) = 0.67. Higher gain looked
  cooler, which matches the report. The shift is in the shadows: R/G of the
  darker half ranged 0.98–1.76 (sd 0.18), while the brightest 5% held at
  1.04–1.14 (sd 0.02).
- **Root cause of the hunting.** The per-frame scene estimate `log2 L − log2(e·g)`
  was not exposure-invariant at these gains: corr(scene_ev, log2 e·g) =
  **0.93**, with a range of 0.48 EV over 0.40 EV of exposure (≈ 1.2 EV of
  apparent scene change per EV of gain). A higher preview gain makes the scene
  read brighter, so the plan asks for less, and the reverse. Preview frames update
  the estimate at frame rate with weight 1 − smoothing = 0.5, so the loop
  gain is above 1 and it oscillates.
- **An amplifier.** `onStatus` calls `notifyChange` whenever the plan's
  shutter/gain (Part 2: also the preview plans) changes, and `app.js`'s
  `scheduled-exposure-change` handler calls `refreshStatus()` straight away.
  The new plan differs again, so the dashboard re-requests at network round-trip rate
  instead of once per 3 s poll. This loop is in the PR #16 code (the key
  was `[seeding, shutter_us, gain]`); Part 2 only added fields to the key.
- Why the colour moves with gain: most likely the sensor/ISP rendering of
  near-black at different analogue gains (noise floor, black level, gain-
  dependent colour denoise). This has not been isolated. With a stable exposure it no
  longer flickers; it only changes when the exposure really changes.
- Stills are not affected in the same way: on 2026-09-22 the captured scene estimate
  held at about −26.2 across 3.1–4.2 s at gain 1.

### Acceptance criteria

Q1. **Time-based preview learning.** `observe_preview` blends the scene and
    clip estimates by `1 − exp(−Δt / 5 s)`, where Δt is the time since the state
    was last updated (clamped to 0 or more). A burst of frames at 8 fps moves the
    estimate by the same amount as one frame after the same total time, within 1%.
    Captures (`observe`) keep per-frame `smoothing` and are unchanged.
Q2. **The loop converges.** A simulation with the measured bias (the scene
    reads 1.2 EV brighter per EV of preview exposure), a 3 s poll and
    8 fps frames settles: after 60 s the planned preview exposure moves less than 1/6 EV
    between polls, and the peak-to-peak swing over the last 30 s is below 1/6 EV. The
    same simulation with the old per-frame learning keeps a swing of ≥ 1/3 EV
    (a regression guard that shows the test detects the bug).
Q3. **No re-request storm.** A plan-driven change re-requests the stream
    without an immediate extra status fetch. User edits (the toggle,
    settings) still refresh status immediately, so a budget change still shows
    within about 1 s.
Q4. **Hysteresis.** A new preview exposure is sent (a controls-only
    `/api/stream/reconfigure`, with no pipeline restart) only when the preview source
    changes seeding state, or its exposure differs from the last one sent by more
    than 1/6 EV. Compare press and release always send.
Q5. All Part 1 and Part 2 tests pass; some Part 2 tests only need their frame
    timestamps advanced, and no assertion is weakened.
Q6. fmt, test, clippy, Node and Biome pass.
Q7. **On the Pi, at night** (needs a deploy): over 40 s of recorded preview,
    control revisions ≪ 86 and the gain spread ≪ 0.40 EV, with no visible colour
    pumping.

### Test plan (written before implementation)

- Rust: `preview_learning_is_time_based` (Q1),
  `preview_loop_with_gain_dependent_metering_settles` (Q2, including the
  old-behaviour guard), and updated Part 1/2 preview tests (Q5).
- Node: `previewNeedsUpdate` (Q4). The event split (Q3) is covered by code
  review and the on-Pi recording (Q7): it is browser wiring without a pure seam.
- Pi: repeat the 40 s MJPEG recording and analysis after an approved deploy
  (Q7).

### Part 3 implementation

- `src/exposure_ramp.rs`: `PREVIEW_LEARNING_TIME_CONSTANT_S = 5.0`.
  `observe_preview` blends the scene and clip estimates by `1 − e^(−Δt/τ)`, with Δt
  measured from `state.updated_at` and clamped to 0 or more. `observe`
  (captures) is unchanged.
- `src/web/scheduled-exposure.js`: `previewNeedsUpdate` with
  `PREVIEW_HYSTERESIS_EV = 1/6`. `previewOverride` records the source it
  sends (`lastSentSource`). `onStatus` re-requests the stream only when
  `previewNeedsUpdate` says so, through the new preview-only event.
  `setComparing` uses the same event. The unused `lastPlanKey` is removed.
- `src/web/app.js`: a `scheduled-exposure-preview-change` listener
  (`controlRevision += 1; schedulePreviewUpdate()`, with no `refreshStatus`).
- Tests: `preview_learning_is_time_based`,
  `preview_loop_with_gain_dependent_metering_settles` (new).
  `preview_frames_seed_the_ramp_and_keep_the_capture_step_anchor`,
  `preview_frames_leave_the_guard_alone` and the Part 2 `preview_loop` helper
  now advance time explicitly (1 s, 2 s, and one iteration per 3 s poll).
  Their assertions use the time-based blend; none were loosened. Node:
  `previewNeedsUpdate`.
- Docs: `docs/optic-daemon-exposure-ramping.md` gets the §5.7 learning formula,
  a §11 bullet (which also drops a stale "eases white balance"), and a new §11.1.

Test mistake found on the first run: an assertion claimed that 700 → 600 ms (0.22 EV) is
below the 1/6 EV hysteresis. The test was wrong, so it now checks 650 → 600 ms
(0.12 EV, no update) and 700 → 600 ms (update).

### Part 3 validation (Mac)

| Check | Result |
|---|---|
| `cargo fmt --check` | pass |
| `cargo test --locked` | pass: 264 (`exposure_ramp` 38) |
| `cargo clippy --locked --all-targets -- -D warnings` | pass |
| `node --test tests/web/*.test.js` | pass: 30 |
| Biome 2.5.14 `ci` | pass: 13 files |
| Q7 on the Pi | **not run**: needs a deploy |

Simulated loop (the measured bias of 1.2 EV per EV, 8 fps, 3 s poll, 40 polls;
numbers from a temporary `eprintln!`, removed afterwards):

| Learning | Peak-to-peak swing, last 10 polls |
|---|---|
| Old: per frame, weight 0.5 | **0.667 EV**, a sustained limit cycle alternating 21.19 ↔ 21.86 |
| New: time-based, τ = 5 s | **0.000 EV**, settled at the correct exposure |

### Part 3 limitations

- The per-gain shadow colour itself is not fixed. It is sensor and ISP rendering at
  high analogue gain, not isolated (a fixed-gain test at 5× and 7× would
  isolate it). It now changes only when the exposure really changes.
- While a preview runs, its frames still teach the scene estimate that the next
  scheduled capture plans from, and the high-gain preview metering is biased
  relative to stills at gain 1. The size of that bias on the timelapse is not
  measured. It predates this work (§11), but Part 3 makes it visible.
- The event split (Q3) has no unit test. It is verified by review and needs
  the Q7 recording.

### Part 3 on the Pi (2026-09-23, user-approved "deploy now + re-record")

**Deploy.** `scripts/build-deploy-optic-daemon.sh`, exit 0. On-Pi: 259 tests
in the main binary, clippy, release build, `SUCCESS`. The daemon was down from 23:49:53 to
23:52:45. The served `scheduled-exposure.js` contains `previewNeedsUpdate`.

**Q7 recording.** The user reloaded a visible dashboard, with Scheduled exposure on and
the preview started. 60 s of MJPEG was recorded as a read-only viewer and analysed
with the same script as the baseline (scratchpad `stream_stats.py`):

| | Before (21:56, 40 s) | After (23:53, 60 s) |
|---|---|---|
| Control revisions | 86 | 2 (one of them the stream restart after a capture) |
| Distinct exposures while live | 40 (gain 5.22–6.87, 0.40 EV) | **1** (119 ms × 12.19 for all 196 live frames) |
| Frame R/G | 1.08–1.41, sd 0.092 | 1.18–1.22, **sd 0.010** |
| Frame B/G | sd 0.025 | sd 0.005 |

The colour pumping is gone while the preview is live. The preview ran 119 ms × 12.19 ≈ 1.45 s,
which matches the plan (3.44 s) at the guard's target (−1.29 EV).

**New finding: preview frame rate (user: "preview frame rate should be
consistent").** The recording's second segment was two frames at 3,474,267 µs
× 1.0 (sequence reset to 0). The journal (23:53–23:56) shows why:

- Every scheduled capture runs `stop_existing_preview`, then a still pipeline
  of 11 warm-up frames at the capture's exposure: `warmup_and_capture` took
  25,072 ms and 22,758 ms at about 3.5 s night frames. The preview gets **no frames for about
  23–25 s every minute**.
- The preview pipeline then restarts, and its first frames still carry the
  still's exposure (3.47 s): libcamera applies new controls a few frames late, so there are
  about 7 more seconds at about 0.3 fps.
- At a 1-minute interval with a night frame of about 3.5 s, the preview is live for only about half of
  each minute. This predates Parts 1–3 (captures always stopped the preview).
  It is more visible tonight because the night exposure is long (at 20:51, 0.84 s:
  6.6 s per capture).
- The user also questioned "re-request the stream". An exposure-only change takes
  `ReconfigureStream`'s controls-only path (`native_camera.rs`: settings and
  revision updated, no pipeline restart), so it costs no frames. The worklog
  and design text are corrected to say "exposure update".

Candidate fixes (not implemented; design doc §9 already lists the first two
as follow-ups): pass the preview controls to `camera.start()` so the first
frames after a capture use the preview exposure; shorten the 11-frame warm-up
for fully manual stills (needs an on-Pi test); longer term, a combined preview and still
configuration that never stops the preview.

