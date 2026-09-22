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
/// A gap longer than this since the last ramped frame breaks the sequence:
/// the next capture reseeds from auto exposure instead of stepping from a
/// stale exposure (design doc §5.4).
pub const RESEED_GAP: Duration = Duration::minutes(30);

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
        let checks: [(f64, f64, f64, &'static str); 4] = [
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
        }
    }
}

/// Brightness and colour of one captured frame (design doc §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
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
    pub updated_at: DateTime<Utc>,
    /// Smoothed scene brightness: `log2(luminance) − log2(exposure product)`.
    pub scene_ev: f64,
    /// The previous frame's planned exposure. `None` right after a seed,
    /// which lets the first planned frame jump without a step limit.
    pub planned_log2_exposure: Option<f64>,
}

/// Plans the next ramped capture (design doc §5.3/§5.4).
pub fn plan(
    state: Option<&RampState>,
    now: DateTime<Utc>,
    sun_elevation_deg: Option<f64>,
    settings: &RampSettings,
    max_shutter_us: u64,
) -> RampPlan {
    let Some(state) = state.filter(|state| now - state.updated_at <= RESEED_GAP) else {
        return RampPlan::Seed;
    };
    let max_shutter_us = max_shutter_us.clamp(settings.min_shutter_us, settings.max_shutter_us);
    let target = target_luminance(target_bias_ev(sun_elevation_deg, settings)).log2();
    let mut desired = target - state.scene_ev;
    if let Some(previous) = state.planned_log2_exposure {
        desired = desired.clamp(
            previous - settings.max_step_ev,
            previous + settings.max_step_ev,
        );
    }
    let lowest = (settings.min_shutter_us as f64).log2();
    let highest = (max_shutter_us as f64 * f64::from(settings.max_gain)).log2();
    let (shutter_us, gain) = split_exposure(
        desired.clamp(lowest, highest),
        settings.min_shutter_us,
        max_shutter_us,
        settings.max_gain,
    );
    RampPlan::Manual {
        shutter_us,
        gain,
        log2_exposure: (shutter_us as f64 * f64::from(gain)).log2(),
    }
}

/// Folds a completed capture into the ramp state.
///
/// White balance is deliberately not part of the ramp: scheduled captures
/// use whatever the dashboard's White balance control says, exactly like a
/// manual capture (user decision, 2026-09-22 — see design doc §6).
pub fn observe(
    state: Option<&RampState>,
    plan: &RampPlan,
    observation: &FrameObservation,
    now: DateTime<Utc>,
    settings: &RampSettings,
) -> Option<RampState> {
    let exposure = (observation.exposure_us.max(1.0) * observation.analogue_gain.max(1.0)).log2();
    let scene_ev = observation.meter.luminance.max(MIN_LUMINANCE).log2() - exposure;
    match (plan, state) {
        (RampPlan::Manual { log2_exposure, .. }, Some(state)) => {
            let blend = 1.0 - settings.smoothing;
            Some(RampState {
                updated_at: now,
                scene_ev: state.scene_ev + blend * (scene_ev - state.scene_ev),
                planned_log2_exposure: Some(*log2_exposure),
            })
        }
        _ => Some(RampState {
            updated_at: now,
            scene_ev,
            planned_log2_exposure: None,
        }),
    }
}

/// Folds a *live preview* frame into the ramp state (design doc §11).
///
/// Brightness only. `planned_log2_exposure` is deliberately left alone: it
/// anchors the capture sequence's per-frame step limit, so a preview restart
/// or a passing cloud can't jump a running timelapse.
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
            scene_ev,
            planned_log2_exposure: None,
        });
    };
    let blend = 1.0 - settings.smoothing;
    Some(RampState {
        updated_at: now,
        scene_ev: state.scene_ev + blend * (scene_ev - state.scene_ev),
        planned_log2_exposure: state.planned_log2_exposure,
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
        ];
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
        };
        let clean = wild.sanitized();
        assert!(clean.validate().is_ok(), "{clean:?}");
        assert_eq!(clean.max_shutter_us, MAX_SHUTTER_US);
        assert_eq!(clean.max_gain, RampSettings::default().max_gain);
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
        let again: ScheduleExposure =
            serde_json::from_str(&serde_json::to_string(&parsed).unwrap()).unwrap();
        assert_eq!(again, parsed);
    }

    fn seeded(scene_ev: f64, updated_at: DateTime<Utc>) -> RampState {
        RampState {
            updated_at,
            scene_ev,
            planned_log2_exposure: None,
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
        let state = observe(None, &RampPlan::Seed, &observation, at(0), &settings).unwrap();
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
        let next = observe_preview(Some(&running), &brighter, at(1), &settings).unwrap();
        assert_eq!(next.planned_log2_exposure, Some(18.0));
        // Smoothed toward the newly measured scene, not jumped.
        let measured = (MID_GREY * 4.0).log2() - 2_000_f64.log2();
        assert!((next.scene_ev - (-20.0 + 0.5 * (measured + 20.0))).abs() < 1e-9);
    }

    #[test]
    fn a_preview_seeded_ramp_plans_without_a_step_limit() {
        // The first plan after preview seeding may jump straight to the
        // target: there is no capture sequence to keep smooth yet.
        let settings = RampSettings::default();
        let observation = FrameObservation {
            exposure_us: 2_000.0,
            analogue_gain: 1.0,
            meter: meter(MID_GREY / 16.0, None),
        };
        let state = observe_preview(None, &observation, at(0), &settings).unwrap();
        let RampPlan::Manual { log2_exposure, .. } =
            plan(Some(&state), at(1), None, &settings, 5_000_000)
        else {
            panic!("expected a manual plan");
        };
        // Metered 4 EV dark at 2 ms -> 32 ms.
        assert!(
            (log2_exposure - 32_000_f64.log2()).abs() < 1e-3,
            "{log2_exposure}"
        );
    }
}
