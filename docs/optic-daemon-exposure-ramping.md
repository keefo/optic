# Design: Exposure Ramping ("Holy Grail" Day-to-Night) for Scheduled Captures

**Component:** `src/exposure_ramp.rs` (pure logic), wired into
`optic_scheduler` scheduled captures and `native_camera` still capture.
**Status:** The ramp (§5.1–§5.5) is deployed: the Pi's 0.1.31 ran it in
`AutoRamp` with default settings through the night of 2026-09-22 (observed via
`/api/status` and the capture sidecars, 2026-09-23). It is hardware-observed
but not user-accepted; see `worklogs/2026-09-21-exposure-ramping.md`. The
highlight guard (§5.6) is implemented and unit-tested but **not deployed and
not hardware-validated**; see `worklogs/2026-09-23-highlight-guard.md`.

## 1. Goal

A scheduled timelapse that runs through sunset or sunrise should come out
smooth: no sudden exposure jumps, no per-frame white-balance flicker, and
a night that still reads as night. The camera has no AE loop running during
a ramped capture. Instead, each captured frame is measured, and the next
frame's exposure is planned from that measurement, a target brightness that
follows the sun, and per-frame step limits.

## 2. What the Hardware Lets Us Control

| Control | libcamera control | Ramped? |
|---|---|---|
| Shutter | `ExposureTime` (manual) | Yes, first |
| Analogue gain | `AnalogueGain` (manual) | Yes, after shutter reaches its limit |
| White balance | `AwbEnable(false)` + `ColourGains`, or AWB modes | No — the operator's own setting (§6) |
| Aperture | none (manual 6 mm f/1.2 CS lens) | No |
| Focus | none (manual) | No |

## 3. Decisions (user, 2026-09-21)

| Topic | Decision |
|---|---|
| Max shutter | Keep the existing 5 s validation cap. The f/1.2 lens gathers at 5 s what an f/2.4 lens gathers at 20 s. Longer needs a separate hardware test. |
| Night look | **Brightened night.** The target stays at the day level down to sun +6°, then eases 2 EV darker by −18° (astronomical dusk). |
| Scope | **Global only.** One exposure mode for all scheduled captures: `Dashboard` (unchanged behaviour) or `AutoRamp`. No per-rule presets (§3.1). |
| White balance | **Not part of the ramp** (revised 2026-09-22, twice). Scheduled captures use the dashboard's own White balance control, exactly like a manual capture. |
| Metering | **Trimmed log-mean luma** over a 1/16 sample grid, darkest/brightest 2% dropped. |
| Highlight guard (2026-09-23) | **A clipping budget, not highlight protection** (§5.6): 1% of metered samples may clip; above that the ramp backs off. Active only with the sun below 0°. Pulls ≤ 1 EV per frame after two over-budget frames; recovers 1/12 EV per frame. Budget under *Advanced* on the dashboard. |

### 3.1 Why there are no per-rule exposure presets

The scheduler merges rules whose occurrences fall within 10 s of each other
into one physical capture tagged with every contributing slug
(`docs/optic-daemon-scheduler.md` §3.1). One frame has exactly one exposure,
so per-rule ramps would put an off-exposure frame into every losing rule's
sequence, which is the flicker ramping is meant to remove. Dedicated
timelapse controllers (e.g. Timelapse+ VIEW, qDslrDashboard) treat exposure
the same way: one continuous ramp per camera with a few global knobs (night
compensation, max shutter, max ISO), while the "program" only decides when
to shoot. Scene brightness is the same whichever rule fired, so a single
ramp state fed by every captured frame fits Optic's merge model exactly.

Per-rule fixed overrides (e.g. a moon rule with a short fixed exposure) were
considered and declined for now. They can be added later as an optional rule
field without changing the ramp.

## 4. Configuration

A new field on `ScheduleConfig` (`AppConfig.schedule.exposure`). It is staged
from the dashboard with `POST /api/schedule/exposure` (§10) and committed by
Save Settings (`POST /api/config/commit`). `POST /api/schedule/preview`
accepts it too, but the Scheduler page no longer sends it, so staging rules
there leaves it unchanged. A config written before this field existed
deserializes as `Dashboard`.

```json
"exposure": { "mode": "Dashboard" }

"exposure": {
  "mode": "AutoRamp",
  "min_shutter_us": 100,
  "max_shutter_us": 5000000,
  "max_gain": 8.0,
  "day_bias_ev": 0.0,
  "night_drop_ev": 2.0,
  "max_step_ev": 0.333,
  "smoothing": 0.5,
  "clip_budget_percent": 1.0
}
```

| Field | Range | Meaning |
|---|---|---|
| `min_shutter_us` | 100 ..= `max_shutter_us` | Shortest shutter the ramp uses. |
| `max_shutter_us` | ..= 5,000,000 | Longest shutter (the `CameraSettings` cap). |
| `max_gain` | 1 ..= 16 | Highest analogue gain. |
| `day_bias_ev` | −3 ..= 3 | *Brightness compensation* in the UI: shifts the whole curve (day **and** night), in EV relative to 18% grey. The config key keeps its original name so saved configs stay valid. |
| `night_drop_ev` | 0 ..= 6 | How much darker the night target is than the day target. |
| `max_step_ev` | 0.05 ..= 2 | Largest exposure change between two consecutive ramped frames. |
| `smoothing` | 0 ..= 0.95 | Weight of the previous scene estimate (0 = react fully to each frame). |
| `clip_budget_percent` | 0 ..= 10 | Highlight guard (§5.6): the percentage of metered samples allowed to clip (luma ≥ 250) with the sun below 0°. 0 turns the guard off. A config written before this field existed gets the default, 1%. |

`POST /api/schedule/exposure` (and `/api/schedule/preview`) reject
out-of-range values with 422. At run
time the scheduler clamps any hand-edited value into range instead of
failing captures.

## 5. The Ramp Loop

Everything below is in `src/exposure_ramp.rs` as pure functions with no
camera or clock dependency. The unit tests drive it with synthetic frames
and synthetic sun elevations.

### 5.1 Metering (`meter_yuv420`)

The still stream is YUV420 (full-range BT.601, sYCC). For every 4th pixel
of every 4th row:

1. Luma `Y` goes into a 256-bin histogram.
2. When `16 ≤ Y ≤ 235` (neither crushed nor clipped), `Y/U/V` is converted
   to RGB and linearized (γ 2.2); the linear R, G, B sums feed the
   grey-world estimate.

From the histogram, the darkest 2% and brightest 2% of samples are dropped,
and the rest are averaged in the log domain after linearizing each bin:
`L = exp(mean(ln((Y + 0.5) / 255)^2.2))`. `L` is a linear luminance in
0..1, where 0.18 is photographic mid-grey. The meter also reports the
fraction of samples at `Y ≥ 250` (clipped).

### 5.2 Target brightness (`target_bias_ev`)

```
bias(elev) = day_bias_ev                                  elev ≥ +6°
           = day_bias_ev − night_drop_ev                  elev ≤ −18°
           = day_bias_ev − night_drop_ev · s(t)           otherwise
  t = (6 − elev) / 24,   s(t) = 3t² − 2t³  (smoothstep)
target_luminance = 0.18 · 2^bias
```

The sun elevation is computed from `ScheduleConfig.station` at the shot's
time (`ephemeris::sun_equatorial`/`elevation_deg`). Without a station the
curve stays at the day level.

### 5.3 Scene estimate and next exposure (`RampState::plan`, `RampState::observe`)

Exposure is handled as a log2 exposure product `E = log2(shutter_us × gain)`.
Each ramped frame reports the **actual** exposure and gain from the
completed request's metadata (not the requested values), so control latency
or clamping cannot mislead the loop.

- **Observe:** the frame's scene brightness is `S = log2(L) − E_actual`.
  The smoothed estimate is `Ŝ ← Ŝ + (1 − smoothing)(S − Ŝ)`.
- **Plan:** `E_desired = min(log2(target_luminance) − Ŝ, E_max) + H`, where
  `E_max = log2(max_shutter_eff × max_gain)` and `H ≤ 0` is the highlight
  guard's offset (§5.6; 0 when it is inactive, which makes the `min` a no-op
  because of the final clamp). It then moves at most
  `max_step_ev` from the previous frame's planned `E` (downward, plus whatever
  the guard just pulled), and is clamped to
  `[log2(min_shutter), log2(max_shutter_eff × max_gain)]`.
- **Split (shutter first, gain last):** `shutter = clamp(2^E, min, max_eff)`,
  `gain = clamp(2^E / shutter, 1, max_gain)`. When darkening, gain falls to
  1 before the shutter shortens. This matches the usual rule of adding gain
  last and removing it first.

### 5.4 Seeding and reseeding

With no ramp state (daemon start, or switching to `AutoRamp`), or when more
than 30 minutes have passed since the last ramped frame, the next capture is
a **seed frame**. It uses libcamera auto exposure and AWB (shutter 0,
gain 0, `awb: auto`), as `Dashboard` defaults do. Its measurement
initializes `Ŝ`. The first planned frame after a seed
may jump straight to the desired exposure (no step limit), because there is
no smooth sequence to protect yet. Every frame after that is step-limited.
The seed frame is a normal output frame.

Ramp state lives only in the scheduler actor's memory. A daemon restart
reseeds, which is intended: after a restart, the last exposure may be stale.

### 5.5 Interval budget (`max_shutter_for_gap`)

A still capture currently waits for about 11 frames (3 queued requests + 8
warmup, `native_camera.rs` `CAPTURE_WARMUP_FRAMES`), and each frame lasts at
least the shutter time. At 5 s that is about 55 s per capture. The
scheduler therefore caps the shutter for each shot using the gap to the next
scheduled shot:

```
max_shutter_eff = min(max_shutter_us,
                      (0.8 · gap − 1.5 s) / 11)       (never below min_shutter_us)
```

Example: a 30 s night interval caps the shutter at about 2.0 s, and a
1-minute interval at about 4.2 s. Shortening the warmup for fully manual
ramped frames would remove most of this cost, but changes camera behaviour,
so it needs an on-Pi test first (§9).

### 5.6 Highlight guard: a clipping budget (2026-09-23)

**Problem.** §5.1 meters by dropping the brightest 2% and log-averaging the
rest, so a dark scene with lamps is exposed until its *dark* parts reach the
night target, whatever that does to the lamps. §5.1 already measures the
clipped fraction; before this section nothing used it. On the night of
2026-09-22 (ramp running, default settings,
`worklogs/2026-09-23-highlight-guard.md`) the night target was met
exactly (trimmed mean 0.045 = 0.18 · 2⁻²), and 2.65% of samples clipped. At
full resolution the largest blown area covered 0.46% of the frame, and
Y ≥ 230 covered 5.5%. When room lights reflected in the window went off at
21:16 and 21:22, the metered scene fell 2.2 EV. The ramp opened from 0.64 s to
3.1 s and the clipped fraction went from 0.35% to 2.5%, with the largest blob
about 10× larger.

**Why a budget and not "protect the highlights".** Highlight-priority
exposure is right for *diffuse* highlights (walls, cloud, snow), because
clipping them is irreversible. Street lamps, signs and lit windows are
*emissive*, and good night footage lets their cores clip. What looks wrong is
the *size* of the blown area and its haze, which grows with exposure. A
99th-percentile target was rejected: on this scene p99 is 254 (the lamps
themselves), so it would underexpose everything. The 95th percentile at night
sits at 229 (lit lot surfaces), and a clipped-fraction budget alone already
bounds that, so no percentile is used.

**Rule.** Each captured frame gives an *excess*:

```
excess = log2(max(clipped_fraction, 1e-6) / budget) + (E_planned − E_actual)
```

The second term moves the measurement to the planned exposure, so a frame
the camera exposed differently from the plan (for example the 1.6 EV-dark
frames seen on 2026-09-22) cannot mislead it. It assumes clipped area roughly
doubles per EV; the measured response is 0.8–1.2 doublings per EV. The state keeps a
guard offset `H ≤ 0` (EV) and the previous frame's excess.

- **Pull (fast):** when *both* this frame's and the previous frame's excess
  are above 0, `H ← H − min(min(excess, previous_excess), 1 EV)`. A single
  bright frame (a passing headlight: at most +0.8 percentage points measured)
  never moves the exposure. A real change is corrected within 2–3 frames.
- **Hold:** between budget/2 and budget, `H` is unchanged. This hysteresis
  band is about 1 EV wide, so the guard does not hunt.
- **Recover (slow):** below budget/2, `H ← min(0, H + 1/12 EV)`.
- **Floor:** `H ≥ −3 EV`, so something that always clips (a lamp shining
  into the lens) cannot black out the night.
- **Gate:** with the sun at or above 0°, or no station, the guard never pulls
  and `H` recovers at the same slow rate. Daytime clipping (1–7% of samples on
  2026-09-22, from sky and glass) is left alone. Between +2° and −3°
  clipping measured under 0.1%, so the gate switches while it has nothing to do.
  Budget 0 disables the guard and sets `H = 0`.

**Plan.** `E_desired = min(log2(target) − Ŝ, E_max) + H`. The offset applies
below the shutter × gain ceiling, so in a scene too dark for the ceiling it
still darkens the frame actually shot instead of disappearing into the
headroom above it. The per-frame step limit still applies, but downward it is widened by the amount the guard just
pulled, so a pull is not slowed down to `max_step_ev`. Upward it is unchanged,
so recovery is never faster than the ramp. Because `H ≤ 0`, and the
clamps are monotonic, the guarded plan is never brighter than the unguarded
one. The guard can only darken, and the §5.2 night curve stays.

**What feeds it.** Only captured frames (`observe`). Preview frames
(`observe_preview`, §11) leave `H` and the previous excess untouched, like
`planned_log2_exposure`. They run at frame rate and at a different
shutter/gain mix, and the per-frame rates above are defined per captured
frame. The live plan and the preview still *include* the current `H`. A seed
(start, or a reseed after 30 min) starts with `H = 0`.

**Expected effect with a 1% budget.** On the 2026-09-22 night, frames at −1.6 EV
clipped 1.2% with a largest blob of 0.29%, so the guard should settle about
1.5 EV below the unguarded night exposure (≈ 1.2 s instead of 3.5 s at a
1-minute interval). On-Pi validation is pending (see the worklog).

## 6. White Balance Is Not Ramped (revised 2026-09-22)

The ramp controls exposure only. Scheduled captures use whatever the
dashboard's **White balance** control says — the same control a manual
capture uses — and that field stays editable while *Scheduled exposure* is
on.

Two earlier designs were tried and dropped:

1. **Eased grey-world:** colour gains seeded from AWB and nudged toward
   neutral each frame. Grey-world pulls a strongly coloured scene toward
   grey, which removes the sunset the timelapse exists to capture. It also
   drifted visibly once live preview frames fed the loop at frame rate
   (§11).
2. **Locked gains:** seed once from AWB, then hold. That fixed the drift but
   still replaced the operator's choice with a number the camera happened to
   pick at an arbitrary moment.

What "locked" should mean is *the operator's setting, held*: choose a fixed
preset (Daylight, Cloudy, …) and every scheduled frame uses it, which also
removes the per-frame AWB flicker that motivated the original design.
Leaving the control on Auto keeps libcamera's per-frame AWB, flicker
included — the operator's choice to make. The UI note says so.

`CameraSettings.colour_gains` remains as a manual-white-balance capability
(`AwbEnable(false)` + `ColourGains`), but nothing sets it today; the preview
override carries exposure only.

## 7. Integration

- `CameraSettings.colour_gains: Option<[f32; 2]>` (new, default `None`).
  `Some` means `AwbEnable(false)` + `ColourGains`; `None` keeps today's
  `AwbEnable(true)` + `AwbMode`. It is validated to 0.5..8.
- `CaptureResult.exposure: Option<CaptureExposure>` (new). Native still
  captures report the frame's actual exposure, gain and colour gains, plus
  the §5.1 meter. The same data appears in the `/api/capture` JSON response.
- `optic_scheduler::fire_capture` builds the request from the dashboard
  settings (profile, rotation, flips, denoise, DNG preference unchanged) and,
  in `AutoRamp`, overrides `shutter_us`, `gain`, `awb` and `colour_gains`
  with the plan. The capture-history entry records these ramped settings.
- `SchedulerStatus.exposure` (new, in `GET /api/status` → `schedule`) shows
  the mode, the last planned shutter/gain/colour gains, target bias, sun
  elevation, measured scene EV, and whether the last frame was a seed.
- `GET /api/status` → `exposure_plan.highlight_ev` (2026-09-23) reports the
  highlight guard's current offset (§5.6), and the dashboard caption shows
  "highlight guard −x EV" while it is darkening. Each pull is also logged
  (`highlight guard darkened the ramp`, with the clipped fraction, pull and
  offset).
- **Dashboard UI (2026-09-22 redesign, §10):** a *Scheduled exposure*
  toggle below Shutter on the dashboard's camera controls. The Scheduler page
  only shows the ramp status read-only.
- Manual dashboard captures and the live preview are unchanged.

## 8. Failure Handling

| Case | Behaviour |
|---|---|
| Capture fails | Ramp state unchanged. The next shot plans from the last good state (or reseeds after 30 min). |
| No meter (non-Linux backend, missing metadata) | State unchanged. A seed that never completes keeps the next shot a seed. |
| Invalid exposure config in `config.json` | Clamped into range at run time. The preview endpoint rejects it on input. |
| No station | Target stays at the day level. Ramping still works. |

## 9. Not Done / Follow-ups

- A shutter longer than 5 s is not supported (decision). It would need an
  on-Pi test of long exposures, capture time and HTTP timeouts.
- Warmup for fully manual ramped frames is unchanged (about 11 frames). Reducing it, and
  passing the still controls to `camera.start()`, needs an on-Pi test and
  would lift the §5.5 interval budget.
- Per-rule fixed exposure overrides were declined for now (§3.1).
- The metering does not yet handle a region of interest (e.g. excluding the sky).
- The highlight guard (§5.6) learns only from captured frames, so a ramp
  started from preview frames still shows the unguarded exposure until two
  night captures have run. Its constants (1 EV pull, 1/12 EV recovery, −3 EV
  floor, 0° gate) come from one night's measurements and may need tuning
  after the first guarded night.
- On 2026-09-22, many night frames came out about 1.6 EV darker than planned at
  the same requested shutter (21:47–05:25, often every 3–5 minutes). The cause
  is not established; the actual-exposure metadata now recorded in each
  sidecar (`worklogs/2026-09-22-capture-exposure-metadata.md`) should show
  whether the camera used a shorter exposure.
- Ramp state is not persisted across daemon restarts (§5.4, intended).

## 10. Dashboard UI (redesign, 2026-09-22)

The first UI (an eight-field card on the Scheduler page) was judged too
technical and gave no way to see the result. The redesign puts the ramp where
exposure already lives:

- **Toggle.** *Scheduled exposure* sits below Shutter on the dashboard. It
  stages `ScheduleConfig.exposure` through `POST /api/schedule/exposure` and
  is saved or discarded with the dashboard's Save Settings / Discard
  Changes.
- **Locked, live fields.** With the toggle on, Shutter and Gain are
  read-only and show the *next planned exposure* from (White balance stays
  editable — §6)
  `GET /api/status` → `exposure_plan`. This is recomputed on every poll from
  the ramp state, the settings, and the current sun elevation, so it drifts
  as the night target changes and jumps when a scheduled frame is observed.
  An amber style marks ramp-driven values, and a brief pulse marks a change.
  While the ramp is still seeding they show the preview's own live auto
  exposure instead.
- **Brightness-equivalent preview.** The live preview can't run a 4 s
  shutter, so the stream request carries a preview-only `exposure_override`:
  the plan's total `shutter × gain`, with the shutter clamped to the preview
  frame time and the rest moved into gain (≤ 16), plus the plan's colour
  gains. Brightness and colour match the next scheduled frame; noise is
  higher. `reconfigure_stream` stages only `settings`, so the override is
  never saved.
- **Three simple controls.** *Brightness compensation* (a slider in 1/3 EV
  steps; it shifts the whole curve, day and night), *Night look* (Dark ↔
  Bright, which maps to `night_drop_ev` 4 … 0 EV), and *Max shutter* /
  *Max gain*. Max step, smoothing, min shutter and the *Highlight clip
  budget* (§5.6, 2026-09-23) sit under *Advanced*. (A white-balance rate
  field was listed here before white balance left the ramp, §6.)

## 11. The Ramp Learns From Live Preview Frames

Metering only captured frames left the UI unable to show anything before the
first scheduled capture: with no ramp state there is no exposure to preview,
so the preview fell back to the camera's own auto exposure, which ignores
these settings. Exposure compensation (`ExposureValue`) would have been the
small fix, but this pipeline accepts and ignores it — measured on the
IMX477: identical exposure and identical image brightness at −3, 0 and +3 EV
(`worklogs/2026-09-21-exposure-ramping.md`).

So the preview is metered too:

- `PreviewFrame.meter` carries §5.1's meter for each preview frame.
- One shared `RampStore` (`Arc<Mutex<Option<RampState>>>`) lives on
  `SchedulerHandle` and is read by scheduled captures, by `live_exposure_plan`
  and by a task `SchedulerHandle::spawn` starts, which subscribes to preview
  frames and folds them in with `exposure_ramp::observe_preview`. It re-reads
  the staged config once a second, and in `Dashboard` mode it measures
  nothing and clears the state.
- `observe_preview` updates the smoothed scene estimate and eases white
  balance but never writes `planned_log2_exposure`, the anchor for the
  capture sequence's per-frame step limit. Preview frames therefore inform
  the ramp without letting a preview restart or a passing cloud jump a
  running timelapse.
- Effects: unsaved settings show up in the preview within about a second of
  it running, with the scheduler paused; the locked fields show real values
  instead of "Auto"; and a scheduled run starts from a converged estimate
  instead of a seed frame, removing the ~1 EV first-frame overshoot measured
  in the first hardware test.
- The loop stays correct because each observation uses the frame's *actual*
  exposure from its metadata, even when the preview cannot reach the planned
  exposure (a multi-second night shutter clamps to the preview frame time and
  makes up the difference in gain, up to 16×).
