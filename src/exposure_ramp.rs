//! Exposure ramping ("Holy Grail" day-to-night) for scheduled captures —
//! `docs/optic-daemon-exposure-ramping.md`.
//!
//! Pure logic only: metering a YUV420 frame, the sun-elevation target
//! curve, the shutter-first/gain-last split, and the per-frame ramp state.
//! No camera, clock or I/O dependency, so every piece is unit-tested with
//! synthetic frames and synthetic sun elevations. `optic_scheduler` owns
//! the one `RampState` and threads captures through `plan`/`observe`.

// Metering is only called from the Linux-only native camera backend.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::camera::{MAX_GAIN, MAX_SHUTTER_US, MIN_GAIN, MIN_SHUTTER_US, STILL_CAPTURE_FRAMES};

/// Photographic mid-grey as linear luminance; `bias_ev = 0` targets it.
pub const MID_GREY: f64 = 0.18;
/// Sun elevation at and above which the target stays at the day level.
pub const DAY_ELEVATION_DEG: f64 = 6.0;
/// Sun elevation at and below which the target is fully at the night level.
pub const NIGHT_ELEVATION_DEG: f64 = -18.0;
/// A gap longer than this since the last *captured* ramped frame breaks the
/// sequence: the next capture meters itself afresh instead of planning from
/// a stale exposure (design doc §5.4). Live preview frames don't count:
/// they refresh `updated_at`, and on 2026-09-28 six hours of unattended,
/// saturated preview frames kept a wrong estimate "fresh"
/// (worklogs/2026-09-28-ramp-overexposure.md).
pub const RESEED_GAP: Duration = Duration::minutes(30);
/// A ramped output frame with at least this share of samples clipped is
/// logged as overexposed. Daytime sky and glass clipped 1–7% on 2026-09-22;
/// the 2026-09-28 white frame clipped 60%.
pub const OVEREXPOSED_CLIPPED_FRACTION: f64 = 0.25;

/// Metering samples every 4th pixel of every 4th row (1/16 of the frame).
const SAMPLE_STEP: usize = 4;
/// Fraction of samples dropped at each end of the luma histogram.
const TRIM_FRACTION: f64 = 0.02;
/// Approximate display gamma of the ISP's output, used to linearize luma.
const GAMMA: f64 = 2.2;
/// Luma at or above this counts as clipped.
const CLIPPED_LUMA: u8 = 250;
/// Luma range whose chroma is trusted for the grey-world estimate.
const GREY_WORLD_LUMA: std::ops::RangeInclusive<u8> = 16..=235;
/// Frames need at least this fraction of usable mid-tone samples before
/// their colour means are reported at all.
const MIN_COLOUR_SAMPLE_FRACTION: f64 = 0.01;
/// Share of the gap to the next shot a capture may use.
const GAP_BUDGET_FRACTION: f64 = 0.8;
/// Pipeline start/stop, encode and write time on top of the frames.
const CAPTURE_OVERHEAD_US: f64 = 1_500_000.0;
/// Floor for luminance before taking a log.
const MIN_LUMINANCE: f64 = 1e-6;
/// Floor for the clipped fraction before taking a log.
const MIN_CLIPPED_FRACTION: f64 = 1e-6;

// Highlight guard (design doc §5.6). Rates are per captured frame.
/// The guard runs only while the sun is below this elevation.
const GUARD_SUN_ELEVATION_DEG: f64 = 0.0;
/// Largest darkening the guard applies after one frame.
const GUARD_MAX_PULL_EV: f64 = 1.0;
/// How fast the guard gives exposure back once clipping is well under budget.
const GUARD_RECOVERY_EV: f64 = 1.0 / 12.0;
/// The guard never darkens the plan by more than this.
const GUARD_FLOOR_EV: f64 = -3.0;
/// Recovery starts only below this share of the budget; between it and the
/// budget the guard holds (hysteresis, about 1 EV wide).
const GUARD_RELEASE_FRACTION: f64 = 0.5;
/// Largest `clip_budget_percent`.
const MAX_CLIP_BUDGET_PERCENT: f64 = 10.0;
/// Time constant for what live preview frames teach the ramp (design doc
/// §11). Per-frame learning at preview frame rate made a feedback loop: at
/// night the preview runs high analogue gain, the scene then meters about
/// 1.2 EV brighter per EV of gain, and the preview exposure hunted ±0.2 EV
/// (worklogs/2026-09-23-highlight-guard.md, Part 3).
const PREVIEW_LEARNING_TIME_CONSTANT_S: f64 = 5.0;
/// Floor for the clipped fraction in the clip estimate (design doc §5.7), so
/// a clip-free frame can't drag its log average towards −∞.
const CLIP_ESTIMATE_FLOOR: f64 = 1e-4;

// Daylight sanity ceiling (design doc §5.8), in µs × gain at the day
// target. Fitted to this station's 2026-09-24/25 dawns and dusks: the
// mid-grey exposure peaked at 621 ms at sun 0…1°, 283 ms at +2…3°, 79 ms at
// +6…7°, 53 ms at +7…9° and 41 ms above +10°. Frame by frame, the curve
// stays at least 1.6 EV above every one of them (tightest: 52.8 ms at +8.2°).
/// The ceiling with the sun on the horizon.
const DAYLIGHT_CEILING_AT_HORIZON_US: f64 = 2_500_000.0;
/// The ceiling halves for every this many degrees of sun elevation...
const DAYLIGHT_CEILING_HALVING_DEG: f64 = 2.0;
/// ...down to this floor in full daylight.
const DAYLIGHT_CEILING_FLOOR_US: f64 = 160_000.0;

/// How scheduled captures are exposed — `ScheduleConfig.exposure`. One
/// global mode, deliberately not per rule: merged rules share one physical
/// frame, so they must share one exposure (design doc §3.1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode")]
pub enum ScheduleExposure {
    /// The committed dashboard settings, exactly as before this feature.
    #[default]
    Dashboard,
    AutoRamp(RampSettings),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RampSettings {
    pub min_shutter_us: u64,
    pub max_shutter_us: u64,
    pub max_gain: f32,
    /// Daylight target, in EV relative to mid-grey.
    pub day_bias_ev: f64,
    /// How much darker the night target is than the day target.
    pub night_drop_ev: f64,
    /// Largest planned exposure change between consecutive ramped frames.
    pub max_step_ev: f64,
    /// Weight of the previous scene estimate; 0 reacts fully to each frame.
    pub smoothing: f64,
    /// Highlight guard: percentage of metered samples allowed to clip while
    /// the sun is below 0° before the ramp backs off. 0 disables it.
    pub clip_budget_percent: f64,
}

impl Default for RampSettings {
    fn default() -> Self {
        Self {
            min_shutter_us: MIN_SHUTTER_US,
            max_shutter_us: MAX_SHUTTER_US,
            max_gain: 8.0,
            day_bias_ev: 0.0,
            night_drop_ev: 2.0,
            max_step_ev: 1.0 / 3.0,
            smoothing: 0.5,
            clip_budget_percent: 1.0,
        }
    }
}

impl RampSettings {
    /// Input validation for `POST /api/schedule/preview`.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(MIN_SHUTTER_US..=MAX_SHUTTER_US).contains(&self.min_shutter_us) {
            return Err("min shutter must be between 100 and 5000000 microseconds");
        }
        if !(self.min_shutter_us..=MAX_SHUTTER_US).contains(&self.max_shutter_us) {
            return Err("max shutter must be between min shutter and 5000000 microseconds");
        }
        if !(MIN_GAIN..=MAX_GAIN).contains(&self.max_gain) {
            return Err("max gain must be between 1 and 16");
        }
        let checks: [(f64, f64, f64, &'static str); 5] = [
            (
                self.day_bias_ev,
                -3.0,
                3.0,
                "day brightness must be between -3 and 3 EV",
            ),
            (
                self.night_drop_ev,
                0.0,
                6.0,
                "night darkening must be between 0 and 6 EV",
            ),
            (
                self.max_step_ev,
                0.05,
                2.0,
                "max step must be between 0.05 and 2 EV",
            ),
            (
                self.smoothing,
                0.0,
                0.95,
                "smoothing must be between 0 and 0.95",
            ),
            (
                self.clip_budget_percent,
                0.0,
                MAX_CLIP_BUDGET_PERCENT,
                "clip budget must be between 0 and 10 percent",
            ),
        ];
        for (value, min, max, message) in checks {
            if !(min..=max).contains(&value) {
                return Err(message);
            }
        }
        Ok(())
    }

    /// Run-time guard for a hand-edited `config.json`: clamps every field
    /// into range (non-finite values fall back to the default) rather than
    /// failing scheduled captures.
    pub fn sanitized(&self) -> Self {
        let defaults = Self::default();
        let clamp = |value: f64, min: f64, max: f64, fallback: f64| {
            if value.is_finite() {
                value.clamp(min, max)
            } else {
                fallback
            }
        };
        let min_shutter_us = self.min_shutter_us.clamp(MIN_SHUTTER_US, MAX_SHUTTER_US);
        Self {
            min_shutter_us,
            max_shutter_us: self.max_shutter_us.clamp(min_shutter_us, MAX_SHUTTER_US),
            max_gain: if self.max_gain.is_finite() {
                self.max_gain.clamp(MIN_GAIN, MAX_GAIN)
            } else {
                defaults.max_gain
            },
            day_bias_ev: clamp(self.day_bias_ev, -3.0, 3.0, defaults.day_bias_ev),
            night_drop_ev: clamp(self.night_drop_ev, 0.0, 6.0, defaults.night_drop_ev),
            max_step_ev: clamp(self.max_step_ev, 0.05, 2.0, defaults.max_step_ev),
            smoothing: clamp(self.smoothing, 0.0, 0.95, defaults.smoothing),
            clip_budget_percent: clamp(
                self.clip_budget_percent,
                0.0,
                MAX_CLIP_BUDGET_PERCENT,
                defaults.clip_budget_percent,
            ),
        }
    }
}

/// Brightness and colour of one captured frame (design doc §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct FrameMeter {
    /// Trimmed log-mean linear luminance, 0..1 (0.18 = mid-grey).
    pub luminance: f64,
    /// Fraction of samples with luma at or above `CLIPPED_LUMA`.
    pub clipped_fraction: f64,
    pub samples: u32,
    /// Mean linear R, G, B of mid-tone samples; `None` when too few.
    pub grey_world: Option<[f64; 3]>,
}

/// Meters a full-range BT.601 YUV420 frame laid out as the still stream
/// delivers it: a `stride × height` Y plane followed by `stride/2 ×
/// height/2` U and V planes. `None` if the buffer doesn't match.
pub fn meter_yuv420(data: &[u8], width: u32, height: u32, stride: u32) -> Option<FrameMeter> {
    let (width, height, stride) = (width as usize, height as usize, stride as usize);
    if width < 2 || height < 2 || stride < width || !stride.is_multiple_of(2) {
        return None;
    }
    let uv_stride = stride / 2;
    let y_len = stride.checked_mul(height)?;
    let uv_len = uv_stride.checked_mul(height / 2)?;
    if data.len() < y_len.checked_add(uv_len.checked_mul(2)?)? {
        return None;
    }
    let (luma, chroma) = data.split_at(y_len);
    let (u_plane, v_plane) = chroma.split_at(uv_len);
    let linear = linear_lut();

    let mut histogram = [0_u64; 256];
    let mut rgb_sum = [0.0_f64; 3];
    let mut rgb_samples = 0_u64;
    for y in (0..height).step_by(SAMPLE_STEP) {
        let row = &luma[y * stride..y * stride + width];
        let chroma_row = (y / 2) * uv_stride;
        for x in (0..width).step_by(SAMPLE_STEP) {
            let value = row[x];
            histogram[usize::from(value)] += 1;
            if GREY_WORLD_LUMA.contains(&value) {
                let u = f64::from(u_plane[chroma_row + x / 2]) - 128.0;
                let v = f64::from(v_plane[chroma_row + x / 2]) - 128.0;
                let y_value = f64::from(value);
                let channels = [
                    y_value + 1.402 * v,
                    y_value - 0.344_136 * u - 0.714_136 * v,
                    y_value + 1.772 * u,
                ];
                for (sum, channel) in rgb_sum.iter_mut().zip(channels) {
                    *sum += linear[channel.round().clamp(0.0, 255.0) as usize];
                }
                rgb_samples += 1;
            }
        }
    }

    let samples: u64 = histogram.iter().sum();
    if samples == 0 {
        return None;
    }
    let trim = (samples as f64 * TRIM_FRACTION).floor() as u64;
    let mut kept = histogram;
    trim_histogram(kept.iter_mut(), trim);
    trim_histogram(kept.iter_mut().rev(), trim);
    let kept_total: u64 = kept.iter().sum();
    let log_sum: f64 = kept
        .iter()
        .zip(linear.iter())
        .map(|(&count, &value)| count as f64 * value.max(MIN_LUMINANCE).ln())
        .sum();
    let luminance = (log_sum / kept_total.max(1) as f64).exp();
    let clipped: u64 = histogram[usize::from(CLIPPED_LUMA)..].iter().sum();

    let grey_world = (rgb_samples > 0
        && rgb_samples as f64 >= samples as f64 * MIN_COLOUR_SAMPLE_FRACTION)
        .then(|| rgb_sum.map(|sum| sum / rgb_samples as f64));
    Some(FrameMeter {
        luminance,
        clipped_fraction: clipped as f64 / samples as f64,
        samples: u32::try_from(samples).unwrap_or(u32::MAX),
        grey_world,
    })
}

/// Removes `count` samples from the histogram bins in iteration order.
fn trim_histogram<'a>(bins: impl Iterator<Item = &'a mut u64>, mut count: u64) {
    for bin in bins {
        if count == 0 {
            return;
        }
        let taken = (*bin).min(count);
        *bin -= taken;
        count -= taken;
    }
}

/// Linear light for each 8-bit code, sampled at the bin centre so code 0
/// stays finite under `ln`.
fn linear_lut() -> [f64; 256] {
    std::array::from_fn(|code| ((code as f64 + 0.5) / 255.0).powf(GAMMA))
}

/// Target brightness in EV relative to mid-grey for a sun elevation
/// (design doc §5.2). `None` (no station configured) stays at day level.
pub fn target_bias_ev(sun_elevation_deg: Option<f64>, settings: &RampSettings) -> f64 {
    let Some(elevation) = sun_elevation_deg else {
        return settings.day_bias_ev;
    };
    let span = DAY_ELEVATION_DEG - NIGHT_ELEVATION_DEG;
    let t = ((DAY_ELEVATION_DEG - elevation) / span).clamp(0.0, 1.0);
    let eased = t * t * (3.0 - 2.0 * t);
    settings.day_bias_ev - settings.night_drop_ev * eased
}

pub fn target_luminance(bias_ev: f64) -> f64 {
    MID_GREY * bias_ev.exp2()
}

/// Splits a log2 exposure product (`shutter_us × gain`) into shutter first,
/// then gain: gain rises above 1 only once the shutter is at `max_shutter_us`,
/// and falls back to 1 before the shutter shortens.
pub fn split_exposure(
    log2_exposure: f64,
    min_shutter_us: u64,
    max_shutter_us: u64,
    max_gain: f32,
) -> (u64, f32) {
    let product = log2_exposure.exp2();
    let shutter = product
        .clamp(min_shutter_us as f64, max_shutter_us as f64)
        .round();
    // Below the shutter ceiling the shutter carries the whole exposure;
    // dividing by the rounded shutter would leave a spurious gain of ~1.0004.
    let gain = if shutter < max_shutter_us as f64 {
        f64::from(MIN_GAIN)
    } else {
        (product / shutter).clamp(f64::from(MIN_GAIN), f64::from(max_gain))
    };
    (shutter as u64, gain as f32)
}

/// Longest shutter that lets a capture finish well before the next
/// scheduled shot (design doc §5.5). A still capture waits about
/// `STILL_CAPTURE_FRAMES` frames, each at least one shutter long.
pub fn max_shutter_for_gap(gap: Option<Duration>, settings: &RampSettings) -> u64 {
    let Some(gap) = gap else {
        return settings.max_shutter_us;
    };
    let gap_us = gap.num_microseconds().unwrap_or(i64::MAX) as f64;
    let budget = (GAP_BUDGET_FRACTION * gap_us - CAPTURE_OVERHEAD_US) / STILL_CAPTURE_FRAMES as f64;
    let budget = if budget.is_finite() && budget > 0.0 {
        budget as u64
    } else {
        0
    };
    budget
        .min(settings.max_shutter_us)
        .max(settings.min_shutter_us)
}

/// What the next ramped capture should use.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "kind")]
pub enum RampPlan {
    /// Auto exposure and AWB; its measurement (re)starts the ramp.
    Seed,
    Manual {
        shutter_us: u64,
        gain: f32,
        /// `log2(shutter_us × gain)` actually planned, after clamping.
        log2_exposure: f64,
    },
}

/// What a completed capture reports back: the exposure the camera actually
/// used (request metadata) and the frame's meter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameObservation {
    pub exposure_us: f64,
    pub analogue_gain: f64,
    pub meter: FrameMeter,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct RampState {
    /// When anything, captured or preview frame, last updated the state.
    pub updated_at: DateTime<Utc>,
    /// When a scheduled capture (or its metering pass) last updated it.
    /// `None` for a state learned from preview frames only. Only this
    /// decides whether the next capture may plan rather than reseed.
    pub captured_at: Option<DateTime<Utc>>,
    /// Smoothed scene brightness: `log2(luminance) − log2(exposure product)`.
    pub scene_ev: f64,
    /// The previous frame's planned exposure. `None` right after a seed,
    /// which lets the first planned frame jump without a step limit.
    pub planned_log2_exposure: Option<f64>,
    /// Highlight guard (design doc §5.6); learns from captured frames only.
    pub highlight: HighlightGuard,
}

/// The clipping-budget guard's state (design doc §5.6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct HighlightGuard {
    /// Added to the planned exposure; always in `GUARD_FLOOR_EV..=0`, so the
    /// guard can only darken.
    pub offset_ev: f64,
    /// How much the last captured frame pulled `offset_ev` down. The next
    /// plan's downward step limit widens by this much.
    pub last_pull_ev: f64,
    /// The last captured frame's clipping excess, `log2(clipped / budget)`
    /// at its planned exposure. A pull needs two frames over budget in a row.
    pub previous_excess_ev: Option<f64>,
    /// Smoothed `log2(clipped fraction) − log2(actual exposure)` from every
    /// frame, captured or preview (design doc §5.7). Budget-independent, so
    /// the preview target follows a new budget without waiting for a frame.
    pub clip_scene_ev: Option<f64>,
}

impl HighlightGuard {
    /// Folds one captured frame's clipping excess into the guard. `excess_ev`
    /// is `None` when the frame had no planned exposure to compare with.
    fn next(&self, excess_ev: Option<f64>, active: bool, budget_percent: f64) -> Self {
        if budget_percent <= 0.0 {
            return Self::default();
        }
        let recovered = (self.offset_ev + GUARD_RECOVERY_EV).min(0.0);
        let (Some(excess), true) = (excess_ev, active) else {
            return Self {
                offset_ev: recovered,
                last_pull_ev: 0.0,
                previous_excess_ev: None,
                clip_scene_ev: self.clip_scene_ev,
            };
        };
        let confirmed = self
            .previous_excess_ev
            .map_or(0.0, |previous| previous.min(excess));
        let (offset_ev, last_pull_ev) = if confirmed > 0.0 {
            let pull = confirmed
                .min(GUARD_MAX_PULL_EV)
                .min(self.offset_ev - GUARD_FLOOR_EV)
                .max(0.0);
            (self.offset_ev - pull, pull)
        } else if excess < GUARD_RELEASE_FRACTION.log2() {
            (recovered, 0.0)
        } else {
            (self.offset_ev, 0.0)
        };
        Self {
            offset_ev,
            last_pull_ev,
            previous_excess_ev: Some(excess),
            clip_scene_ev: self.clip_scene_ev,
        }
    }

    /// Folds one frame's clipping into the clip estimate.
    fn with_clip_sample(self, sample: f64, smoothing: f64) -> Self {
        let clip_scene_ev = match self.clip_scene_ev {
            Some(previous) => previous + (1.0 - smoothing) * (sample - previous),
            None => sample,
        };
        Self {
            clip_scene_ev: Some(clip_scene_ev),
            ..self
        }
    }
}

/// One frame's clip estimate sample (design doc §5.7).
fn clip_sample(observation: &FrameObservation) -> f64 {
    let exposure = (observation.exposure_us.max(1.0) * observation.analogue_gain.max(1.0)).log2();
    observation
        .meter
        .clipped_fraction
        .max(CLIP_ESTIMATE_FLOOR)
        .log2()
        - exposure
}

/// Whether the highlight guard runs at this sun elevation: only below the
/// horizon, and never without a station (design doc §5.6).
pub fn guard_active(sun_elevation_deg: Option<f64>) -> bool {
    sun_elevation_deg.is_some_and(|elevation| elevation < GUARD_SUN_ELEVATION_DEG)
}

/// Why a plan is a seed (design doc §5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedReason {
    /// No ramp state at all (daemon start, or just switched to `AutoRamp`).
    NoState,
    /// State learned from preview frames only; no capture has measured the
    /// scene yet.
    NoCapture,
    /// The last capture is older than `RESEED_GAP`.
    CaptureGap,
}

/// Everything that went into one plan, for the per-capture log line and
/// `/api/status` (design doc §5.9).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct RampDecision {
    pub plan: RampPlan,
    pub seed_reason: Option<SeedReason>,
    /// Seconds since the capture that last updated the state.
    pub capture_age_s: Option<i64>,
    /// The scene estimate the plan used (`None` without state).
    pub scene_ev: Option<f64>,
    pub sun_elevation_deg: Option<f64>,
    pub target_bias_ev: f64,
    /// The planned exposure before the step limit and the ceilings,
    /// highlight guard included.
    pub desired_log2_exposure: Option<f64>,
    /// The step limit this plan allowed each way (`None` without an anchor).
    pub step_limit_ev: Option<f64>,
    pub step_limited: bool,
    /// The daylight sanity ceiling (§5.8), when the sun is up.
    pub ceiling_log2_exposure: Option<f64>,
    /// The ceiling cut this plan.
    pub ceiling_bound: bool,
    pub guard_offset_ev: f64,
    /// The last captured frame's clipping excess over the budget, EV.
    pub guard_previous_excess_ev: Option<f64>,
    pub max_shutter_us: u64,
}

/// Which updates keep a state usable for planning.
#[derive(Clone, Copy)]
enum Freshness {
    /// Scheduled captures: only a capture within `RESEED_GAP` counts.
    Capture,
    /// The live preview's view (§11): preview frames count too, so the
    /// dashboard can show the settings before any capture.
    Preview,
}

/// Plans the next ramped capture (design doc §5.3/§5.4).
pub fn plan(
    state: Option<&RampState>,
    now: DateTime<Utc>,
    sun_elevation_deg: Option<f64>,
    settings: &RampSettings,
    max_shutter_us: u64,
) -> RampPlan {
    decide(state, now, sun_elevation_deg, settings, max_shutter_us).plan
}

/// `plan`, with the inputs and limits that produced it.
pub fn decide(
    state: Option<&RampState>,
    now: DateTime<Utc>,
    sun_elevation_deg: Option<f64>,
    settings: &RampSettings,
    max_shutter_us: u64,
) -> RampDecision {
    decide_with(
        state,
        now,
        sun_elevation_deg,
        settings,
        max_shutter_us,
        Freshness::Capture,
    )
}

/// What the live preview plans for (design doc §11): like `plan`, but a
/// state kept fresh by preview frames alone still plans, so the preview
/// shows the settings before any capture. Scheduled captures never use it.
pub fn preview_plan(
    state: Option<&RampState>,
    now: DateTime<Utc>,
    sun_elevation_deg: Option<f64>,
    settings: &RampSettings,
    max_shutter_us: u64,
) -> RampPlan {
    decide_with(
        state,
        now,
        sun_elevation_deg,
        settings,
        max_shutter_us,
        Freshness::Preview,
    )
    .plan
}

/// The step limit for a plan anchored to a capture `since` ago: `max_step_ev`
/// per minute, never less than one step (design doc §5.3). A 1-minute or
/// faster timelapse steps exactly as before; a slower one can follow dawn.
pub fn step_limit_ev(settings: &RampSettings, since: Duration) -> f64 {
    let minutes = since.num_milliseconds() as f64 / 60_000.0;
    settings.max_step_ev * minutes.max(1.0)
}

/// The daylight sanity ceiling (design doc §5.8) as a log2 exposure
/// product, or `None` with the sun below the horizon or no station.
pub fn daylight_ceiling_log2(
    sun_elevation_deg: Option<f64>,
    settings: &RampSettings,
) -> Option<f64> {
    let elevation = sun_elevation_deg.filter(|elevation| *elevation >= 0.0)?;
    let ceiling_us = (DAYLIGHT_CEILING_AT_HORIZON_US
        * (-elevation / DAYLIGHT_CEILING_HALVING_DEG).exp2())
    .max(DAYLIGHT_CEILING_FLOOR_US);
    Some(ceiling_us.log2() + settings.day_bias_ev)
}

fn decide_with(
    state: Option<&RampState>,
    now: DateTime<Utc>,
    sun_elevation_deg: Option<f64>,
    settings: &RampSettings,
    max_shutter_us: u64,
    freshness: Freshness,
) -> RampDecision {
    let limits = ExposureLimits::new(settings, max_shutter_us, sun_elevation_deg);
    let capture_age = state.and_then(|state| state.captured_at).map(|at| now - at);
    let mut decision = RampDecision {
        plan: RampPlan::Seed,
        seed_reason: None,
        capture_age_s: capture_age.map(|age| age.num_seconds()),
        scene_ev: state.map(|state| state.scene_ev),
        sun_elevation_deg,
        target_bias_ev: target_bias_ev(sun_elevation_deg, settings),
        desired_log2_exposure: None,
        step_limit_ev: None,
        step_limited: false,
        ceiling_log2_exposure: limits.ceiling,
        ceiling_bound: false,
        guard_offset_ev: state.map_or(0.0, |state| state.highlight.offset_ev),
        guard_previous_excess_ev: state.and_then(|state| state.highlight.previous_excess_ev),
        max_shutter_us: limits.max_shutter_us,
    };
    let seed_reason = match (state, freshness) {
        (None, _) => Some(SeedReason::NoState),
        (Some(_), Freshness::Capture) => match capture_age {
            None => Some(SeedReason::NoCapture),
            Some(age) if age > RESEED_GAP => Some(SeedReason::CaptureGap),
            Some(_) => None,
        },
        (Some(state), Freshness::Preview) => {
            (now - state.updated_at > RESEED_GAP).then_some(SeedReason::CaptureGap)
        }
    };
    let Some(state) = state.filter(|_| seed_reason.is_none()) else {
        decision.seed_reason = seed_reason;
        return decision;
    };
    // The guard darkens what the ramp would actually shoot, so a scene too
    // dark for max shutter × max gain can't hide the offset above the
    // ceiling (design doc §5.6). With no offset this is the plain ramp.
    let desired = unguarded_log2_exposure(state, sun_elevation_deg, settings, &limits)
        + state.highlight.offset_ev;
    let mut stepped = desired;
    if let Some(previous) = state.planned_log2_exposure {
        let step = step_limit_ev(settings, capture_age.unwrap_or_else(Duration::zero));
        // A guard pull may step down further than the step limit (design
        // doc §5.6); upward the step limit is unchanged.
        stepped = desired.clamp(
            previous - step - state.highlight.last_pull_ev,
            previous + step,
        );
        decision.step_limit_ev = Some(step);
        decision.step_limited = stepped != desired;
    }
    decision.desired_log2_exposure = Some(desired);
    decision.ceiling_bound = limits.ceiling.is_some_and(|ceiling| stepped > ceiling);
    let (shutter_us, gain) = limits.split(stepped, settings);
    decision.plan = RampPlan::Manual {
        shutter_us,
        gain,
        log2_exposure: (shutter_us as f64 * f64::from(gain)).log2(),
    };
    decision
}

/// The exposure range one shot may use.
struct ExposureLimits {
    max_shutter_us: u64,
    lowest: f64,
    /// The hardware ceiling: `max_shutter_eff × max_gain`.
    highest: f64,
    /// The daylight sanity ceiling (design doc §5.8), when the sun is up.
    ceiling: Option<f64>,
}

impl ExposureLimits {
    fn new(settings: &RampSettings, max_shutter_us: u64, sun_elevation_deg: Option<f64>) -> Self {
        let max_shutter_us = max_shutter_us.clamp(settings.min_shutter_us, settings.max_shutter_us);
        Self {
            max_shutter_us,
            lowest: (settings.min_shutter_us as f64).log2(),
            highest: (max_shutter_us as f64 * f64::from(settings.max_gain)).log2(),
            ceiling: daylight_ceiling_log2(sun_elevation_deg, settings),
        }
    }

    /// Clamps into range, the daylight ceiling included, then splits
    /// shutter first, gain last.
    fn split(&self, log2_exposure: f64, settings: &RampSettings) -> (u64, f32) {
        let highest = self
            .ceiling
            .map_or(self.highest, |ceiling| ceiling.min(self.highest))
            .max(self.lowest);
        split_exposure(
            log2_exposure.clamp(self.lowest, highest),
            settings.min_shutter_us,
            self.max_shutter_us,
            settings.max_gain,
        )
    }
}

/// The ramp's exposure without the highlight guard or the step limit,
/// capped at the shutter × gain ceiling.
fn unguarded_log2_exposure(
    state: &RampState,
    sun_elevation_deg: Option<f64>,
    settings: &RampSettings,
    limits: &ExposureLimits,
) -> f64 {
    let target = target_luminance(target_bias_ev(sun_elevation_deg, settings)).log2();
    (target - state.scene_ev).min(limits.highest)
}

/// Where the highlight guard is heading (`H*`, design doc §5.7): the offset
/// at which the clip estimate predicts exactly the budget, in
/// `GUARD_FLOOR_EV..=0`. 0 whenever the guard can't act.
pub fn highlight_target_ev(
    state: &RampState,
    sun_elevation_deg: Option<f64>,
    settings: &RampSettings,
    max_shutter_us: u64,
) -> f64 {
    let Some(clip_scene_ev) = state.highlight.clip_scene_ev else {
        return 0.0;
    };
    if settings.clip_budget_percent <= 0.0 || !guard_active(sun_elevation_deg) {
        return 0.0;
    }
    let limits = ExposureLimits::new(settings, max_shutter_us, sun_elevation_deg);
    let at_budget = (settings.clip_budget_percent / 100.0).log2() - clip_scene_ev;
    let unguarded = unguarded_log2_exposure(state, sun_elevation_deg, settings, &limits);
    let target = (at_budget - unguarded).clamp(GUARD_FLOOR_EV, 0.0);
    if target.is_finite() { target } else { 0.0 }
}

/// A shutter and gain pair.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ExposureSetting {
    pub shutter_us: u64,
    pub gain: f32,
}

/// What the live preview shows for the next ramped frame (design doc §5.7).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PreviewPlans {
    /// `H*`, the guard's target offset.
    pub target_ev: f64,
    /// The next frame with the guard at its target.
    pub preview: ExposureSetting,
    /// The next frame without the guard, for *hold to compare*.
    pub unguarded: ExposureSetting,
}

/// The preview's exposures around the next planned frame. `None` while the
/// next frame is a seed.
pub fn preview_plans(
    state: Option<&RampState>,
    now: DateTime<Utc>,
    sun_elevation_deg: Option<f64>,
    settings: &RampSettings,
    max_shutter_us: u64,
) -> Option<PreviewPlans> {
    let RampPlan::Manual { log2_exposure, .. } =
        preview_plan(state, now, sun_elevation_deg, settings, max_shutter_us)
    else {
        return None;
    };
    let state = state?;
    let limits = ExposureLimits::new(settings, max_shutter_us, sun_elevation_deg);
    let target_ev = highlight_target_ev(state, sun_elevation_deg, settings, max_shutter_us);
    let unguarded = log2_exposure - state.highlight.offset_ev;
    let setting = |log2: f64| {
        let (shutter_us, gain) = limits.split(log2, settings);
        ExposureSetting { shutter_us, gain }
    };
    Some(PreviewPlans {
        target_ev,
        preview: setting(unguarded + target_ev),
        unguarded: setting(unguarded),
    })
}

/// Folds a completed capture into the ramp state. `sun_elevation_deg` is
/// the shot's, and gates the highlight guard (design doc §5.6).
///
/// White balance is deliberately not part of the ramp: scheduled captures
/// use whatever the dashboard's White balance control says, exactly like a
/// manual capture (user decision, 2026-09-22 — see design doc §6).
pub fn observe(
    state: Option<&RampState>,
    plan: &RampPlan,
    observation: &FrameObservation,
    now: DateTime<Utc>,
    sun_elevation_deg: Option<f64>,
    settings: &RampSettings,
) -> Option<RampState> {
    let exposure = (observation.exposure_us.max(1.0) * observation.analogue_gain.max(1.0)).log2();
    let scene_ev = observation.meter.luminance.max(MIN_LUMINANCE).log2() - exposure;
    match (plan, state) {
        (RampPlan::Manual { log2_exposure, .. }, Some(state)) => {
            let blend = 1.0 - settings.smoothing;
            // Clipping as it would be at the *planned* exposure, so a frame
            // the camera exposed differently can't mislead the guard.
            let excess_ev = (observation.meter.clipped_fraction.max(MIN_CLIPPED_FRACTION)
                / (settings.clip_budget_percent / 100.0))
                .log2()
                + (log2_exposure - exposure);
            Some(RampState {
                updated_at: now,
                captured_at: Some(now),
                scene_ev: state.scene_ev + blend * (scene_ev - state.scene_ev),
                planned_log2_exposure: Some(*log2_exposure),
                highlight: HighlightGuard {
                    // The budget-independent clip estimate survives a guard
                    // reset (budget 0), so the preview target stays live.
                    clip_scene_ev: state.highlight.clip_scene_ev,
                    ..state.highlight.next(
                        excess_ev.is_finite().then_some(excess_ev),
                        guard_active(sun_elevation_deg),
                        settings.clip_budget_percent,
                    )
                }
                .with_clip_sample(clip_sample(observation), settings.smoothing),
            })
        }
        _ => Some(RampState {
            updated_at: now,
            captured_at: Some(now),
            scene_ev,
            planned_log2_exposure: None,
            highlight: HighlightGuard::default()
                .with_clip_sample(clip_sample(observation), settings.smoothing),
        }),
    }
}

/// Folds a *live preview* frame into the ramp state (design doc §11).
///
/// Brightness only. `planned_log2_exposure` is deliberately left alone: it
/// anchors the capture sequence's per-frame step limit, so a preview restart
/// or a passing cloud can't jump a running timelapse. So is the highlight
/// guard's timelapse offset, whose rates are per captured frame (design doc
/// §5.6); only its clip estimate learns from the preview (§5.7).
pub fn observe_preview(
    state: Option<&RampState>,
    observation: &FrameObservation,
    now: DateTime<Utc>,
    settings: &RampSettings,
) -> Option<RampState> {
    let exposure = (observation.exposure_us.max(1.0) * observation.analogue_gain.max(1.0)).log2();
    let scene_ev = observation.meter.luminance.max(MIN_LUMINANCE).log2() - exposure;
    let Some(state) = state else {
        return Some(RampState {
            updated_at: now,
            // Preview frames never count as a capture (design doc §5.4).
            captured_at: None,
            scene_ev,
            planned_log2_exposure: None,
            highlight: HighlightGuard::default()
                .with_clip_sample(clip_sample(observation), settings.smoothing),
        });
    };
    // By elapsed time, not per frame, so frame rate doesn't set how hard the
    // preview pulls on the estimate.
    let elapsed_s = (now - state.updated_at)
        .num_microseconds()
        .map_or(f64::MAX, |us| us as f64 / 1e6)
        .max(0.0);
    let blend = 1.0 - (-elapsed_s / PREVIEW_LEARNING_TIME_CONSTANT_S).exp();
    Some(RampState {
        updated_at: now,
        captured_at: state.captured_at,
        scene_ev: state.scene_ev + blend * (scene_ev - state.scene_ev),
        planned_log2_exposure: state.planned_log2_exposure,
        // Only the clip estimate learns here (design doc §5.7).
        highlight: state
            .highlight
            .with_clip_sample(clip_sample(observation), 1.0 - blend),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn at(minutes: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 21, 18, 0, 0).unwrap() + Duration::minutes(minutes)
    }

    /// Builds a YUV420 frame; `pixel(x, y)` returns (Y, U, V).
    fn frame(
        width: usize,
        height: usize,
        stride: usize,
        pixel: impl Fn(usize, usize) -> (u8, u8, u8),
    ) -> Vec<u8> {
        let uv_stride = stride / 2;
        let y_len = stride * height;
        let uv_len = uv_stride * height / 2;
        let mut data = vec![0xAA; y_len + 2 * uv_len];
        for y in 0..height {
            for x in 0..width {
                let (luma, u, v) = pixel(x, y);
                data[y * stride + x] = luma;
                let chroma = (y / 2) * uv_stride + x / 2;
                data[y_len + chroma] = u;
                data[y_len + uv_len + chroma] = v;
            }
        }
        data
    }

    fn linear(code: u8) -> f64 {
        ((f64::from(code) + 0.5) / 255.0).powf(GAMMA)
    }

    fn meter(luminance: f64, grey_world: Option<[f64; 3]>) -> FrameMeter {
        FrameMeter {
            luminance,
            clipped_fraction: 0.0,
            samples: 1000,
            grey_world,
        }
    }

    #[test]
    fn meter_measures_a_uniform_grey_frame_as_its_own_luminance() {
        let data = frame(64, 48, 64, |_, _| (118, 128, 128));
        let result = meter_yuv420(&data, 64, 48, 64).unwrap();
        assert!((result.luminance - linear(118)).abs() < 1e-9);
        assert_eq!(result.samples, 16 * 12);
        assert_eq!(result.clipped_fraction, 0.0);
        let [r, g, b] = result.grey_world.unwrap();
        assert!((r - g).abs() < 1e-3 && (b - g).abs() < 1e-3);
    }

    #[test]
    fn meter_trims_the_extreme_tails() {
        // 1% black and 1% white samples, both inside the 2% trim.
        let data = frame(400, 400, 400, |x, y| {
            let index = (y / 4) * 100 + x / 4;
            match index % 100 {
                0 => (0, 128, 128),
                1 => (255, 128, 128),
                _ => (100, 128, 128),
            }
        });
        let result = meter_yuv420(&data, 400, 400, 400).unwrap();
        assert!((result.luminance - linear(100)).abs() < 1e-9);
        assert!((result.clipped_fraction - 0.01).abs() < 1e-9);
    }

    #[test]
    fn meter_ignores_stride_padding() {
        let pixel = |x: usize, y: usize| (((x * 3 + y * 5) % 200 + 20) as u8, 120, 140);
        let tight = meter_yuv420(&frame(64, 32, 64, pixel), 64, 32, 64).unwrap();
        let padded = meter_yuv420(&frame(64, 32, 96, pixel), 64, 32, 96).unwrap();
        assert_eq!(tight, padded);
    }

    #[test]
    fn meter_log_mean_is_below_the_arithmetic_mean_for_mixed_scenes() {
        // Half bright sky, half dark ground: the log mean doesn't let the
        // sky dominate.
        let data = frame(64, 64, 64, |_, y| {
            if y < 32 {
                (230, 128, 128)
            } else {
                (40, 128, 128)
            }
        });
        let result = meter_yuv420(&data, 64, 64, 64).unwrap();
        let arithmetic = (linear(230) + linear(40)) / 2.0;
        let geometric = (linear(230) * linear(40)).sqrt();
        assert!((result.luminance - geometric).abs() < 1e-9);
        assert!(result.luminance < arithmetic);
    }

    #[test]
    fn meter_reports_a_colour_cast_in_the_grey_world_means() {
        // V > 128 pushes red up; U < 128 pulls blue down.
        let data = frame(32, 32, 32, |_, _| (120, 110, 150));
        let [r, g, b] = meter_yuv420(&data, 32, 32, 32).unwrap().grey_world.unwrap();
        assert!(r > g && g > b);
    }

    #[test]
    fn meter_has_no_grey_world_estimate_for_a_black_frame() {
        let data = frame(32, 32, 32, |_, _| (2, 128, 128));
        let result = meter_yuv420(&data, 32, 32, 32).unwrap();
        assert!(result.grey_world.is_none());
        assert!(result.luminance < 1e-4);
    }

    #[test]
    fn meter_rejects_a_short_or_malformed_buffer() {
        assert!(meter_yuv420(&[0; 10], 64, 48, 64).is_none());
        assert!(meter_yuv420(&[0; 10_000], 64, 48, 63).is_none());
        assert!(meter_yuv420(&[], 0, 0, 0).is_none());
    }

    #[test]
    fn target_curve_holds_day_and_night_levels_and_eases_between() {
        let settings = RampSettings::default();
        assert_eq!(target_bias_ev(Some(30.0), &settings), 0.0);
        assert_eq!(target_bias_ev(Some(6.0), &settings), 0.0);
        assert!((target_bias_ev(Some(-6.0), &settings) + 1.0).abs() < 1e-9);
        assert_eq!(target_bias_ev(Some(-18.0), &settings), -2.0);
        assert_eq!(target_bias_ev(Some(-40.0), &settings), -2.0);
        let mut previous = f64::INFINITY;
        for tenth in (-250..=150).rev() {
            let bias = target_bias_ev(Some(f64::from(tenth) / 10.0), &settings);
            assert!(bias <= previous);
            previous = bias;
        }
    }

    #[test]
    fn target_curve_without_a_station_stays_at_day_level() {
        let settings = RampSettings {
            day_bias_ev: -0.5,
            ..RampSettings::default()
        };
        assert_eq!(target_bias_ev(None, &settings), -0.5);
    }

    #[test]
    fn split_puts_exposure_into_shutter_before_gain() {
        // 10 ms at gain 1.
        assert_eq!(
            split_exposure(10_000_f64.log2(), 100, 5_000_000, 8.0),
            (10_000, 1.0)
        );
        // 2 s × 4 with a 2 s shutter ceiling: shutter pinned, gain carries the rest.
        let (shutter, gain) = split_exposure(8_000_000_f64.log2(), 100, 2_000_000, 8.0);
        assert_eq!(shutter, 2_000_000);
        assert!((gain - 4.0).abs() < 1e-4);
        // Beyond max shutter × max gain the gain ceiling holds.
        let (shutter, gain) = split_exposure(1e9_f64.log2(), 100, 2_000_000, 8.0);
        assert_eq!((shutter, gain), (2_000_000, 8.0));
        // Below the minimum: minimum shutter, gain 1.
        assert_eq!(
            split_exposure(10.0_f64.log2(), 100, 5_000_000, 8.0),
            (100, 1.0)
        );
        // Regression: a shutter rounded down must not leave gain ~1.0004.
        assert_eq!(
            split_exposure(998.4_f64.log2(), 100, 5_000_000, 8.0),
            (998, 1.0)
        );
    }

    #[test]
    fn interval_budget_caps_the_shutter() {
        let settings = RampSettings::default();
        let cap = |seconds| max_shutter_for_gap(Some(Duration::seconds(seconds)), &settings);
        assert_eq!(cap(30), 2_045_454);
        assert_eq!(cap(60), 4_227_272);
        assert_eq!(cap(600), 5_000_000);
        assert_eq!(cap(1), 100);
        assert_eq!(max_shutter_for_gap(None, &settings), 5_000_000);
    }

    #[test]
    fn settings_validation_rejects_each_out_of_range_field() {
        assert!(RampSettings::default().validate().is_ok());
        let bad = [
            RampSettings {
                min_shutter_us: 50,
                ..RampSettings::default()
            },
            RampSettings {
                max_shutter_us: 6_000_000,
                ..RampSettings::default()
            },
            RampSettings {
                min_shutter_us: 2_000,
                max_shutter_us: 1_000,
                ..RampSettings::default()
            },
            RampSettings {
                max_gain: 0.5,
                ..RampSettings::default()
            },
            RampSettings {
                max_gain: 17.0,
                ..RampSettings::default()
            },
            RampSettings {
                day_bias_ev: 4.0,
                ..RampSettings::default()
            },
            RampSettings {
                night_drop_ev: -1.0,
                ..RampSettings::default()
            },
            RampSettings {
                max_step_ev: 0.0,
                ..RampSettings::default()
            },
            RampSettings {
                smoothing: 1.0,
                ..RampSettings::default()
            },
            RampSettings {
                day_bias_ev: f64::NAN,
                ..RampSettings::default()
            },
            RampSettings {
                clip_budget_percent: -0.1,
                ..RampSettings::default()
            },
            RampSettings {
                clip_budget_percent: 10.5,
                ..RampSettings::default()
            },
        ];
        for budget in [0.0, 0.25, 10.0] {
            let settings = RampSettings {
                clip_budget_percent: budget,
                ..RampSettings::default()
            };
            assert!(settings.validate().is_ok(), "{budget}");
        }
        for settings in bad {
            assert!(settings.validate().is_err(), "{settings:?}");
        }
    }

    #[test]
    fn sanitized_clamps_hand_edited_values_into_range() {
        let wild = RampSettings {
            min_shutter_us: 1,
            max_shutter_us: 60_000_000,
            max_gain: f32::NAN,
            day_bias_ev: 10.0,
            night_drop_ev: f64::INFINITY,
            max_step_ev: 0.0,
            smoothing: 2.0,
            clip_budget_percent: 50.0,
        };
        let clean = wild.sanitized();
        assert!(clean.validate().is_ok(), "{clean:?}");
        assert_eq!(clean.max_shutter_us, MAX_SHUTTER_US);
        assert_eq!(clean.max_gain, RampSettings::default().max_gain);
        assert_eq!(clean.clip_budget_percent, MAX_CLIP_BUDGET_PERCENT);
        let nan_budget = RampSettings {
            clip_budget_percent: f64::NAN,
            ..RampSettings::default()
        };
        assert_eq!(nan_budget.sanitized().clip_budget_percent, 1.0);
    }

    #[test]
    fn exposure_mode_defaults_to_dashboard_and_round_trips() {
        assert_eq!(ScheduleExposure::default(), ScheduleExposure::Dashboard);
        let json = r#"{"mode":"AutoRamp","max_gain":4.0}"#;
        let parsed: ScheduleExposure = serde_json::from_str(json).unwrap();
        let ScheduleExposure::AutoRamp(settings) = &parsed else {
            panic!("expected AutoRamp");
        };
        assert_eq!(settings.max_gain, 4.0);
        assert_eq!(settings.night_drop_ev, 2.0);
        // A config saved before the highlight guard existed gets its 1% default.
        assert_eq!(settings.clip_budget_percent, 1.0);
        let again: ScheduleExposure =
            serde_json::from_str(&serde_json::to_string(&parsed).unwrap()).unwrap();
        assert_eq!(again, parsed);
    }

    /// A state last updated by a capture at `updated_at`.
    fn seeded(scene_ev: f64, updated_at: DateTime<Utc>) -> RampState {
        RampState {
            updated_at,
            captured_at: Some(updated_at),
            scene_ev,
            planned_log2_exposure: None,
            highlight: HighlightGuard::default(),
        }
    }

    #[test]
    fn no_state_or_a_stale_state_plans_a_seed() {
        let settings = RampSettings::default();
        assert_eq!(
            plan(None, at(0), None, &settings, 5_000_000),
            RampPlan::Seed
        );
        let state = seeded(-12.0, at(0));
        assert_eq!(
            plan(Some(&state), at(31), None, &settings, 5_000_000),
            RampPlan::Seed
        );
        assert!(matches!(
            plan(Some(&state), at(30), None, &settings, 5_000_000),
            RampPlan::Manual { .. }
        ));
    }

    #[test]
    fn a_seed_observation_starts_the_ramp() {
        let settings = RampSettings::default();
        let observation = FrameObservation {
            exposure_us: 1_000.0,
            analogue_gain: 1.0,
            meter: meter(MID_GREY, None),
        };
        let state = observe(None, &RampPlan::Seed, &observation, at(0), None, &settings).unwrap();
        assert!((state.scene_ev - (MID_GREY.log2() - 1_000_f64.log2())).abs() < 1e-9);
        // No capture anchor yet, so the next plan may jump to the target.
        assert_eq!(state.planned_log2_exposure, None);
    }

    #[test]
    fn first_frame_after_a_seed_jumps_then_frames_are_step_limited() {
        let settings = RampSettings::default();
        // Seed metered 3 EV too dark at 0.5 s (AE's frame-duration cap at night).
        let scene_ev = (MID_GREY / 8.0).log2() - 500_000_f64.log2();
        let state = seeded(scene_ev, at(0));
        let RampPlan::Manual {
            log2_exposure,
            shutter_us,
            gain,
            ..
        } = plan(Some(&state), at(1), None, &settings, 5_000_000)
        else {
            panic!("expected manual plan");
        };
        assert!((log2_exposure - 4_000_000_f64.log2()).abs() < 1e-6);
        assert_eq!((shutter_us, gain), (4_000_000, 1.0));

        let stepped = RampState {
            planned_log2_exposure: Some(500_000_f64.log2()),
            ..state
        };
        let RampPlan::Manual { log2_exposure, .. } =
            plan(Some(&stepped), at(1), None, &settings, 5_000_000)
        else {
            panic!("expected manual plan");
        };
        assert!((log2_exposure - (500_000_f64.log2() + 1.0 / 3.0)).abs() < 1e-5);
    }

    /// Scene brightness `log2(luminance) − log2(shutter_us × gain)` for a
    /// sun elevation: bright daylight down to a dark, moonless-ish night.
    fn simulated_scene_ev(elevation: f64) -> f64 {
        let day = -12.4;
        let night = -29.0;
        let t = ((10.0 - elevation) / 28.0).clamp(0.0, 1.0);
        day + (night - day) * t
    }

    fn simulated_luminance(scene_ev: f64, log2_exposure: f64) -> f64 {
        (scene_ev + log2_exposure).exp2().clamp(1e-5, 1.0)
    }

    #[test]
    fn simulated_sunset_converges_smoothly_to_the_night_target() {
        let settings = RampSettings::default();
        let interval = Duration::seconds(30);
        let max_shutter = max_shutter_for_gap(Some(interval), &settings);
        let max_log2 = (max_shutter as f64 * f64::from(settings.max_gain)).log2();

        // Seed at +10°: AE lands on mid-grey (capped at 0.5 s).
        let elevation_at = |frame: i64| 10.0 - 0.125 * frame as f64;
        let scene = simulated_scene_ev(elevation_at(0));
        let seed_exposure = (MID_GREY.log2() - scene).min(500_000_f64.log2());
        let mut state = observe(
            None,
            &RampPlan::Seed,
            &FrameObservation {
                exposure_us: seed_exposure.exp2(),
                analogue_gain: 1.0,
                meter: meter(simulated_luminance(scene, seed_exposure), None),
            },
            at(0),
            Some(elevation_at(0)),
            &settings,
        )
        .unwrap();

        let mut previous: Option<f64> = None;
        let mut previous_gain = 1.0_f32;
        let mut on_target_checks = 0;
        for frame in 1..=320 {
            let now = at(0) + interval * frame as i32;
            let elevation = elevation_at(frame);
            let step = plan(Some(&state), now, Some(elevation), &settings, max_shutter);
            let RampPlan::Manual {
                shutter_us,
                gain,
                log2_exposure,
                ..
            } = step
            else {
                panic!("unexpected seed at frame {frame}");
            };
            assert!(shutter_us <= max_shutter);
            if let Some(previous) = previous {
                // Smooth: monotonic (no oscillation) and within the step limit.
                assert!(
                    log2_exposure >= previous - 1e-9,
                    "frame {frame} went darker"
                );
                assert!(log2_exposure - previous <= settings.max_step_ev + 1e-6);
            }
            // Gain only rises once the shutter is at its ceiling.
            if gain > 1.0 {
                assert_eq!(shutter_us, max_shutter, "frame {frame}");
            }
            assert!(gain >= previous_gain);
            previous_gain = gain;
            previous = Some(log2_exposure);

            let scene = simulated_scene_ev(elevation);
            let luminance = simulated_luminance(scene, log2_exposure);
            if frame > 10 && log2_exposure < max_log2 - 1e-6 {
                let target = target_luminance(target_bias_ev(Some(elevation), &settings));
                let error = (luminance / target).log2().abs();
                assert!(error < 0.5, "frame {frame}: {error} EV off target");
                on_target_checks += 1;
            }
            state = observe(
                Some(&state),
                &step,
                &FrameObservation {
                    exposure_us: shutter_us as f64,
                    analogue_gain: f64::from(gain),
                    meter: meter(luminance, None),
                },
                now,
                Some(elevation),
                &settings,
            )
            .unwrap();
        }
        assert!(on_target_checks > 150, "{on_target_checks}");
        // Night: the ramp ends pinned at max shutter × max gain.
        assert!((previous.unwrap() - max_log2).abs() < 1e-6);
    }

    #[test]
    fn smoothing_damps_a_single_bright_outlier_frame() {
        let settings = RampSettings::default();
        let exposure = 20_000_f64.log2();
        let scene = MID_GREY.log2() - exposure;
        let state = RampState {
            planned_log2_exposure: Some(exposure),
            ..seeded(scene, at(0))
        };
        let step = plan(Some(&state), at(1), None, &settings, 5_000_000);
        // Headlights: this one frame meters 3 EV brighter.
        let bright = observe(
            Some(&state),
            &step,
            &FrameObservation {
                exposure_us: 20_000.0,
                analogue_gain: 1.0,
                meter: meter(MID_GREY * 8.0, None),
            },
            at(1),
            None,
            &settings,
        )
        .unwrap();
        assert!((bright.scene_ev - (scene + 1.5)).abs() < 1e-9);
        let RampPlan::Manual { log2_exposure, .. } =
            plan(Some(&bright), at(2), None, &settings, 5_000_000)
        else {
            panic!("expected manual plan");
        };
        // Only one step darker, not the full 3 EV.
        assert!((exposure - log2_exposure - settings.max_step_ev).abs() < 1e-3);
    }

    #[test]
    fn preview_frames_seed_the_ramp_and_keep_the_capture_step_anchor() {
        let settings = RampSettings::default();
        let observation = FrameObservation {
            exposure_us: 2_000.0,
            analogue_gain: 1.0,
            meter: meter(MID_GREY, None),
        };
        // No state: the preview seeds it, with no capture anchor.
        let seeded_state = observe_preview(None, &observation, at(0), &settings).unwrap();
        assert!((seeded_state.scene_ev - (MID_GREY.log2() - 2_000_f64.log2())).abs() < 1e-9);
        assert_eq!(seeded_state.planned_log2_exposure, None);

        // With a capture-driven state, the anchor survives so a running
        // timelapse keeps its 1/3 EV step limit (design doc §11).
        let running = RampState {
            planned_log2_exposure: Some(18.0),
            ..seeded(-20.0, at(0))
        };
        let brighter = FrameObservation {
            meter: meter(MID_GREY * 4.0, None),
            ..observation
        };
        let one_second = at(0) + Duration::seconds(1);
        let next = observe_preview(Some(&running), &brighter, one_second, &settings).unwrap();
        assert_eq!(next.planned_log2_exposure, Some(18.0));
        // Smoothed toward the newly measured scene by elapsed time, not
        // jumped (1 s of a 5 s time constant).
        let measured = (MID_GREY * 4.0).log2() - 2_000_f64.log2();
        let blend = 1.0 - (-1.0 / PREVIEW_LEARNING_TIME_CONSTANT_S).exp();
        assert!((next.scene_ev - (-20.0 + blend * (measured + 20.0))).abs() < 1e-9);
    }

    #[test]
    fn a_preview_seeded_ramp_previews_without_a_step_limit_but_captures_meter_first() {
        // The preview's plan after preview seeding may jump straight to the
        // target: there is no capture sequence to keep smooth yet.
        let settings = RampSettings::default();
        let observation = FrameObservation {
            exposure_us: 2_000.0,
            analogue_gain: 1.0,
            meter: meter(MID_GREY / 16.0, None),
        };
        let state = observe_preview(None, &observation, at(0), &settings).unwrap();
        assert_eq!(state.captured_at, None);
        // A scheduled capture never plans from preview frames alone
        // (worklogs/2026-09-28-ramp-overexposure.md).
        let decision = decide(Some(&state), at(1), None, &settings, 5_000_000);
        assert_eq!(decision.plan, RampPlan::Seed);
        assert_eq!(decision.seed_reason, Some(SeedReason::NoCapture));
        let RampPlan::Manual { log2_exposure, .. } =
            preview_plan(Some(&state), at(1), None, &settings, 5_000_000)
        else {
            panic!("expected a manual preview plan");
        };
        // Metered 4 EV dark at 2 ms -> 32 ms.
        assert!(
            (log2_exposure - 32_000_f64.log2()).abs() < 1e-3,
            "{log2_exposure}"
        );
    }

    // --- Highlight guard (design doc §5.6) ---

    /// Night sun elevation, well below the guard's 0° gate.
    const NIGHT_SUN: Option<f64> = Some(-25.0);
    /// 60 s interval shutter cap, as on the Pi on 2026-09-22.
    const NIGHT_MAX_SHUTTER_US: u64 = 4_227_272;
    /// Measured night scene brightness on 2026-09-22 (log2 L − log2 E).
    const NIGHT_SCENE_EV: f64 = -26.0;

    fn clip_meter(luminance: f64, clipped_fraction: f64) -> FrameMeter {
        FrameMeter {
            clipped_fraction,
            ..meter(luminance, None)
        }
    }

    /// A running night ramp at the unguarded night target.
    fn night_state(highlight: HighlightGuard) -> RampState {
        let settings = RampSettings::default();
        let target = target_luminance(target_bias_ev(NIGHT_SUN, &settings)).log2();
        RampState {
            planned_log2_exposure: Some(target - NIGHT_SCENE_EV),
            highlight,
            ..seeded(NIGHT_SCENE_EV, at(0))
        }
    }

    /// Plans and captures one frame of a static scene. `clip_at` gives the
    /// clipped fraction at the exposure the camera actually used;
    /// `fault_ev` makes the camera expose that much darker than planned.
    fn capture_frame(
        state: &RampState,
        settings: &RampSettings,
        sun: Option<f64>,
        fault_ev: f64,
        clip_at: impl Fn(f64) -> f64,
    ) -> (f64, RampState) {
        let step = plan(Some(state), at(1), sun, settings, NIGHT_MAX_SHUTTER_US);
        let RampPlan::Manual { log2_exposure, .. } = step else {
            panic!("expected a manual plan");
        };
        let actual = log2_exposure - fault_ev;
        let observation = FrameObservation {
            exposure_us: actual.exp2(),
            analogue_gain: 1.0,
            meter: clip_meter(
                (state.scene_ev + actual).exp2().min(1.0),
                clip_at(actual).min(1.0),
            ),
        };
        let next = observe(Some(state), &step, &observation, at(1), sun, settings).unwrap();
        (log2_exposure, next)
    }

    #[test]
    fn guard_pulls_after_two_over_budget_frames() {
        let settings = RampSettings::default();
        let state = night_state(HighlightGuard::default());
        // Last night's steady 2.65% clipped, far over the 1% budget.
        let (_, first) = capture_frame(&state, &settings, NIGHT_SUN, 0.0, |_| 0.0265);
        assert_eq!(first.highlight.offset_ev, 0.0, "one frame is not enough");
        let (_, second) = capture_frame(&first, &settings, NIGHT_SUN, 0.0, |_| 0.0265);
        // Excess log2(2.65) = 1.41 EV, capped at 1 EV per frame.
        assert!((second.highlight.offset_ev + GUARD_MAX_PULL_EV).abs() < 1e-9);
        assert!((second.highlight.last_pull_ev - GUARD_MAX_PULL_EV).abs() < 1e-9);
    }

    #[test]
    fn guard_pull_is_not_slowed_by_the_step_limit() {
        let settings = RampSettings::default();
        let pulled = night_state(HighlightGuard {
            offset_ev: -1.0,
            last_pull_ev: 1.0,
            previous_excess_ev: Some(1.4),
            clip_scene_ev: None,
        });
        let unguarded = night_state(HighlightGuard::default());
        let (guarded_ev, _) = capture_frame(&pulled, &settings, NIGHT_SUN, 0.0, |_| 0.0);
        let (unguarded_ev, _) = capture_frame(&unguarded, &settings, NIGHT_SUN, 0.0, |_| 0.0);
        assert!(
            (unguarded_ev - guarded_ev - 1.0).abs() < 1e-3,
            "{guarded_ev} vs {unguarded_ev}"
        );
        // Without the widening, max_step_ev (1/3) would have capped it.
        assert!(unguarded_ev - guarded_ev > settings.max_step_ev + 0.1);
    }

    #[test]
    fn guard_darkens_from_the_exposure_ceiling_in_a_very_dark_scene() {
        // So dark the unguarded ramp wants 5 EV more than max shutter × max
        // gain: the offset must still darken the frame actually shot.
        let settings = RampSettings::default();
        let ceiling = (NIGHT_MAX_SHUTTER_US as f64 * f64::from(settings.max_gain)).log2();
        let target = target_luminance(target_bias_ev(NIGHT_SUN, &settings)).log2();
        let state = RampState {
            scene_ev: target - ceiling - 5.0,
            planned_log2_exposure: Some(ceiling - 1.0),
            ..night_state(HighlightGuard {
                offset_ev: -1.0,
                last_pull_ev: 1.0,
                previous_excess_ev: Some(1.0),
                clip_scene_ev: None,
            })
        };
        let (planned, _) = capture_frame(&state, &settings, NIGHT_SUN, 0.0, |_| 0.0);
        assert!(
            (planned - (ceiling - 1.0)).abs() < 1e-5,
            "{planned} vs {ceiling}"
        );
    }

    #[test]
    fn guard_ignores_a_single_bright_frame_but_catches_a_sustained_rise() {
        let settings = RampSettings::default();
        let mut state = night_state(HighlightGuard::default());
        let reference = state.planned_log2_exposure.unwrap();
        // 0.8% at the reference exposure: inside the hold band.
        let steady = |exposure: f64| 0.008 * (exposure - reference).exp2();
        let headlight = |exposure: f64| 3.0 * steady(exposure);
        let mut planned = Vec::new();
        for frame in 0..12 {
            let (exposure, next) = if frame == 6 {
                capture_frame(&state, &settings, NIGHT_SUN, 0.0, headlight)
            } else {
                capture_frame(&state, &settings, NIGHT_SUN, 0.0, steady)
            };
            planned.push(exposure);
            state = next;
        }
        // Identical frames (the plan rounds the shutter to whole µs).
        assert!((planned[0] - reference).abs() < 1e-5);
        assert!(planned.iter().all(|&e| e == planned[0]), "{planned:?}");
        assert_eq!(state.highlight.offset_ev, 0.0);

        // The same ×3 held: the frame after the second one is darker.
        let mut exposures = Vec::new();
        for _ in 0..3 {
            let (exposure, next) = capture_frame(&state, &settings, NIGHT_SUN, 0.0, headlight);
            exposures.push(exposure);
            state = next;
        }
        assert!(exposures[1] - exposures[2] > 0.5, "{exposures:?}");
    }

    #[test]
    fn guard_stops_at_its_floor_holds_in_band_and_recovers_slowly() {
        let settings = RampSettings::default();
        // A lamp shining into the lens: half the frame clips at any exposure.
        let mut state = night_state(HighlightGuard::default());
        for _ in 0..20 {
            state = capture_frame(&state, &settings, NIGHT_SUN, 0.0, |_| 0.5).1;
            assert!(state.highlight.offset_ev >= GUARD_FLOOR_EV);
            assert!(state.highlight.last_pull_ev <= GUARD_MAX_PULL_EV);
        }
        assert_eq!(state.highlight.offset_ev, GUARD_FLOOR_EV);

        // Between budget/2 and budget: hold.
        for _ in 0..5 {
            state = capture_frame(&state, &settings, NIGHT_SUN, 0.0, |_| 0.007).1;
            assert_eq!(state.highlight.offset_ev, GUARD_FLOOR_EV);
        }
        // Well under budget: back 1/12 EV per frame, never above 0.
        for frame in 1..=40 {
            state = capture_frame(&state, &settings, NIGHT_SUN, 0.0, |_| 0.001).1;
            let expected = (GUARD_FLOOR_EV + f64::from(frame) * GUARD_RECOVERY_EV).min(0.0);
            assert!((state.highlight.offset_ev - expected).abs() < 1e-9);
        }
        assert_eq!(state.highlight.offset_ev, 0.0);
    }

    #[test]
    fn guard_never_brightens() {
        let guards = [0.0, -1.0, GUARD_FLOOR_EV]
            .into_iter()
            .flat_map(|offset_ev| {
                [None, Some(-2.0), Some(0.5), Some(3.0)].map(|previous_excess_ev| HighlightGuard {
                    offset_ev,
                    last_pull_ev: 0.0,
                    previous_excess_ev,
                    clip_scene_ev: None,
                })
            });
        let mut checked = 0;
        for guard in guards {
            for scene_ev in [-26.0, -20.0, -13.0] {
                for clip in [0.0, 0.001, 0.01, 0.05, 0.5] {
                    for sun in [None, Some(10.0), Some(-0.5), Some(-25.0)] {
                        for budget in [0.0, 0.25, 1.0, 10.0] {
                            for fault_ev in [0.0, 1.6, -1.0] {
                                let settings = RampSettings {
                                    clip_budget_percent: budget,
                                    ..RampSettings::default()
                                };
                                let state = RampState {
                                    highlight: guard,
                                    ..night_state(HighlightGuard::default())
                                };
                                let state = RampState { scene_ev, ..state };
                                let (_, next) =
                                    capture_frame(&state, &settings, sun, fault_ev, |_| clip);
                                let offset = next.highlight.offset_ev;
                                assert!((GUARD_FLOOR_EV..=0.0).contains(&offset), "{offset}");
                                let unguarded = RampState {
                                    highlight: HighlightGuard::default(),
                                    ..next
                                };
                                let guarded_ev =
                                    capture_frame(&next, &settings, sun, 0.0, |_| 0.0).0;
                                let unguarded_ev =
                                    capture_frame(&unguarded, &settings, sun, 0.0, |_| 0.0).0;
                                assert!(guarded_ev <= unguarded_ev + 1e-9);
                                checked += 1;
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(checked, 12 * 3 * 5 * 4 * 4 * 3);
    }

    #[test]
    fn guard_is_inactive_in_daylight_without_a_station_or_with_zero_budget() {
        let zero_budget = RampSettings {
            clip_budget_percent: 0.0,
            ..RampSettings::default()
        };
        let cases = [
            (Some(10.0), RampSettings::default()),
            (Some(0.0), RampSettings::default()),
            (None, RampSettings::default()),
            (NIGHT_SUN, zero_budget),
        ];
        for (sun, settings) in cases {
            let mut state = night_state(HighlightGuard::default());
            let mut reference = night_state(HighlightGuard::default());
            // 5% clipped every frame: bright sky and glass.
            for _ in 0..20 {
                let (guarded_ev, next) = capture_frame(&state, &settings, sun, 0.0, |_| 0.05);
                let (reference_ev, next_reference) =
                    capture_frame(&reference, &settings, sun, 0.0, |_| 0.0);
                assert_eq!(guarded_ev, reference_ev, "sun {sun:?}");
                assert_eq!(next.highlight.offset_ev, 0.0);
                state = next;
                reference = next_reference;
            }
        }
        // A guard left over from the night eases off after sunrise instead
        // of jumping (the step limit would cap a jump anyway).
        let settings = RampSettings::default();
        let dawn = night_state(HighlightGuard {
            offset_ev: -1.5,
            ..HighlightGuard::default()
        });
        let (_, next) = capture_frame(&dawn, &settings, Some(1.0), 0.0, |_| 0.05);
        assert!((next.highlight.offset_ev - (-1.5 + GUARD_RECOVERY_EV)).abs() < 1e-9);
        // Budget 0 switches it off outright.
        let (_, off) = capture_frame(&dawn, &zero_budget, NIGHT_SUN, 0.0, |_| 0.05);
        assert_eq!(
            HighlightGuard {
                clip_scene_ev: None,
                ..off.highlight
            },
            HighlightGuard::default()
        );
    }

    #[test]
    fn guard_judges_clipping_at_the_planned_exposure() {
        // 2026-09-22 had frames 1.6 EV darker than planned. Such a frame
        // clips less, but must not look like a reason to recover or pull.
        let settings = RampSettings::default();
        let guarded = night_state(HighlightGuard {
            offset_ev: -1.0,
            ..HighlightGuard::default()
        });
        // Already settled 1 EV under the unguarded plan.
        let reference = guarded.planned_log2_exposure.unwrap() - 1.0;
        let mut state = RampState {
            planned_log2_exposure: Some(reference),
            ..guarded
        };
        // 0.8% at the planned exposure: inside the hold band.
        let clip_at = |exposure: f64| 0.008 * (exposure - reference).exp2();
        for fault_ev in [1.6, -1.0, 0.0, 1.6] {
            state = capture_frame(&state, &settings, NIGHT_SUN, fault_ev, clip_at).1;
            assert_eq!(state.highlight.offset_ev, -1.0, "fault {fault_ev}");
        }
    }

    #[test]
    fn preview_frames_leave_the_guard_alone() {
        let settings = RampSettings::default();
        let guard = HighlightGuard {
            offset_ev: -1.5,
            last_pull_ev: 1.0,
            previous_excess_ev: Some(0.3),
            clip_scene_ev: Some(-30.0),
        };
        let state = night_state(guard);
        for clip in [0.0, 0.5] {
            let observation = FrameObservation {
                exposure_us: 118_750.0,
                analogue_gain: 16.0,
                meter: clip_meter(0.04, clip),
            };
            let two_seconds = at(0) + Duration::seconds(2);
            let next = observe_preview(Some(&state), &observation, two_seconds, &settings).unwrap();
            // The timelapse offset and its confirmation state are untouched
            // (§5.6); only the clip estimate learns (§5.7).
            assert_eq!(
                HighlightGuard {
                    clip_scene_ev: guard.clip_scene_ev,
                    ..next.highlight
                },
                guard
            );
            assert_eq!(next.planned_log2_exposure, state.planned_log2_exposure);
            let sample = clip.max(CLIP_ESTIMATE_FLOOR).log2() - (118_750.0_f64 * 16.0).log2();
            let blend = 1.0 - (-2.0 / PREVIEW_LEARNING_TIME_CONSTANT_S).exp();
            let expected = -30.0 + blend * (sample + 30.0);
            assert!((next.highlight.clip_scene_ev.unwrap() - expected).abs() < 1e-9);
        }
    }

    /// Measured on 2026-09-22 21:10–22:40 PDT (one frame per minute, sun
    /// −20° to −32°), from `worklogs/2026-09-23-highlight-guard.md`: planned
    /// log2(shutter_us × gain), trimmed log-mean luminance, and clipped
    /// fraction. Room lights reflected in the window went off at 21:16 and
    /// 21:22, and the ramp opened 2.3 EV into bloom.
    const NIGHT_2026_09_22: [(f64, f64, f64); 91] = [
        (19.278, 0.04365, 0.00346),
        (19.299, 0.04630, 0.00361),
        (19.277, 0.04391, 0.00350),
        (19.293, 0.04627, 0.00373),
        (19.272, 0.04260, 0.00352),
        (19.310, 0.04510, 0.00371),
        (19.307, 0.01546, 0.00342),
        (19.640, 0.02339, 0.00487),
        (19.973, 0.03246, 0.00750),
        (20.307, 0.03627, 0.00964),
        (20.501, 0.04983, 0.01090),
        (20.427, 0.04575, 0.01048),
        (20.414, 0.01185, 0.01015),
        (20.747, 0.01796, 0.01341),
        (21.080, 0.02642, 0.01685),
        (21.414, 0.03817, 0.02283),
        (21.719, 0.05273, 0.02913),
        (21.605, 0.04652, 0.02607),
        (21.581, 0.04501, 0.02476),
        (21.581, 0.04537, 0.02520),
        (21.574, 0.04441, 0.02516),
        (21.584, 0.04631, 0.02607),
        (21.563, 0.04347, 0.02482),
        (21.589, 0.04690, 0.02659),
        (21.559, 0.04487, 0.02480),
        (21.561, 0.04848, 0.02590),
        (21.507, 0.04374, 0.02453),
        (21.527, 0.04316, 0.02437),
        (21.557, 0.04533, 0.02484),
        (21.552, 0.04474, 0.02501),
        (21.556, 0.04553, 0.02491),
        (21.547, 0.04451, 0.02412),
        (21.556, 0.04855, 0.02464),
        (21.501, 0.04451, 0.02389),
        (21.509, 0.13025, 0.02561),
        (21.175, 0.09534, 0.01944),
        (20.842, 0.07136, 0.01436),
        (20.509, 0.01707, 0.00416),
        (20.600, 0.05633, 0.01206),
        (20.436, 0.04634, 0.01051),
        (20.415, 0.04554, 0.01032),
        (20.405, 0.04330, 0.01009),
        (20.432, 0.04661, 0.01055),
        (20.405, 0.04470, 0.01030),
        (20.409, 0.04522, 0.01036),
        (20.405, 0.04390, 0.01026),
        (20.422, 0.04584, 0.01055),
        (20.408, 0.04394, 0.01019),
        (20.424, 0.01239, 0.01038),
        (20.757, 0.01802, 0.01295),
        (21.091, 0.02607, 0.01654),
        (21.424, 0.03591, 0.02056),
        (21.757, 0.05181, 0.02913),
        (21.663, 0.04540, 0.02542),
        (21.656, 0.04463, 0.02470),
        (21.662, 0.04593, 0.02578),
        (21.647, 0.04666, 0.02542),
        (21.620, 0.04215, 0.02404),
        (21.668, 0.04517, 0.02580),
        (21.664, 0.04421, 0.02536),
        (21.677, 0.04517, 0.02598),
        (21.674, 0.04479, 0.02605),
        (21.677, 0.04490, 0.02559),
        (21.678, 0.04449, 0.02569),
        (21.686, 0.04434, 0.02636),
        (21.697, 0.04475, 0.02642),
        (21.700, 0.04449, 0.02636),
        (21.708, 0.04457, 0.02669),
        (21.715, 0.04442, 0.02712),
        (21.724, 0.04552, 0.02750),
        (21.716, 0.04432, 0.02677),
        (21.726, 0.04635, 0.02750),
        (21.705, 0.04418, 0.02630),
        (21.717, 0.04798, 0.02729),
        (21.671, 0.04348, 0.02538),
        (21.695, 0.04431, 0.02625),
        (21.706, 0.04473, 0.02656),
        (21.710, 0.04481, 0.02675),
        (21.712, 0.04665, 0.02706),
        (21.686, 0.04318, 0.02526),
        (21.716, 0.04544, 0.02700),
        (21.708, 0.04419, 0.02598),
        (21.721, 0.04558, 0.02681),
        (21.711, 0.04435, 0.02549),
        (21.722, 0.04579, 0.02656),
        (21.709, 0.04545, 0.02559),
        (21.701, 0.04474, 0.02511),
        (21.705, 0.04515, 0.02582),
        (21.702, 0.04641, 0.02536),
        (21.680, 0.04396, 0.02472),
        (21.697, 0.04602, 0.02538),
    ];

    /// Replays the measured night closed-loop: each frame keeps its measured
    /// scene brightness, and its clipped fraction scales with the planned
    /// exposure at about 1 doubling per EV (measured 0.8–1.2).
    fn replay_night(settings: &RampSettings) -> Vec<(f64, f64)> {
        let (first_ev, first_luminance, _) = NIGHT_2026_09_22[0];
        let mut state = RampState {
            planned_log2_exposure: Some(first_ev),
            ..seeded(first_luminance.log2() - first_ev, at(0))
        };
        let mut frames = Vec::new();
        for &(measured_ev, luminance, clipped) in &NIGHT_2026_09_22[1..] {
            let scene_ev = luminance.log2() - measured_ev;
            let step = plan(
                Some(&state),
                at(1),
                NIGHT_SUN,
                settings,
                NIGHT_MAX_SHUTTER_US,
            );
            let RampPlan::Manual { log2_exposure, .. } = step else {
                panic!("expected a manual plan");
            };
            let clip = (clipped * (log2_exposure - measured_ev).exp2()).min(1.0);
            let observation = FrameObservation {
                exposure_us: log2_exposure.exp2(),
                analogue_gain: 1.0,
                meter: clip_meter((scene_ev + log2_exposure).exp2().min(1.0), clip),
            };
            state = observe(
                Some(&state),
                &step,
                &observation,
                at(1),
                NIGHT_SUN,
                settings,
            )
            .unwrap();
            frames.push((log2_exposure, clip));
        }
        frames
    }

    #[test]
    fn replay_2026_09_22_night_with_and_without_the_guard() {
        let mean = |values: &mut dyn Iterator<Item = f64>| {
            let values: Vec<f64> = values.collect();
            values.iter().sum::<f64>() / values.len() as f64
        };
        let off = replay_night(&RampSettings {
            clip_budget_percent: 0.0,
            ..RampSettings::default()
        });
        let on = replay_night(&RampSettings::default());
        // The last 30 frames (22:11–22:40) are the settled night.
        let settled = on.len() - 30;

        // Guard off reproduces what the Pi did: same exposure, same bloom.
        let measured = mean(&mut NIGHT_2026_09_22[settled + 1..].iter().map(|row| row.0));
        let off_ev = mean(&mut off[settled..].iter().map(|frame| frame.0));
        assert!((off_ev - measured).abs() < 0.35, "{off_ev} vs {measured}");
        assert!(mean(&mut off[settled..].iter().map(|frame| frame.1)) > 0.02);

        // Guard on: at least 1 EV darker, clipping near budget, stable.
        let on_ev = mean(&mut on[settled..].iter().map(|frame| frame.0));
        assert!(off_ev - on_ev >= 1.0, "only {} EV darker", off_ev - on_ev);
        for window in on[settled..].windows(2) {
            assert!(window[1].1 <= 0.015, "clipped {}", window[1].1);
            assert!((window[1].0 - window[0].0).abs() < 0.2, "{window:?}");
        }
        // And never brighter than the unguarded ramp at any frame.
        for (guarded, unguarded) in on.iter().zip(&off) {
            assert!(guarded.0 <= unguarded.0 + 1e-9);
        }
    }

    // --- Highlight guard in the preview (design doc §5.7) ---

    /// A night ramp whose clip estimate predicts `clipped` at the unguarded
    /// night exposure.
    fn night_state_clipping(clipped: f64) -> RampState {
        let state = night_state(HighlightGuard::default());
        let unguarded = state.planned_log2_exposure.unwrap();
        RampState {
            highlight: HighlightGuard {
                clip_scene_ev: Some(clipped.log2() - unguarded),
                ..HighlightGuard::default()
            },
            ..state
        }
    }

    fn with_budget(clip_budget_percent: f64) -> RampSettings {
        RampSettings {
            clip_budget_percent,
            ..RampSettings::default()
        }
    }

    fn log2_of(setting: ExposureSetting) -> f64 {
        (setting.shutter_us as f64 * f64::from(setting.gain)).log2()
    }

    #[test]
    fn clip_estimate_is_normalised_by_actual_exposure() {
        let settings = RampSettings::default();
        let estimate = |exposure_us: f64, clipped: f64| {
            let observation = FrameObservation {
                exposure_us,
                analogue_gain: 1.0,
                meter: clip_meter(0.04, clipped),
            };
            let preview = observe_preview(None, &observation, at(0), &settings).unwrap();
            let captured = observe(
                None,
                &RampPlan::Seed,
                &observation,
                at(0),
                NIGHT_SUN,
                &settings,
            )
            .unwrap();
            assert_eq!(
                preview.highlight.clip_scene_ev,
                captured.highlight.clip_scene_ev
            );
            preview.highlight.clip_scene_ev.unwrap()
        };
        // Half the exposure, half the clipping: the same scene.
        assert!((estimate(2_000_000.0, 0.02) - estimate(1_000_000.0, 0.01)).abs() < 1e-9);
        // A clip-free frame is floored, not −∞.
        assert!(estimate(1_000_000.0, 0.0).is_finite());
    }

    #[test]
    fn highlight_target_follows_the_budget_without_new_frames() {
        // Last night: 2.65% clipped at the unguarded night exposure.
        let state = night_state_clipping(0.0265);
        let target = |budget: f64| {
            highlight_target_ev(
                &state,
                NIGHT_SUN,
                &with_budget(budget),
                NIGHT_MAX_SHUTTER_US,
            )
        };
        assert!((target(1.0) - (0.01_f64 / 0.0265).log2()).abs() < 1e-9);
        assert!((target(0.5) - (0.005_f64 / 0.0265).log2()).abs() < 1e-9);
        assert_eq!(target(2.65), 0.0);
        assert_eq!(target(5.0), 0.0);
        assert_eq!(target(0.1), GUARD_FLOOR_EV);

        // The same state, two budgets: the preview moves by the difference.
        let preview = |budget: f64| {
            preview_plans(
                Some(&state),
                at(1),
                NIGHT_SUN,
                &with_budget(budget),
                NIGHT_MAX_SHUTTER_US,
            )
            .unwrap()
        };
        let loose = preview(5.0);
        let tight = preview(1.0);
        assert_eq!(loose.preview, loose.unguarded);
        let difference = log2_of(loose.preview) - log2_of(tight.preview);
        assert!((difference - 1.406).abs() < 1e-3, "{difference}");
    }

    #[test]
    fn highlight_target_is_zero_when_the_guard_cannot_act() {
        let state = night_state_clipping(0.05);
        let settings = RampSettings::default();
        for sun in [Some(0.0), Some(12.0), None] {
            assert_eq!(
                highlight_target_ev(&state, sun, &settings, NIGHT_MAX_SHUTTER_US),
                0.0
            );
        }
        assert_eq!(
            highlight_target_ev(&state, NIGHT_SUN, &with_budget(0.0), NIGHT_MAX_SHUTTER_US),
            0.0
        );
        let unmeasured = night_state(HighlightGuard::default());
        assert_eq!(
            highlight_target_ev(&unmeasured, NIGHT_SUN, &settings, NIGHT_MAX_SHUTTER_US),
            0.0
        );
        assert!(highlight_target_ev(&state, NIGHT_SUN, &settings, NIGHT_MAX_SHUTTER_US) < 0.0);
    }

    #[test]
    fn preview_plans_bracket_the_capture_plan() {
        let settings = RampSettings::default();
        let plan_of = |state: &RampState| {
            let RampPlan::Manual {
                shutter_us, gain, ..
            } = plan(
                Some(state),
                at(1),
                NIGHT_SUN,
                &settings,
                NIGHT_MAX_SHUTTER_US,
            )
            else {
                panic!("expected a manual plan");
            };
            ExposureSetting { shutter_us, gain }
        };
        // No clipping, no guard: the preview is exactly the next frame.
        let calm = night_state_clipping(0.001);
        let plans = preview_plans(
            Some(&calm),
            at(1),
            NIGHT_SUN,
            &settings,
            NIGHT_MAX_SHUTTER_US,
        )
        .unwrap();
        assert_eq!(plans.target_ev, 0.0);
        assert_eq!(plans.preview, plan_of(&calm));
        assert_eq!(plans.unguarded, plan_of(&calm));

        // The timelapse is at −0.5 EV, heading for −1.4: the preview shows
        // the target, the compare view the frame without any guard.
        let easing = RampState {
            highlight: HighlightGuard {
                offset_ev: -0.5,
                ..night_state_clipping(0.0265).highlight
            },
            ..night_state_clipping(0.0265)
        };
        let plans = preview_plans(
            Some(&easing),
            at(1),
            NIGHT_SUN,
            &settings,
            NIGHT_MAX_SHUTTER_US,
        )
        .unwrap();
        let next = log2_of(plan_of(&easing));
        assert!((log2_of(plans.unguarded) - (next + 0.5)).abs() < 1e-5);
        assert!((log2_of(plans.preview) - (next + 0.5 + plans.target_ev)).abs() < 1e-5);
        assert!(log2_of(plans.preview) <= log2_of(plans.unguarded));

        // A seed has no preview plan.
        assert!(preview_plans(None, at(1), NIGHT_SUN, &settings, NIGHT_MAX_SHUTTER_US).is_none());
    }

    /// Closed preview loop: each iteration the preview runs the planned
    /// preview exposure (optionally `shortfall_ev` short, as a night preview
    /// is), meters `clipped_at(actual)`, and folds the frame in.
    fn preview_loop(
        settings: &RampSettings,
        shortfall_ev: f64,
        iterations: usize,
        clipped_at: impl Fn(f64) -> f64,
    ) -> Vec<(f64, f64)> {
        let mut state = night_state(HighlightGuard::default());
        let mut trace = Vec::new();
        for iteration in 0..iterations {
            // One preview update per 3 s status poll.
            let now = at(0) + Duration::seconds(3 * (iteration as i64 + 1));
            let plans = preview_plans(Some(&state), now, NIGHT_SUN, settings, NIGHT_MAX_SHUTTER_US)
                .unwrap();
            let planned = log2_of(plans.preview);
            assert!(planned <= log2_of(plans.unguarded) + 1e-9);
            let actual = planned - shortfall_ev;
            let observation = FrameObservation {
                exposure_us: actual.exp2(),
                analogue_gain: 1.0,
                meter: clip_meter(
                    (state.scene_ev + actual).exp2().min(1.0),
                    clipped_at(actual).min(1.0),
                ),
            };
            state = observe_preview(Some(&state), &observation, now, settings).unwrap();
            trace.push((plans.target_ev, clipped_at(planned)));
        }
        trace
    }

    #[test]
    fn simulated_preview_loop_converges_to_the_budget() {
        let settings = RampSettings::default();
        let budget = settings.clip_budget_percent / 100.0;
        let unguarded = night_state(HighlightGuard::default())
            .planned_log2_exposure
            .unwrap();
        for slope in [0.8, 1.0, 1.2] {
            for shortfall_ev in [0.0, 0.8] {
                // 2.65% at the unguarded exposure, `slope` doublings per EV.
                let clipped_at = |exposure: f64| 0.0265 * (slope * (exposure - unguarded)).exp2();
                let trace = preview_loop(&settings, shortfall_ev, 20, clipped_at);
                for &(target_ev, clipped) in &trace[8..] {
                    let ratio = clipped / budget;
                    assert!(
                        (0.8..=1.2).contains(&ratio),
                        "slope {slope}, shortfall {shortfall_ev}: {ratio} at {target_ev}"
                    );
                }
            }
        }
        // Lamp cores that clip 2% at any exposure: the target stops at the
        // floor instead of darkening without end.
        let trace = preview_loop(&settings, 0.0, 20, |exposure| {
            (0.0265 * (exposure - unguarded).exp2()).max(0.02)
        });
        assert!(
            trace
                .iter()
                .all(|&(target_ev, _)| target_ev >= GUARD_FLOOR_EV)
        );
        assert_eq!(trace.last().unwrap().0, GUARD_FLOOR_EV);
    }

    // --- Preview learning over time (worklog 2026-09-23, Part 3) ---

    #[test]
    fn preview_learning_is_time_based() {
        let settings = RampSettings::default();
        let brighter = FrameObservation {
            exposure_us: 2_000.0,
            analogue_gain: 1.0,
            meter: clip_meter(MID_GREY * 4.0, 0.02),
        };
        let start = RampState {
            highlight: HighlightGuard {
                clip_scene_ev: Some(-30.0),
                ..HighlightGuard::default()
            },
            ..seeded(-20.0, at(0))
        };
        // Sixteen frames at 8 fps against one frame after the same 2 s.
        let mut burst = start;
        for frame in 1..=16 {
            let now = at(0) + Duration::milliseconds(125 * frame);
            burst = observe_preview(Some(&burst), &brighter, now, &settings).unwrap();
        }
        let once = observe_preview(
            Some(&start),
            &brighter,
            at(0) + Duration::seconds(2),
            &settings,
        )
        .unwrap();
        let moved = |state: &RampState| state.scene_ev - start.scene_ev;
        assert!((moved(&burst) / moved(&once) - 1.0).abs() < 0.01);
        let clip_moved = |state: &RampState| state.highlight.clip_scene_ev.unwrap() + 30.0;
        assert!((clip_moved(&burst) / clip_moved(&once) - 1.0).abs() < 0.01);
        // 2 s of a 5 s time constant: well short of a per-frame 0.5 jump.
        let measured = (MID_GREY * 4.0).log2() - 2_000_f64.log2();
        assert!(moved(&once) < 0.4 * (measured - start.scene_ev));
        // A frame stamped before the last update learns nothing.
        let stale = observe_preview(
            Some(&start),
            &brighter,
            at(0) - Duration::seconds(1),
            &settings,
        )
        .unwrap();
        assert_eq!(stale.scene_ev, start.scene_ev);
    }

    /// The live preview's closed loop as measured on 2026-09-23: the
    /// dashboard applies the planned preview exposure once per 3 s poll,
    /// 8 fps frames meter the scene `bias` EV brighter per EV of exposure
    /// above `reference` (the gain-dependent night metering), and
    /// `observe_preview` learns from every frame. Returns the planned
    /// preview exposure per poll.
    fn gain_biased_preview_loop(
        learn: impl Fn(&RampState, &FrameObservation, DateTime<Utc>) -> RampState,
        bias: f64,
        polls: usize,
    ) -> Vec<f64> {
        let settings = RampSettings::default();
        let mut state = night_state(HighlightGuard::default());
        let reference = state.planned_log2_exposure.unwrap();
        let true_scene = state.scene_ev;
        let mut applied = reference + 0.2;
        let mut trace = Vec::new();
        for poll in 0..polls {
            for frame in 0..24 {
                let now = at(0) + Duration::milliseconds(3_000 * poll as i64 + 125 * frame);
                let scene = true_scene + bias * (applied - reference);
                let observation = FrameObservation {
                    exposure_us: applied.exp2(),
                    analogue_gain: 1.0,
                    meter: clip_meter((scene + applied).exp2().min(1.0), 0.001),
                };
                state = learn(&state, &observation, now);
            }
            let now = at(0) + Duration::milliseconds(3_000 * (poll as i64 + 1));
            let plans = preview_plans(
                Some(&state),
                now,
                NIGHT_SUN,
                &settings,
                NIGHT_MAX_SHUTTER_US,
            )
            .unwrap();
            applied = log2_of(plans.preview);
            trace.push(applied);
        }
        trace
    }

    #[test]
    fn preview_loop_with_gain_dependent_metering_settles() {
        let settings = RampSettings::default();
        let swing = |trace: &[f64]| {
            let window = &trace[trace.len() - 10..];
            window.iter().cloned().fold(f64::MIN, f64::max)
                - window.iter().cloned().fold(f64::MAX, f64::min)
        };
        // Measured: ~1.2 EV brighter per EV of preview gain.
        let fixed = gain_biased_preview_loop(
            |state, observation, now| {
                observe_preview(Some(state), observation, now, &settings).unwrap()
            },
            1.2,
            40,
        );
        assert!(swing(&fixed) < 1.0 / 6.0, "still hunting: {fixed:?}");
        for pair in fixed[20..].windows(2) {
            assert!((pair[1] - pair[0]).abs() < 1.0 / 6.0, "{pair:?}");
        }

        // Regression guard: the old per-frame learning (weight 1 − smoothing
        // on every 8 fps frame) keeps hunting in the same loop.
        let per_frame = gain_biased_preview_loop(
            |state, observation, now| {
                let exposure = (observation.exposure_us * observation.analogue_gain).log2();
                let scene_ev = observation.meter.luminance.log2() - exposure;
                RampState {
                    updated_at: now,
                    scene_ev: state.scene_ev
                        + (1.0 - settings.smoothing) * (scene_ev - state.scene_ev),
                    ..*state
                }
            },
            1.2,
            40,
        );
        assert!(
            swing(&per_frame) >= 1.0 / 3.0,
            "old loop settled: {per_frame:?}"
        );
    }

    // --- 2026-09-28 08:00 dawn overexposure (worklogs/2026-09-28-ramp-overexposure.md) ---

    /// The preview override's ceiling: one 8 fps frame × 0.95, at 16× gain
    /// (`previewEquivalent` in `scheduled-exposure.js`).
    const PREVIEW_CEILING_US: f64 = 118_750.0;
    const PREVIEW_CEILING_GAIN: f64 = 16.0;
    /// Sun at the 08:00 PDT capture, from the station's ephemeris.
    const DAWN_SUN: Option<f64> = Some(7.6);

    fn pdt(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 28, hour + 7, minute, 0)
            .unwrap()
    }

    /// The scene over the night of 2026-09-27/28: the 2026-09-22 night level
    /// until 06:30, then brightening to about 45 ms mid-grey at 08:00 (the
    /// 2026-09-25 dawn at the same sun elevation).
    fn dawn_scene_ev(now: DateTime<Utc>) -> f64 {
        let dawn_start = pdt(6, 30);
        let day = MID_GREY.log2() - 45_000_f64.log2();
        let t = ((now - dawn_start).num_seconds() as f64 / 5_400.0).clamp(0.0, 1.0);
        NIGHT_SCENE_EV + t * (day - NIGHT_SCENE_EV)
    }

    /// A frame of `scene_ev` at `exposure_us` × `gain`, metered as the
    /// daemon would: a white frame's trimmed log-mean is `linear(255)`.
    fn frame_of(scene_ev: f64, exposure_us: f64, gain: f64) -> FrameObservation {
        let luminance = (scene_ev + (exposure_us * gain).log2()).exp2();
        let white = linear(255);
        FrameObservation {
            exposure_us,
            analogue_gain: gain,
            meter: clip_meter(
                luminance.min(white),
                if luminance >= white { 1.0 } else { 0.0 },
            ),
        }
    }

    /// The ramp as the Pi held it at 08:00: the 01:31 restart dropped all
    /// state, and 6.5 h of live preview frames at the preview override's
    /// ceiling (nothing re-sent it) rebuilt it. No capture in between.
    fn replay_2026_09_28_state(settings: &RampSettings) -> RampState {
        let mut state = None;
        let mut now = pdt(1, 31);
        while now < pdt(8, 0) {
            let frame = frame_of(dawn_scene_ev(now), PREVIEW_CEILING_US, PREVIEW_CEILING_GAIN);
            state = observe_preview(state.as_ref(), &frame, now, settings);
            now += Duration::seconds(2);
        }
        state.unwrap()
    }

    #[test]
    fn replay_2026_09_28_dawn_overexposure() {
        let settings = RampSettings::default();
        let state = replay_2026_09_28_state(&settings);
        // The preview only ever saw white frames at dawn, so the estimate
        // sits on their lower bound, about 3 EV darker than the real scene.
        let bound = linear(255).log2() - (PREVIEW_CEILING_US * PREVIEW_CEILING_GAIN).log2();
        assert!((state.scene_ev - bound).abs() < 0.01, "{}", state.scene_ev);
        assert!(dawn_scene_ev(pdt(8, 0)) - state.scene_ev > 2.5);
        assert_eq!(state.planned_log2_exposure, None);
        assert_eq!(state.captured_at, None);
        let max_shutter = max_shutter_for_gap(Some(Duration::hours(1)), &settings);

        // Before the fix the capture planned from this state. The preview
        // rule is that same rule, and without the daylight ceiling (no
        // station: same day-level target, no ceiling) it reproduces the
        // Pi's 343,529 µs at gain 1.0.
        let RampPlan::Manual {
            shutter_us, gain, ..
        } = preview_plan(Some(&state), pdt(8, 0), None, &settings, max_shutter)
        else {
            panic!("expected the old rule's manual plan");
        };
        assert_eq!(gain, 1.0);
        assert!(
            (shutter_us as f64 / 343_529.0 - 1.0).abs() < 0.02,
            "{shutter_us}"
        );
        // The daylight ceiling alone would have cut it to 180 ms at +7.6°.
        let RampPlan::Manual { shutter_us, .. } =
            preview_plan(Some(&state), pdt(8, 0), DAWN_SUN, &settings, max_shutter)
        else {
            panic!("expected a manual preview plan");
        };
        assert!((175_000..=185_000).contains(&shutter_us), "{shutter_us}");

        // Now: no capture since the restart, so the capture meters first.
        let decision = decide(Some(&state), pdt(8, 0), DAWN_SUN, &settings, max_shutter);
        assert_eq!(decision.plan, RampPlan::Seed);
        assert_eq!(decision.seed_reason, Some(SeedReason::NoCapture));

        // The metering still (auto exposure; the 09:00 frame's 7.2 ms
        // at gain 1 equivalent) measures the real scene, and the shot is
        // planned from it: the day target, about 45 ms, not 343 ms.
        let metering = frame_of(dawn_scene_ev(pdt(8, 0)), 635.0, 11.38);
        assert!(metering.meter.clipped_fraction == 0.0);
        let metered = observe(
            Some(&state),
            &RampPlan::Seed,
            &metering,
            pdt(8, 0),
            DAWN_SUN,
            &settings,
        )
        .unwrap();
        let decision = decide(Some(&metered), pdt(8, 0), DAWN_SUN, &settings, max_shutter);
        let RampPlan::Manual {
            shutter_us,
            gain,
            log2_exposure,
        } = decision.plan
        else {
            panic!("expected a manual plan after metering, got {decision:?}");
        };
        assert_eq!(decision.seed_reason, None);
        assert!(!decision.ceiling_bound && !decision.step_limited);
        assert!(
            (log2_exposure - 45_000_f64.log2()).abs() < 0.1,
            "{shutter_us} × {gain}"
        );
    }

    fn preview_frame(scene_ev: f64) -> FrameObservation {
        frame_of(scene_ev, 100_000.0, 4.0)
    }

    #[test]
    fn preview_activity_does_not_keep_a_stale_capture_state_fresh() {
        let settings = RampSettings::default();
        let mut state = Some(seeded(-20.0, at(0)));
        // Two hours of preview frames every 2 s after the last capture.
        for second in (2..=7_200).step_by(2) {
            let now = at(0) + Duration::seconds(second);
            state = observe_preview(state.as_ref(), &preview_frame(-20.0), now, &settings);
        }
        let state = state.unwrap();
        assert_eq!(state.updated_at, at(120));
        assert_eq!(state.captured_at, Some(at(0)));
        let decision = decide(Some(&state), at(120), None, &settings, 5_000_000);
        assert_eq!(decision.plan, RampPlan::Seed);
        assert_eq!(decision.seed_reason, Some(SeedReason::CaptureGap));
        assert_eq!(decision.capture_age_s, Some(7_200));
        // The dashboard still previews from it (design doc §11).
        assert!(preview_plans(Some(&state), at(120), None, &settings, 5_000_000).is_some());
    }

    #[test]
    fn a_recent_capture_keeps_planning_through_preview_updates() {
        let settings = RampSettings::default();
        let mut state = Some(RampState {
            planned_log2_exposure: Some(12.0),
            ..seeded(MID_GREY.log2() - 12.0, at(0))
        });
        for second in (2..=600).step_by(2) {
            let now = at(0) + Duration::seconds(second);
            // The scene got 4 EV darker; the preview measures it.
            state = observe_preview(
                state.as_ref(),
                &preview_frame(MID_GREY.log2() - 16.0),
                now,
                &settings,
            );
        }
        let decision = decide(state.as_ref(), at(10), None, &settings, 5_000_000);
        let RampPlan::Manual { log2_exposure, .. } = decision.plan else {
            panic!("expected a manual plan");
        };
        // Step-limited from the capture's 12.0: 10 minutes allow 10 steps.
        assert!(decision.step_limited);
        assert!((decision.step_limit_ev.unwrap() - 10.0 * settings.max_step_ev).abs() < 1e-9);
        assert!((log2_exposure - (12.0 + 10.0 * settings.max_step_ev)).abs() < 1e-3);
    }

    #[test]
    fn step_limit_scales_with_minutes_since_the_capture() {
        let settings = RampSettings::default();
        let step = settings.max_step_ev;
        assert_eq!(step_limit_ev(&settings, Duration::seconds(30)), step);
        assert_eq!(step_limit_ev(&settings, Duration::seconds(60)), step);
        assert!((step_limit_ev(&settings, Duration::minutes(10)) - 10.0 * step).abs() < 1e-12);
        // A 1-minute timelapse is planned about 50 s after its previous
        // frame was observed: exactly one step, as before.
        let state = RampState {
            planned_log2_exposure: Some(12.0),
            ..seeded(-30.0, at(0))
        };
        let decision = decide(
            Some(&state),
            at(0) + Duration::seconds(50),
            None,
            &settings,
            5_000_000,
        );
        assert_eq!(decision.step_limit_ev, Some(step));
        let RampPlan::Manual { log2_exposure, .. } = decision.plan else {
            panic!("expected a manual plan");
        };
        assert!((log2_exposure - (12.0 + step)).abs() < 1e-3);
    }

    #[test]
    fn seeds_report_why() {
        let settings = RampSettings::default();
        let no_state = decide(None, at(0), None, &settings, 5_000_000);
        assert_eq!(no_state.seed_reason, Some(SeedReason::NoState));
        let stale = decide(
            Some(&seeded(-12.0, at(0))),
            at(31),
            None,
            &settings,
            5_000_000,
        );
        assert_eq!(stale.seed_reason, Some(SeedReason::CaptureGap));
        let fresh = decide(
            Some(&seeded(-12.0, at(0))),
            at(30),
            None,
            &settings,
            5_000_000,
        );
        assert_eq!(fresh.seed_reason, None);
        assert_eq!(fresh.capture_age_s, Some(1_800));
    }

    #[test]
    fn daylight_ceiling_follows_the_sun() {
        let settings = RampSettings::default();
        let ceiling_ms =
            |sun: f64| daylight_ceiling_log2(Some(sun), &settings).unwrap().exp2() / 1e3;
        assert!((ceiling_ms(0.0) - 2_500.0).abs() < 1e-6);
        assert!((ceiling_ms(2.0) - 1_250.0).abs() < 1e-6);
        assert!((ceiling_ms(7.6) - 179.5).abs() < 0.5, "{}", ceiling_ms(7.6));
        assert!((ceiling_ms(10.0) - 160.0).abs() < 1e-6);
        assert!((ceiling_ms(50.0) - 160.0).abs() < 1e-6);
        // At least 1.58 EV (3×) above the worst-case mid-grey exposures
        // measured on the 2026-09-24/25 dawns and dusks, at their own sun
        // elevation (worklog 2026-09-28): the tightest is 52.8 ms at +8.2°.
        for (sun, measured_ms) in [(0.29, 612.6), (8.22, 52.8), (15.58, 41.0)] {
            assert!(ceiling_ms(sun) >= 3.0 * measured_ms, "{sun}°");
        }
        // Below the horizon or without a station there is none.
        assert_eq!(daylight_ceiling_log2(Some(-0.1), &settings), None);
        assert_eq!(daylight_ceiling_log2(None, &settings), None);
        // Brightness compensation shifts it with the target.
        let brighter = RampSettings {
            day_bias_ev: 1.0,
            ..settings
        };
        let shifted = daylight_ceiling_log2(Some(20.0), &brighter).unwrap();
        assert!((shifted - (160_000_f64.log2() + 1.0)).abs() < 1e-9);
    }

    #[test]
    fn daylight_ceiling_only_ever_darkens_a_plan() {
        let settings = RampSettings::default();
        for scene_ev in [-30.0, -24.0, -20.0, -17.0, -12.0, -8.0] {
            let state = seeded(scene_ev, at(0));
            for sun in [-10.0, -0.5, 0.0, 3.0, 7.6, 20.0] {
                let log2 =
                    |sun: Option<f64>| match plan(Some(&state), at(1), sun, &settings, 5_000_000) {
                        RampPlan::Manual { log2_exposure, .. } => log2_exposure,
                        RampPlan::Seed => panic!("expected a manual plan"),
                    };
                // Same day-level target without a station and at ≥ +6°, so
                // compare at the same target: only the ceiling differs.
                if sun >= DAY_ELEVATION_DEG {
                    assert!(log2(Some(sun)) <= log2(None) + 1e-9);
                }
                let decision = decide(Some(&state), at(1), Some(sun), &settings, 5_000_000);
                let RampPlan::Manual { log2_exposure, .. } = decision.plan else {
                    panic!("expected a manual plan");
                };
                if let Some(ceiling) = decision.ceiling_log2_exposure {
                    assert!(log2_exposure <= ceiling + 1e-3, "{scene_ev} {sun}");
                    assert_eq!(
                        decision.ceiling_bound,
                        decision.desired_log2_exposure.unwrap() > ceiling
                    );
                } else {
                    assert!(!decision.ceiling_bound);
                }
            }
        }
    }
}
