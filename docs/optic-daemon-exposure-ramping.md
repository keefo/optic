# Design: Exposure Ramping ("Holy Grail" Day-to-Night) for Scheduled Captures

**Component:** `src/exposure_ramp.rs` (pure logic), wired into
`optic_scheduler` scheduled captures and `native_camera` still capture.
**Status:** Implemented and tested: Mac unit tests, API and Scheduler page, plus
the Linux/aarch64 compile and camera-free tests on the Pi. **Not deployed and
not hardware-validated**; see
`worklogs/2026-09-21-exposure-ramping.md`.

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
| White balance | `AwbEnable(false)` + `ColourGains` | Yes, eased (§6) |
| Aperture | none (manual 6 mm f/1.2 CS lens) | No |
| Focus | none (manual) | No |

## 3. Decisions (user, 2026-09-21)

| Topic | Decision |
|---|---|
| Max shutter | Keep the existing 5 s validation cap. The f/1.2 lens gathers at 5 s what an f/2.4 lens gathers at 20 s. Longer needs a separate hardware test. |
| Night look | **Brightened night.** The target stays at the day level down to sun +6°, then eases 2 EV darker by −18° (astronomical dusk). |
| Scope | **Global only.** One exposure mode for all scheduled captures: `Dashboard` (unchanged behaviour) or `AutoRamp`. No per-rule presets (§3.1). |
| White balance | **Eased grey-world**, seeded from AWB, at most ~3% change per frame. |
| Metering | **Trimmed log-mean luma** over a 1/16 sample grid, darkest/brightest 2% dropped. |

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

A new field on `ScheduleConfig` (`AppConfig.schedule.exposure`), staged and
committed with the rules through the existing preview/commit/discard flow
(`POST /api/schedule/preview`, then `POST /api/config/commit`). A config
written before this field existed deserializes as `Dashboard`.

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
  "wb_max_step_pct": 3.0
}
```

| Field | Range | Meaning |
|---|---|---|
| `min_shutter_us` | 100 ..= `max_shutter_us` | Shortest shutter the ramp uses. |
| `max_shutter_us` | ..= 5,000,000 | Longest shutter (the `CameraSettings` cap). |
| `max_gain` | 1 ..= 16 | Highest analogue gain. |
| `day_bias_ev` | −3 ..= 3 | Target brightness in daylight, in EV relative to 18% grey. |
| `night_drop_ev` | 0 ..= 6 | How much darker the night target is than the day target. |
| `max_step_ev` | 0.05 ..= 2 | Largest exposure change between two consecutive ramped frames. |
| `smoothing` | 0 ..= 0.95 | Weight of the previous scene estimate (0 = react fully to each frame). |
| `wb_max_step_pct` | 0 ..= 20 | Largest colour-gain change per frame, in percent. |

`POST /api/schedule/preview` rejects out-of-range values with 422. At run
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
- **Plan:** `E_desired = log2(target_luminance) − Ŝ`. It then moves at most
  `max_step_ev` from the previous frame's planned `E`, and is clamped to
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
initializes `Ŝ` and the colour gains. The first planned frame after a seed
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

## 6. White Balance

Per-frame AWB (`AwbEnable(true)`, today's default) re-estimates colour on
every capture and is a known flicker source. In `AutoRamp`:

- The seed frame runs AWB; its reported `ColourGains` become the starting
  gains.
- Every later frame is captured with `AwbEnable(false)` and explicit
  `ColourGains` from the ramp state.
- After each frame, the grey-world estimate (linear mid-tone R, G, B means)
  suggests `gains × (G/R, G/B)`. The ramp moves toward it with the same
  `smoothing`, at most `wb_max_step_pct` per frame, clamped to 0.5..8.
- WB is held unchanged when the frame is too dark to judge
  (`L < 0.01` or fewer than 1% of samples usable). This avoids chasing
  sensor noise at night.

Known limitation: grey-world pulls strongly coloured scenes, such as a vivid
sunset or sodium-lit streets, toward neutral. The per-frame limit makes that
drift slow and smooth rather than preventing it. The DNG keeps the raw data
for fixing colour in post.

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
- The Scheduler page has a new **Scheduled exposure** card with the mode and
  its limits, staged and saved together with the rules, plus a live readout of
  that status.
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
- Ramp state is not persisted across daemon restarts (§5.4, intended).
