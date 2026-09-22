//! Composable timelapse scheduling (`docs/optic-daemon-scheduler.md`).
//!
//! Two halves: a pure rule-composition engine (data types, the
//! deterministic `occurrences()` function, merge/tag, slug validation —
//! Phase 1a) with no camera/hardware dependency, and a bounded actor
//! (`SchedulerHandle`, Phase 1c) that actually ticks against real time and
//! fires captures, following the same shape as `optic_camera`/`optic_sync`.
//! `occurrences()` is deliberately the single implementation used for both
//! "what should fire next" (the actor) and "what would the next 48h look
//! like" (the design doc's future Shot Forecaster, 1d) — see design doc §3.

use std::{collections::HashSet, path::PathBuf};

use chrono::{DateTime, Datelike, Duration, NaiveTime, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    camera::{AppConfig, CaptureProfile, CaptureRequest, CaptureSource},
    durable_state, ephemeris,
    exposure_ramp::{self, FrameObservation, RampPlan, RampState, ScheduleExposure},
    optic_camera::OpticCamera,
    optic_capture_log::{CaptureLog, CaptureLogEntry},
};

/// Occurrences within this window of each other collapse into one physical
/// capture (design doc §3.1). The doc originally proposed 5s on the
/// assumption real captures are faster than that; measured data
/// (`docs/optic-daemon-capture-performance.md`) shows real captures
/// actually take ~5.1-5.6s end to end, right at that line — 10s gives real
/// margin above the slowest measured profile (MasterArchive+DNG, ~5.46s)
/// so two rules whose occurrences are genuinely close together still merge
/// into one physical shot, not two, per user decision 2026-09-20.
pub const MERGE_WINDOW: Duration = Duration::seconds(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScheduleRunState {
    Running,
    Paused,
}

impl Default for ScheduleRunState {
    /// A freshly-provisioned station shouldn't start autonomously firing
    /// captures before an operator has reviewed its rules.
    fn default() -> Self {
        Self::Paused
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Station {
    pub latitude: f64,
    pub longitude: f64,
    pub elevation_m: f64,
    /// IANA timezone name (e.g. "America/Vancouver"). Stored explicitly
    /// rather than read from the OS at run time, so the schedule stays
    /// correct/self-describing even if the Pi's system timezone changes.
    pub timezone: String,
}

impl Default for Station {
    fn default() -> Self {
        Self {
            latitude: 0.0,
            longitude: 0.0,
            elevation_m: 0.0,
            timezone: "UTC".to_owned(),
        }
    }
}

impl Station {
    fn tz(&self) -> Tz {
        self.timezone.parse().unwrap_or(chrono_tz::UTC)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScheduleConfig {
    pub station: Option<Station>,
    pub rules: Vec<Rule>,
    /// How every scheduled capture is exposed — one global mode, not per
    /// rule (`docs/optic-daemon-exposure-ramping.md` §3.1). Defaults to
    /// `Dashboard`, so a config written before this field existed behaves
    /// exactly as before.
    pub exposure: ScheduleExposure,
}

impl ScheduleConfig {
    fn tz(&self) -> Tz {
        self.station.as_ref().map_or(chrono_tz::UTC, Station::tz)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    pub label: String,
    pub slug: String,
    pub enabled: bool,
    pub trigger: Trigger,
    #[serde(default)]
    pub constraints: Constraints,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Trigger {
    Interval {
        every_secs: u64,
        #[serde(default = "default_align_to_wall_clock")]
        align_to_wall_clock: bool,
    },
    RecurringTime {
        days: RecurringDays,
        time: NaiveTime,
    },
    /// Fires at a Solar/Lunar/MilkyWay event, optionally offset — design
    /// doc §2/§14. Requires `ScheduleConfig.station` to be set; a rule
    /// using this trigger with no station configured produces zero
    /// occurrences (same "dead rule" handling as an unsatisfiable
    /// constraint, not an error — see `rule_occurrences`).
    Ephemeris {
        target: CelestialTarget,
        /// Signed offset from the named event, in seconds (e.g. -1800 =
        /// "30 minutes before"). `chrono::Duration` isn't directly
        /// serde-friendly, so this is stored as the same kind of `_secs`
        /// integer every other duration field in this file already uses.
        #[serde(default)]
        offset_secs: i64,
    },
}

fn default_align_to_wall_clock() -> bool {
    true
}

/// Shared by every event taxonomy that can fire on either an upward or
/// downward crossing of a threshold (design doc §2) — no longer
/// solar-specific in practice (used by `SolarEvent::FixedElevation`,
/// `MilkyWayEvent::CoreElevation`), so the name isn't either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CrossingDirection {
    Rising,
    Setting,
    Both,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "event")]
pub enum CelestialTarget {
    Solar(SolarEvent),
    Lunar(LunarEvent),
    MilkyWay(MilkyWayEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum SolarEvent {
    SolarNoon,
    Nadir,
    Sunrise,
    Sunset,
    CivilDawn,
    CivilDusk,
    NauticalDawn,
    NauticalDusk,
    AstronomicalDawn,
    AstronomicalDusk,
    GoldenHourMorningStart,
    GoldenHourMorningEnd,
    GoldenHourEveningStart,
    GoldenHourEveningEnd,
    BlueHourMorningStart,
    BlueHourMorningEnd,
    BlueHourEveningStart,
    BlueHourEveningEnd,
    FixedElevation {
        degrees: f64,
        direction: CrossingDirection,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum LunarEvent {
    Moonrise,
    Moonset,
    LunarTransit,
    LunarAntitransit,
    NewMoon,
    FirstQuarter,
    FullMoon,
    LastQuarter,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum MilkyWayEvent {
    CoreRise,
    CoreSet,
    CoreTransit,
    CoreElevation {
        degrees: f64,
        direction: CrossingDirection,
    },
    Orientation {
        azimuth_degrees: f64,
    },
}

// Adjacent tagging (`content = "value"`), not pure internal tagging like
// `Trigger`/`Constraint` — `Weekdays(Vec<Weekday>)` is a newtype wrapping a
// sequence, which serde cannot represent under pure internal tagging at
// all (confirmed the hard way: it panics at serialize time — "cannot
// serialize tagged newtype variant ... containing a sequence"). `Every`
// and `NthWeekdayOfMonth` would work fine either way; this covers all
// three variants uniformly rather than special-casing just one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value")]
pub enum RecurringDays {
    Every,
    Weekdays(Vec<Weekday>),
    NthWeekdayOfMonth { n: u8, weekday: Weekday },
}

impl RecurringDays {
    fn matches(&self, date: chrono::NaiveDate) -> bool {
        match self {
            Self::Every => true,
            Self::Weekdays(days) => days.contains(&date.weekday()),
            Self::NthWeekdayOfMonth { n, weekday } => {
                date.weekday() == *weekday && (date.day() - 1) / 7 + 1 == u32::from(*n)
            }
        }
    }
}

/// Named-optional-field struct, not `Vec<Constraint>` — deliberately
/// structural, not just validated: at most one of each constraint type can
/// ever exist on a `Rule`, because there is exactly one `Option<T>` slot
/// per type. Two `TimeWindow`s (or any duplicate) on the same rule would be
/// redundant anyway — constraints are AND-composed (see `holds()`), so a
/// second instance of a type could only narrow or exactly duplicate the
/// first, never add anything a single range/window can't already express.
/// `SunElevationWindow`/`MoonElevationWindow`/`MoonIlluminationWindow`/
/// `MilkyWayElevationWindow` (design doc §2) were added here the same way
/// `time_window` was — one named field at a time, each actually
/// implemented rather than stubbed out in advance.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Constraints {
    pub time_window: Option<TimeWindow>,
    pub sun_elevation_window: Option<SunElevationWindow>,
    pub moon_elevation_window: Option<MoonElevationWindow>,
    pub moon_illumination_window: Option<MoonIlluminationWindow>,
    pub milky_way_elevation_window: Option<MilkyWayElevationWindow>,
}

impl Constraints {
    /// `station` is only consulted by the four `*Window` constraints below
    /// `time_window` — passed through even when `None` (rather than
    /// requiring callers to skip calling `holds` at all) so a rule mixing
    /// `time_window` with an elevation window still evaluates its
    /// time-of-day half correctly on a schedule with no station
    /// configured; the elevation half then simply can't hold (see each
    /// window's own `holds`), the same "dead rule" treatment as any other
    /// permanently-unsatisfiable constraint.
    fn holds(&self, when: DateTime<Tz>, station: Option<&Station>) -> bool {
        self.time_window.as_ref().is_none_or(|c| c.holds(when))
            && self
                .sun_elevation_window
                .as_ref()
                .is_none_or(|c| c.holds(when, station))
            && self
                .moon_elevation_window
                .as_ref()
                .is_none_or(|c| c.holds(when, station))
            && self
                .moon_illumination_window
                .as_ref()
                .is_none_or(|c| c.holds(when))
            && self
                .milky_way_elevation_window
                .as_ref()
                .is_none_or(|c| c.holds(when, station))
    }
}

/// Sun elevation angle, in degrees, that must hold for the constraint to
/// pass — e.g. `{ -90, -6 }` = "astronomically dark or darker."
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SunElevationWindow {
    pub min_deg: f64,
    pub max_deg: f64,
}

impl SunElevationWindow {
    fn holds(&self, when: DateTime<Tz>, station: Option<&Station>) -> bool {
        let Some(station) = station else { return false };
        let jd = ephemeris::julian_day(when.with_timezone(&Utc));
        let elevation = ephemeris::elevation_deg(ephemeris::sun_equatorial(jd), station, jd);
        elevation >= self.min_deg && elevation <= self.max_deg
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoonElevationWindow {
    pub min_deg: f64,
    pub max_deg: f64,
}

impl MoonElevationWindow {
    fn holds(&self, when: DateTime<Tz>, station: Option<&Station>) -> bool {
        let Some(station) = station else { return false };
        let jd = ephemeris::julian_day(when.with_timezone(&Utc));
        let elevation = ephemeris::elevation_deg(ephemeris::moon_equatorial(jd), station, jd);
        elevation >= self.min_deg && elevation <= self.max_deg
    }
}

/// Moon illuminated-fraction percentage (0.0-100.0) that must hold — e.g.
/// `{ 0, 20 }` = "dark-sky window" (new moon through thin crescent).
/// Unlike the three elevation windows, illumination doesn't depend on the
/// observer's location at all (it's a Sun-Moon-Earth geometry fact), so
/// this constraint holds regardless of whether a `Station` is configured.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoonIlluminationWindow {
    pub min_pct: f64,
    pub max_pct: f64,
}

impl MoonIlluminationWindow {
    fn holds(&self, when: DateTime<Tz>) -> bool {
        let jd = ephemeris::julian_day(when.with_timezone(&Utc));
        let pct = ephemeris::moon_illumination_pct(jd);
        pct >= self.min_pct && pct <= self.max_pct
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MilkyWayElevationWindow {
    pub min_deg: f64,
    pub max_deg: f64,
}

impl MilkyWayElevationWindow {
    fn holds(&self, when: DateTime<Tz>, station: Option<&Station>) -> bool {
        let Some(station) = station else { return false };
        let jd = ephemeris::julian_day(when.with_timezone(&Utc));
        let elevation =
            ephemeris::elevation_deg(ephemeris::milky_way_core_equatorial(jd), station, jd);
        elevation >= self.min_deg && elevation <= self.max_deg
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeWindow {
    pub days: RecurringDays,
    pub start: NaiveTime,
    pub end: NaiveTime,
}

impl TimeWindow {
    fn holds(&self, when: DateTime<Tz>) -> bool {
        let local_date = when.date_naive();
        let local_time = when.time();
        let in_window = if self.start <= self.end {
            local_time >= self.start && local_time < self.end
        } else {
            // Overnight wrap, e.g. 22:00-06:00: "in window" means
            // >= start (today) OR < end (which, for a time-of-day
            // comparison, correctly covers the small-hours part of
            // the *previous* day's window too).
            local_time >= self.start || local_time < self.end
        };
        if !in_window {
            return false;
        }
        // For the wrapped small-hours case, the calendar day that
        // "owns" this window is the day the window *started* on,
        // i.e. yesterday relative to `local_time < end`.
        let owning_date = if self.start > self.end && local_time < self.end {
            local_date.pred_opt().unwrap_or(local_date)
        } else {
            local_date
        };
        self.days.matches(owning_date)
    }
}

/// One physical capture the schedule would produce, and every rule slug
/// that contributed to it (design doc §3.1 — merged, not suppressed).
#[derive(Debug, Clone, PartialEq)]
pub struct ForecastedShot {
    pub at: DateTime<Tz>,
    /// Sorted alphabetically, deduplicated — see `occurrences()`.
    pub rule_slugs: Vec<String>,
}

/// UTC-friendly entry point to `occurrences()` for callers outside this
/// module (the future Shot Forecaster endpoint) that shouldn't need to know
/// about `chrono_tz::Tz` — converts `from_utc` into the schedule's own
/// station timezone internally, the same way the actor does.
pub fn forecast(
    schedule: &ScheduleConfig,
    from_utc: DateTime<Utc>,
    horizon: Duration,
) -> Vec<ForecastedShot> {
    let tz = schedule.tz();
    occurrences(schedule, from_utc.with_timezone(&tz), horizon)
}

/// Design doc §8's dead-rule advisory: any *enabled* rule that
/// contributes to zero of `shots` — a likely misconfiguration (e.g. a
/// `TimeWindow` and a `SunElevationWindow` that never both hold for this
/// station), surfaced to the operator instead of silently producing
/// nothing forever (§3.1).
pub fn dead_rule_slugs(schedule: &ScheduleConfig, shots: &[ForecastedShot]) -> Vec<String> {
    schedule
        .rules
        .iter()
        .filter(|rule| rule.enabled)
        .filter(|rule| {
            !shots
                .iter()
                .any(|shot| shot.rule_slugs.iter().any(|slug| slug == &rule.slug))
        })
        .map(|rule| rule.slug.clone())
        .collect()
}

/// One pair of enabled rules both "active" (each contributing at least
/// one instant) over a shared span of time — design doc §3.1's "what
/// merge+tag does not solve": two different cadences can both stay
/// active for hours without any individual occurrence ever landing
/// inside the same merge window as the other, so their *combined* rate
/// over that shared span is higher than either rule's own stated rate,
/// without ever showing up as a single merged, multi-tagged shot.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlapAdvisory {
    pub rule_slugs: [String; 2],
    pub window_start: DateTime<Tz>,
    pub window_end: DateTime<Tz>,
    /// Count of each rule's own contributed instants that fall inside
    /// `[window_start, window_end]`, summed — a shot both rules merged
    /// into is counted once per rule, matching how "combined rate" reads
    /// naturally (Weekday Baseline's own shots + Site Visit's own shots
    /// in that window), not the count of distinct physical shots.
    pub combined_shots: usize,
}

/// Every enabled-rule pair whose own contributed instants (read back out
/// of the already-merged `shots`) overlap in time at all. Quadratic in
/// rule count, which is fine — schedules have a handful of rules, not
/// thousands.
pub fn overlap_advisories(
    schedule: &ScheduleConfig,
    shots: &[ForecastedShot],
) -> Vec<OverlapAdvisory> {
    let enabled_slugs: Vec<&str> = schedule
        .rules
        .iter()
        .filter(|rule| rule.enabled)
        .map(|rule| rule.slug.as_str())
        .collect();

    let own_times = |slug: &str| -> Vec<DateTime<Tz>> {
        shots
            .iter()
            .filter(|shot| shot.rule_slugs.iter().any(|s| s == slug))
            .map(|shot| shot.at)
            .collect()
    };

    let mut advisories = Vec::new();
    for i in 0..enabled_slugs.len() {
        for j in (i + 1)..enabled_slugs.len() {
            let (a, b) = (enabled_slugs[i], enabled_slugs[j]);
            let a_times = own_times(a);
            let b_times = own_times(b);
            let (Some(a_first), Some(a_last)) = (a_times.first(), a_times.last()) else {
                continue;
            };
            let (Some(b_first), Some(b_last)) = (b_times.first(), b_times.last()) else {
                continue;
            };
            let window_start = (*a_first).max(*b_first);
            let window_end = (*a_last).min(*b_last);
            if window_start >= window_end {
                continue;
            }
            let combined_shots = a_times
                .iter()
                .chain(b_times.iter())
                .filter(|at| **at >= window_start && **at <= window_end)
                .count();
            advisories.push(OverlapAdvisory {
                rule_slugs: [a.to_owned(), b.to_owned()],
                window_start,
                window_end,
                combined_shots,
            });
        }
    }
    advisories
}

/// The single implementation behind both "when should the actor wake up
/// next" and the Shot Forecaster (design doc §3): deterministic, no I/O, no
/// real clock reads — everything is a function of `schedule` and `from`.
///
/// Occurrences are computed over the open-then-closed interval
/// `(from, from + horizon]` — strictly after `from` (so calling this again
/// right after a shot fires doesn't re-report the shot that just happened),
/// up to and including the end of the horizon.
pub fn occurrences(
    schedule: &ScheduleConfig,
    from: DateTime<Tz>,
    horizon: Duration,
) -> Vec<ForecastedShot> {
    let until = from + horizon;
    let tz = schedule.tz();
    let station = schedule.station.as_ref();

    let mut raw: Vec<(DateTime<Tz>, &str)> = Vec::new();
    for rule in &schedule.rules {
        if !rule.enabled {
            continue;
        }
        for when in rule_occurrences(&rule.trigger, tz, station, from, until) {
            if rule.constraints.holds(when, station) {
                raw.push((when, rule.slug.as_str()));
            }
        }
    }
    raw.sort_by_key(|(when, _)| *when);

    merge(raw)
}

fn rule_occurrences(
    trigger: &Trigger,
    tz: Tz,
    station: Option<&Station>,
    from: DateTime<Tz>,
    until: DateTime<Tz>,
) -> Vec<DateTime<Tz>> {
    match trigger {
        Trigger::Interval {
            every_secs,
            align_to_wall_clock,
        } => interval_occurrences(*every_secs, *align_to_wall_clock, from, until),
        Trigger::RecurringTime { days, time } => {
            recurring_time_occurrences(days, *time, tz, from, until)
        }
        Trigger::Ephemeris {
            target,
            offset_secs,
        } => {
            // No station configured: a dead rule, same treatment as any
            // other permanently-unsatisfiable trigger/constraint — zero
            // occurrences, not an error (design doc §3.1's dead-rule
            // handling).
            let Some(station) = station else {
                return Vec::new();
            };
            ephemeris_occurrences(
                target,
                *offset_secs,
                station,
                from.with_timezone(&Utc),
                until.with_timezone(&Utc),
            )
            .into_iter()
            .map(|utc| utc.with_timezone(&tz))
            .collect()
        }
    }
}

/// Dispatches a `CelestialTarget` to the right `ephemeris` search
/// primitive. Every named event maps to either a threshold-crossing
/// search (rise/set/named-twilight-tier/fixed-elevation/orientation) or an
/// extremum search (transit/antitransit) over the same underlying
/// elevation (or azimuth) function — see `ephemeris`'s module doc for why
/// those two primitives are enough for all three taxonomies.
///
/// `pub(crate)`: also used directly by `web.rs`'s Config-page celestial
/// preview (`GET /api/celestial-preview`), which needs one-off
/// next-occurrence lookups for an ad-hoc (possibly not-yet-saved)
/// `Station` — not a `Rule`/`Trigger`, so it calls this shared primitive
/// straight rather than going through `occurrences()`'s schedule-level
/// machinery.
pub(crate) fn ephemeris_occurrences(
    target: &CelestialTarget,
    offset_secs: i64,
    station: &Station,
    from: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Vec<DateTime<Utc>> {
    // A signed offset shifts the *search window* by the same amount in
    // the opposite direction, then shifts results back — simpler and
    // exactly equivalent to shifting each found instant after the fact,
    // and correctly still finds an event whose un-offset instant falls
    // just outside `[from, until]` but whose offset instant belongs
    // inside it.
    let offset = Duration::seconds(offset_secs);
    let search_from = from - offset;
    let search_until = until - offset;

    let raw = match target {
        CelestialTarget::Solar(event) => {
            solar_occurrences(*event, station, search_from, search_until)
        }
        CelestialTarget::Lunar(event) => {
            lunar_occurrences(*event, station, search_from, search_until)
        }
        CelestialTarget::MilkyWay(event) => {
            milky_way_occurrences(event, station, search_from, search_until)
        }
    };
    raw.into_iter().map(|t| t + offset).collect()
}

fn sun_elevation_at(station: &Station) -> impl Fn(DateTime<Utc>) -> f64 + '_ {
    move |t| {
        let jd = ephemeris::julian_day(t);
        ephemeris::elevation_deg(ephemeris::sun_equatorial(jd), station, jd)
    }
}

fn moon_elevation_at(station: &Station) -> impl Fn(DateTime<Utc>) -> f64 + '_ {
    move |t| {
        let jd = ephemeris::julian_day(t);
        ephemeris::elevation_deg(ephemeris::moon_equatorial(jd), station, jd)
    }
}

fn milky_way_elevation_at(station: &Station) -> impl Fn(DateTime<Utc>) -> f64 + '_ {
    move |t| {
        let jd = ephemeris::julian_day(t);
        ephemeris::elevation_deg(ephemeris::milky_way_core_equatorial(jd), station, jd)
    }
}

fn solar_occurrences(
    event: SolarEvent,
    station: &Station,
    from: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Vec<DateTime<Utc>> {
    use CrossingDirection as Dir;
    let elevation = sun_elevation_at(station);
    let (threshold, direction) = match event {
        SolarEvent::Sunrise => (-0.8333, Dir::Rising),
        SolarEvent::Sunset => (-0.8333, Dir::Setting),
        SolarEvent::CivilDawn => (-6.0, Dir::Rising),
        SolarEvent::CivilDusk => (-6.0, Dir::Setting),
        SolarEvent::NauticalDawn => (-12.0, Dir::Rising),
        SolarEvent::NauticalDusk => (-12.0, Dir::Setting),
        SolarEvent::AstronomicalDawn => (-18.0, Dir::Rising),
        SolarEvent::AstronomicalDusk => (-18.0, Dir::Setting),
        SolarEvent::GoldenHourMorningStart => (-4.0, Dir::Rising),
        SolarEvent::GoldenHourMorningEnd => (6.0, Dir::Rising),
        SolarEvent::GoldenHourEveningStart => (6.0, Dir::Setting),
        SolarEvent::GoldenHourEveningEnd => (-4.0, Dir::Setting),
        SolarEvent::BlueHourMorningStart => (-6.0, Dir::Rising),
        SolarEvent::BlueHourMorningEnd => (-4.0, Dir::Rising),
        SolarEvent::BlueHourEveningStart => (-4.0, Dir::Setting),
        SolarEvent::BlueHourEveningEnd => (-6.0, Dir::Setting),
        SolarEvent::FixedElevation { degrees, direction } => (degrees, direction),
        SolarEvent::SolarNoon => return ephemeris::find_extrema(from, until, true, elevation),
        SolarEvent::Nadir => return ephemeris::find_extrema(from, until, false, elevation),
    };
    ephemeris::find_crossings(from, until, threshold, direction, elevation)
}

fn lunar_occurrences(
    event: LunarEvent,
    station: &Station,
    from: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Vec<DateTime<Utc>> {
    match event {
        LunarEvent::Moonrise => {
            ephemeris::find_crossings(from, until, 0.0, CrossingDirection::Rising, |t| {
                let jd = ephemeris::julian_day(t);
                ephemeris::elevation_deg(ephemeris::moon_equatorial(jd), station, jd)
                    - ephemeris::moon_rise_set_altitude_deg(jd)
            })
        }
        LunarEvent::Moonset => {
            ephemeris::find_crossings(from, until, 0.0, CrossingDirection::Setting, |t| {
                let jd = ephemeris::julian_day(t);
                ephemeris::elevation_deg(ephemeris::moon_equatorial(jd), station, jd)
                    - ephemeris::moon_rise_set_altitude_deg(jd)
            })
        }
        LunarEvent::LunarTransit => {
            ephemeris::find_extrema(from, until, true, moon_elevation_at(station))
        }
        LunarEvent::LunarAntitransit => {
            ephemeris::find_extrema(from, until, false, moon_elevation_at(station))
        }
        LunarEvent::NewMoon => {
            ephemeris::lunar_phase_occurrences(ephemeris::LunarPhase::New, from, until)
        }
        LunarEvent::FirstQuarter => {
            ephemeris::lunar_phase_occurrences(ephemeris::LunarPhase::First, from, until)
        }
        LunarEvent::FullMoon => {
            ephemeris::lunar_phase_occurrences(ephemeris::LunarPhase::Full, from, until)
        }
        LunarEvent::LastQuarter => {
            ephemeris::lunar_phase_occurrences(ephemeris::LunarPhase::Last, from, until)
        }
    }
}

fn milky_way_occurrences(
    event: &MilkyWayEvent,
    station: &Station,
    from: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Vec<DateTime<Utc>> {
    use CrossingDirection as Dir;
    match *event {
        // Same -0.5667° refraction threshold `astro::transit` uses for a
        // star/planet rise/set — the core is, geometrically, exactly that.
        MilkyWayEvent::CoreRise => ephemeris::find_crossings(
            from,
            until,
            -0.5667,
            Dir::Rising,
            milky_way_elevation_at(station),
        ),
        MilkyWayEvent::CoreSet => ephemeris::find_crossings(
            from,
            until,
            -0.5667,
            Dir::Setting,
            milky_way_elevation_at(station),
        ),
        MilkyWayEvent::CoreTransit => {
            ephemeris::find_extrema(from, until, true, milky_way_elevation_at(station))
        }
        MilkyWayEvent::CoreElevation { degrees, direction } => ephemeris::find_crossings(
            from,
            until,
            degrees,
            direction,
            milky_way_elevation_at(station),
        ),
        MilkyWayEvent::Orientation { azimuth_degrees } => {
            ephemeris::find_azimuth_crossings(from, until, azimuth_degrees, move |t| {
                let jd = ephemeris::julian_day(t);
                ephemeris::azimuth_deg(ephemeris::milky_way_core_equatorial(jd), station, jd)
            })
        }
    }
}

fn interval_occurrences(
    every_secs: u64,
    align_to_wall_clock: bool,
    from: DateTime<Tz>,
    until: DateTime<Tz>,
) -> Vec<DateTime<Tz>> {
    if every_secs == 0 {
        return Vec::new();
    }
    let every = Duration::seconds(every_secs as i64);

    let mut next = if align_to_wall_clock {
        // Aligned to a fixed epoch (Unix epoch), not to `from` — so the
        // same rule produces the same absolute instants regardless of when
        // `occurrences()` happens to be called, restart-stable (design
        // doc §3).
        let epoch_secs = from.timestamp();
        let remainder = epoch_secs.rem_euclid(every_secs as i64);
        let boundary_before_or_at = from - Duration::seconds(remainder);
        let mut candidate = boundary_before_or_at;
        while candidate <= from {
            candidate += every;
        }
        candidate
    } else {
        from + every
    };

    let mut out = Vec::new();
    while next <= until {
        out.push(next);
        next += every;
    }
    out
}

fn recurring_time_occurrences(
    days: &RecurringDays,
    time: NaiveTime,
    tz: Tz,
    from: DateTime<Tz>,
    until: DateTime<Tz>,
) -> Vec<DateTime<Tz>> {
    let mut out = Vec::new();
    let mut date = from.date_naive();
    let end_date = until.date_naive();
    // Loop bound is `date <= end_date`, not `<`: a candidate built from
    // `end_date` at an early `time` can still legitimately be <= `until`
    // when `until` itself is later that same day.
    while date <= end_date {
        if days.matches(date)
            && let Some(candidate) = date.and_time(time).and_local_timezone(tz).earliest()
            && candidate > from
            && candidate <= until
        {
            out.push(candidate);
        }
        let Some(next) = date.succ_opt() else {
            break; // NaiveDate::MAX; nothing further to check.
        };
        date = next;
    }
    out
}

/// Groups raw, already-sorted `(when, slug)` pairs into physical shots:
/// consecutive entries within `MERGE_WINDOW` of the *group's first* entry
/// (not pairwise-chained, to avoid unbounded drift) become one
/// `ForecastedShot` with every contributing slug, deduplicated and sorted.
///
/// Deliberately makes no exception for a single rule appearing more than
/// once in one group (e.g. an interval shorter than `MERGE_WINDOW`) — the
/// window is calibrated to "shorter than any real capture," so this is the
/// same hardware-reality reasoning applying uniformly, not a bug: a rule
/// configured faster than the camera can physically shoot merges with
/// itself exactly like it would merge with any other rule.
fn merge(raw: Vec<(DateTime<Tz>, &str)>) -> Vec<ForecastedShot> {
    let mut shots = Vec::new();
    let mut iter = raw.into_iter().peekable();
    while let Some((group_start, first_slug)) = iter.next() {
        let mut slugs: HashSet<&str> = HashSet::new();
        slugs.insert(first_slug);
        while let Some((when, _)) = iter.peek() {
            if *when - group_start <= MERGE_WINDOW {
                let (_, slug) = iter.next().unwrap();
                slugs.insert(slug);
            } else {
                break;
            }
        }
        let mut rule_slugs: Vec<String> = slugs.into_iter().map(str::to_owned).collect();
        rule_slugs.sort();
        shots.push(ForecastedShot {
            at: group_start,
            rule_slugs,
        });
    }
    shots
}

#[derive(Debug, PartialEq, Eq)]
pub enum SlugError {
    Empty {
        rule_index: usize,
    },
    InvalidFormat {
        rule_index: usize,
        slug: String,
    },
    Duplicate {
        rule_index: usize,
        other_index: usize,
        slug: String,
    },
}

/// Design doc §3.1/§14: every rule requires a slug, filesystem-safe, and
/// globally unique case-insensitively across *all* rules regardless of
/// `enabled` — a disabled rule keeps its slug reserved.
pub fn validate_rule_slugs(rules: &[Rule]) -> Result<(), SlugError> {
    fn is_valid_format(slug: &str) -> bool {
        let mut chars = slug.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
            return false;
        }
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    }

    for (index, rule) in rules.iter().enumerate() {
        if rule.slug.is_empty() {
            return Err(SlugError::Empty { rule_index: index });
        }
        if !is_valid_format(&rule.slug) {
            return Err(SlugError::InvalidFormat {
                rule_index: index,
                slug: rule.slug.clone(),
            });
        }
        for (other_index, other) in rules.iter().enumerate().take(index) {
            if rule.slug.eq_ignore_ascii_case(&other.slug) {
                return Err(SlugError::Duplicate {
                    rule_index: index,
                    other_index,
                    slug: rule.slug.clone(),
                });
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Bounded actor (Phase 1c) — same shape as `optic_camera`/`optic_sync`:
// a cheap `Clone` handle over a command channel and a `watch` status
// channel, one owning `tokio::task`.
// ---------------------------------------------------------------------

/// How far ahead the actor looks for its next occurrence before falling
/// back to an idle recheck (design doc §8 uses the same 48h horizon for
/// the Shot Forecaster — reused here, not independently chosen).
const LOOKAHEAD: Duration = Duration::hours(48);
/// How long the actor sleeps before rechecking when nothing is due within
/// `LOOKAHEAD` (e.g. no rules configured yet, or the next occurrence is
/// further out than the horizon covers).
const IDLE_RECHECK: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(Debug, Clone, Serialize)]
pub struct LastCapture {
    pub at: DateTime<Utc>,
    pub rule_slugs: Vec<String>,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchedulerStatus {
    pub run_state: ScheduleRunState,
    pub next_capture_at: Option<DateTime<Utc>>,
    pub next_capture_rules: Vec<String>,
    pub last_capture: Option<LastCapture>,
    /// The last auto-ramped capture; `None` in `Dashboard` mode.
    pub exposure: Option<RampSnapshot>,
    /// The ramp's state for `live_exposure_plan` (the dashboard's live
    /// values); internal, not part of the status JSON.
    #[serde(skip)]
    pub ramp_state: Option<RampState>,
}

/// The next scheduled frame's exposure as of now — what the dashboard's
/// locked Shutter/Gain/White balance fields show while *Scheduled
/// exposure* is on (`docs/optic-daemon-exposure-ramping.md` §10).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LiveExposurePlan {
    /// The next frame is an auto-exposure/AWB seed, so there's no manual
    /// exposure to show yet.
    pub seeding: bool,
    pub shutter_us: Option<u64>,
    pub gain: Option<f32>,
    pub colour_gains: Option<[f32; 2]>,
    pub target_bias_ev: f64,
    pub sun_elevation_deg: Option<f64>,
    pub max_shutter_us: u64,
    /// When the ramp last learned from a captured frame.
    pub ramp_updated_at: Option<DateTime<Utc>>,
}

/// How far ahead `live_exposure_plan` looks for the next two shots to size
/// the interval budget. Short: it runs on every status poll.
const LIVE_PLAN_LOOKAHEAD: Duration = Duration::hours(6);

/// Plans the next ramped frame for `now`, the same way the scheduler will
/// when it fires (sun elevation and interval budget included). `None` in
/// `Dashboard` mode.
pub fn live_exposure_plan(
    schedule: &ScheduleConfig,
    ramp_state: Option<&RampState>,
    now: DateTime<Utc>,
) -> Option<LiveExposurePlan> {
    let ScheduleExposure::AutoRamp(settings) = &schedule.exposure else {
        return None;
    };
    let settings = settings.sanitized();
    let sun_elevation_deg = schedule
        .station
        .as_ref()
        .map(|station| sun_elevation_at(station)(now));
    let mut upcoming = occurrences(
        schedule,
        now.with_timezone(&schedule.tz()),
        LIVE_PLAN_LOOKAHEAD,
    )
    .into_iter();
    let gap = upcoming
        .next()
        .and_then(|next| upcoming.next().map(|after| after.at - next.at));
    let max_shutter_us = exposure_ramp::max_shutter_for_gap(gap, &settings);
    let target_bias_ev = exposure_ramp::target_bias_ev(sun_elevation_deg, &settings);
    let base = LiveExposurePlan {
        seeding: true,
        shutter_us: None,
        gain: None,
        colour_gains: None,
        target_bias_ev,
        sun_elevation_deg,
        max_shutter_us,
        ramp_updated_at: ramp_state.map(|state| state.updated_at),
    };
    match exposure_ramp::plan(
        ramp_state,
        now,
        sun_elevation_deg,
        &settings,
        max_shutter_us,
    ) {
        RampPlan::Seed { .. } => Some(base),
        RampPlan::Manual {
            shutter_us,
            gain,
            colour_gains,
            ..
        } => Some(LiveExposurePlan {
            seeding: false,
            shutter_us: Some(shutter_us),
            gain: Some(gain),
            colour_gains: Some(colour_gains),
            ..base
        }),
    }
}

/// What the last auto-ramped capture did — the exposure the camera
/// actually used (request metadata) alongside the ramp's inputs.
#[derive(Debug, Clone, Serialize)]
pub struct RampSnapshot {
    pub at: DateTime<Utc>,
    /// An auto-exposure/AWB seed frame rather than a ramped one.
    pub seed: bool,
    pub exposure_us: Option<i32>,
    pub analogue_gain: Option<f32>,
    pub colour_gains: Option<[f32; 2]>,
    pub luminance: Option<f64>,
    pub target_bias_ev: f64,
    pub sun_elevation_deg: Option<f64>,
    /// Shutter ceiling for this shot: the setting, or less when the gap to
    /// the next shot is short (`exposure_ramp::max_shutter_for_gap`).
    pub max_shutter_us: u64,
}

#[derive(Debug)]
pub struct SchedulerUnavailable;

enum SchedulerCommand {
    Pause {
        reply: oneshot::Sender<()>,
    },
    Resume {
        reply: oneshot::Sender<()>,
    },
    /// Fire-and-forget: wakes the actor to re-read config immediately,
    /// rather than waiting out whatever sleep it already armed against
    /// the *previous* config. Sent by `commit_config` after every
    /// successful commit — without this, a rule edit could silently wait
    /// hours to take effect if the actor was already sleeping toward a
    /// distant occurrence under the old rules.
    ConfigChanged,
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// The single entry point for autonomous timelapse scheduling.
#[derive(Clone)]
pub struct SchedulerHandle {
    commands: mpsc::Sender<SchedulerCommand>,
    status: watch::Receiver<SchedulerStatus>,
}

impl SchedulerHandle {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        camera: OpticCamera,
        capture_log: Option<CaptureLog>,
        capture_dir: PathBuf,
        config_cache_path: PathBuf,
        run_state_path: PathBuf,
        run_state_cache_path: PathBuf,
        initial_run_state: ScheduleRunState,
    ) -> Self {
        let (commands_tx, commands_rx) = mpsc::channel(16);
        let (status_tx, status_rx) = watch::channel(SchedulerStatus {
            run_state: initial_run_state,
            next_capture_at: None,
            next_capture_rules: Vec::new(),
            last_capture: None,
            exposure: None,
            ramp_state: None,
        });
        tokio::spawn(run_actor(
            camera,
            capture_log,
            capture_dir,
            config_cache_path,
            run_state_path,
            run_state_cache_path,
            initial_run_state,
            commands_rx,
            status_tx,
        ));
        Self {
            commands: commands_tx,
            status: status_rx,
        }
    }

    pub fn status(&self) -> SchedulerStatus {
        self.status.borrow().clone()
    }

    pub async fn pause(&self) -> Result<(), SchedulerUnavailable> {
        self.send(|reply| SchedulerCommand::Pause { reply }).await
    }

    pub async fn resume(&self) -> Result<(), SchedulerUnavailable> {
        self.send(|reply| SchedulerCommand::Resume { reply }).await
    }

    pub async fn shutdown(&self) -> Result<(), SchedulerUnavailable> {
        self.send(|reply| SchedulerCommand::Shutdown { reply })
            .await
    }

    /// Best-effort: if the actor's channel is full or gone, a config
    /// change just gets picked up on the actor's next natural wake
    /// instead — not worth failing the commit request over.
    pub async fn notify_config_changed(&self) {
        let _ = self.commands.send(SchedulerCommand::ConfigChanged).await;
    }

    async fn send(
        &self,
        make_command: impl FnOnce(oneshot::Sender<()>) -> SchedulerCommand,
    ) -> Result<(), SchedulerUnavailable> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(make_command(reply))
            .await
            .map_err(|_| SchedulerUnavailable)?;
        response.await.map_err(|_| SchedulerUnavailable)
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_actor(
    camera: OpticCamera,
    capture_log: Option<CaptureLog>,
    capture_dir: PathBuf,
    config_cache_path: PathBuf,
    run_state_path: PathBuf,
    run_state_cache_path: PathBuf,
    mut run_state: ScheduleRunState,
    mut commands: mpsc::Receiver<SchedulerCommand>,
    status_tx: watch::Sender<SchedulerStatus>,
) {
    // In memory only: a restart reseeds from auto exposure (design doc §5.4).
    let mut ramp: Option<RampState> = None;
    loop {
        if run_state == ScheduleRunState::Paused {
            publish_status(&status_tx, run_state, None, Vec::new());
            let Some(command) = commands.recv().await else {
                return;
            };
            if !handle_command(
                command,
                &mut run_state,
                &run_state_path,
                &run_state_cache_path,
                &status_tx,
            )
            .await
            {
                return;
            }
            continue;
        }

        let config = read_app_config(&config_cache_path).await;
        let tz = config.schedule.tz();
        let now = Utc::now().with_timezone(&tz);
        let mut upcoming = occurrences(&config.schedule, now, LOOKAHEAD).into_iter();
        let next = upcoming.next();
        let gap = next
            .as_ref()
            .and_then(|shot| upcoming.next().map(|after| after.at - shot.at));

        match next {
            Some(shot) => {
                let sleep_for = (shot.at - now)
                    .to_std()
                    .unwrap_or(std::time::Duration::ZERO);
                publish_status(
                    &status_tx,
                    run_state,
                    Some(shot.at.with_timezone(&Utc)),
                    shot.rule_slugs.clone(),
                );
                tokio::select! {
                    () = tokio::time::sleep(sleep_for) => {
                        let (outcome, exposure) = fire_capture(&camera, &capture_log, &capture_dir, &config, &shot, gap, &mut ramp).await;
                        let mut status = status_tx.borrow().clone();
                        status.last_capture = Some(outcome);
                        status.exposure = exposure;
                        status.ramp_state = ramp;
                        let _ = status_tx.send(status);
                    }
                    command = commands.recv() => {
                        let Some(command) = command else { return };
                        if !handle_command(command, &mut run_state, &run_state_path, &run_state_cache_path, &status_tx).await {
                            return;
                        }
                    }
                }
            }
            None => {
                publish_status(&status_tx, run_state, None, Vec::new());
                tokio::select! {
                    () = tokio::time::sleep(IDLE_RECHECK) => {}
                    command = commands.recv() => {
                        let Some(command) = command else { return };
                        if !handle_command(command, &mut run_state, &run_state_path, &run_state_cache_path, &status_tx).await {
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Returns `false` when the actor loop should stop (a `Shutdown` command,
/// or an already-closed channel further up the call chain).
/// Mutates `run_state`, persists it, and publishes the new status *before*
/// replying — never after. Reversing that order would let a caller
/// observe `resume().await` return `Ok(())` while `status()` still
/// reported `Paused`, since the watch channel wouldn't otherwise update
/// until the actor's next natural loop iteration (which the reply doesn't
/// wait for).
async fn handle_command(
    command: SchedulerCommand,
    run_state: &mut ScheduleRunState,
    run_state_path: &std::path::Path,
    run_state_cache_path: &std::path::Path,
    status_tx: &watch::Sender<SchedulerStatus>,
) -> bool {
    match command {
        SchedulerCommand::Pause { reply } => {
            *run_state = ScheduleRunState::Paused;
            persist_run_state(run_state_path, run_state_cache_path, *run_state).await;
            publish_status(status_tx, *run_state, None, Vec::new());
            let _ = reply.send(());
            true
        }
        SchedulerCommand::Resume { reply } => {
            *run_state = ScheduleRunState::Running;
            persist_run_state(run_state_path, run_state_cache_path, *run_state).await;
            publish_status(status_tx, *run_state, None, Vec::new());
            let _ = reply.send(());
            true
        }
        SchedulerCommand::ConfigChanged => true,
        SchedulerCommand::Shutdown { reply } => {
            let _ = reply.send(());
            false
        }
    }
}

fn publish_status(
    tx: &watch::Sender<SchedulerStatus>,
    run_state: ScheduleRunState,
    next_capture_at: Option<DateTime<Utc>>,
    next_capture_rules: Vec<String>,
) {
    let mut status = tx.borrow().clone();
    status.run_state = run_state;
    status.next_capture_at = next_capture_at;
    status.next_capture_rules = next_capture_rules;
    let _ = tx.send(status);
}

async fn read_app_config(config_cache_path: &std::path::Path) -> AppConfig {
    match durable_state::read_cached(config_cache_path).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => AppConfig::default(),
    }
}

async fn persist_run_state(
    durable: &std::path::Path,
    cache: &std::path::Path,
    state: ScheduleRunState,
) {
    let Ok(json) = serde_json::to_string_pretty(&state) else {
        return;
    };
    if let Err(error) = durable_state::write_through(durable, cache, &json).await {
        tracing::warn!(%error, "failed to persist scheduler run state durably");
    }
}

/// The ramp inputs for one `AutoRamp` shot.
struct RampShot {
    settings: exposure_ramp::RampSettings,
    plan: RampPlan,
    target_bias_ev: f64,
    sun_elevation_deg: Option<f64>,
    max_shutter_us: u64,
}

/// Builds a `CaptureRequest` from the committed profile/settings (design
/// doc §6) and, in `AutoRamp` mode, the ramp's exposure
/// (`docs/optic-daemon-exposure-ramping.md` §7): a seed runs auto exposure
/// and AWB, a manual plan sets shutter, gain and colour gains. Everything
/// else (profile, orientation, denoise, DNG policy) is unchanged.
fn build_capture_request(
    config: &AppConfig,
    rule_slugs: &[String],
    plan: Option<&RampPlan>,
) -> CaptureRequest {
    // Binning2k forces its own DNG policy regardless of the committed
    // preference (`validate_raw_policy` rejects it outright). MasterArchive
    // and Dci4k both read `config.save_dng` — as of 2026-09-20,
    // `POST /api/config/save-dng` gives the dashboard's DNG checkbox a real
    // staged/committed path (it previously only fed a one-off manual
    // `CaptureRequest`, never persisted config — reading `config.save_dng`
    // for MasterArchive before that endpoint existed would have silently
    // dropped the DNG from every scheduled frame, since the field could
    // never actually be set to `true`). Now that the field is genuinely
    // settable, a scheduled Master Archive capture respects whatever was
    // last saved, same as every other camera setting already does.
    let save_dng = match config.profile {
        CaptureProfile::Binning2k => false,
        CaptureProfile::MasterArchive | CaptureProfile::Dci4k => config.save_dng,
    };
    let mut settings = config.settings.clone();
    match plan {
        None => {}
        Some(RampPlan::Seed { bias_ev }) => {
            settings.shutter_us = 0;
            settings.gain = 0.0;
            settings.awb = "auto".to_owned();
            settings.colour_gains = None;
            // Auto exposure, but biased toward the ramp's target so the
            // first frame doesn't start from the camera's own default.
            settings.ev = bias_ev.clamp(crate::camera::MIN_EV, crate::camera::MAX_EV);
        }
        Some(RampPlan::Manual {
            shutter_us,
            gain,
            colour_gains,
            ..
        }) => {
            settings.shutter_us = *shutter_us;
            settings.gain = *gain;
            settings.colour_gains = Some(*colour_gains);
        }
    }
    CaptureRequest {
        settings,
        profile: config.profile,
        save_dng,
        source: CaptureSource::Scheduler {
            rule_slugs: rule_slugs.to_vec(),
        },
    }
}

/// Plans the ramp for one shot, or resets it in `Dashboard` mode so that
/// switching back to `AutoRamp` later starts from a fresh seed.
fn plan_ramp_shot(
    config: &AppConfig,
    shot: &ForecastedShot,
    gap: Option<Duration>,
    ramp: &mut Option<RampState>,
    now: DateTime<Utc>,
) -> Option<RampShot> {
    let ScheduleExposure::AutoRamp(settings) = &config.schedule.exposure else {
        *ramp = None;
        return None;
    };
    let settings = settings.sanitized();
    let sun_elevation_deg = config
        .schedule
        .station
        .as_ref()
        .map(|station| sun_elevation_at(station)(shot.at.with_timezone(&Utc)));
    let max_shutter_us = exposure_ramp::max_shutter_for_gap(gap, &settings);
    Some(RampShot {
        plan: exposure_ramp::plan(
            ramp.as_ref(),
            now,
            sun_elevation_deg,
            &settings,
            max_shutter_us,
        ),
        target_bias_ev: exposure_ramp::target_bias_ev(sun_elevation_deg, &settings),
        settings,
        sun_elevation_deg,
        max_shutter_us,
    })
}

/// Fires one scheduled capture (exposed per `build_capture_request`),
/// recording the outcome to the capture history log with `source:
/// "scheduler"` if one is configured, exactly like a manual capture does
/// with `source: "web_ui"`. In `AutoRamp` mode the frame's reported
/// exposure and meter update `ramp`.
async fn fire_capture(
    camera: &OpticCamera,
    capture_log: &Option<CaptureLog>,
    capture_dir: &std::path::Path,
    config: &AppConfig,
    shot: &ForecastedShot,
    gap: Option<Duration>,
    ramp: &mut Option<RampState>,
) -> (LastCapture, Option<RampSnapshot>) {
    let ramp_shot = plan_ramp_shot(config, shot, gap, ramp, Utc::now());
    let request = build_capture_request(
        config,
        &shot.rule_slugs,
        ramp_shot.as_ref().map(|ramp_shot| &ramp_shot.plan),
    );
    let save_dng = request.save_dng;
    let settings = request.settings.clone();
    let source = request.source.clone();
    let requested_at = std::time::SystemTime::now();
    let result = camera.capture_to_stage(capture_dir, request).await;
    let completed_at = std::time::SystemTime::now();

    if let Some(capture_log) = capture_log {
        let entry = CaptureLogEntry::new(
            requested_at,
            completed_at,
            completed_at
                .duration_since(requested_at)
                .unwrap_or_default()
                .as_millis(),
            config.profile,
            settings,
            save_dng,
            &source,
            &result,
        );
        capture_log.record(entry).await;
    }

    let snapshot = ramp_shot.map(|ramp_shot| {
        let exposure = result
            .as_ref()
            .ok()
            .and_then(|capture| capture.exposure)
            .unwrap_or_default();
        if let (Some(exposure_us), Some(analogue_gain), Some(meter)) =
            (exposure.exposure_us, exposure.analogue_gain, exposure.meter)
        {
            let observation = FrameObservation {
                exposure_us: f64::from(exposure_us),
                analogue_gain: f64::from(analogue_gain),
                colour_gains: exposure.colour_gains,
                meter,
            };
            if let Some(next) = exposure_ramp::observe(
                ramp.as_ref(),
                &ramp_shot.plan,
                &observation,
                Utc::now(),
                &ramp_shot.settings,
            ) {
                *ramp = Some(next);
            }
        }
        RampSnapshot {
            at: Utc::now(),
            seed: matches!(ramp_shot.plan, RampPlan::Seed { .. }),
            exposure_us: exposure.exposure_us,
            analogue_gain: exposure.analogue_gain,
            colour_gains: exposure.colour_gains,
            luminance: exposure.meter.map(|meter| meter.luminance),
            target_bias_ev: ramp_shot.target_bias_ev,
            sun_elevation_deg: ramp_shot.sun_elevation_deg,
            max_shutter_us: ramp_shot.max_shutter_us,
        }
    });

    let outcome = match result {
        Ok(_) => LastCapture {
            at: Utc::now(),
            rule_slugs: shot.rule_slugs.clone(),
            success: true,
            error: None,
        },
        Err(error) => {
            tracing::warn!(%error, rules = ?shot.rule_slugs, "scheduled capture failed");
            LastCapture {
                at: Utc::now(),
                rule_slugs: shot.rule_slugs.clone(),
                success: false,
                error: Some(error.to_string()),
            }
        }
    };
    (outcome, snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> DateTime<Tz> {
        chrono_tz::UTC.with_ymd_and_hms(y, m, d, h, min, s).unwrap()
    }

    fn rule(slug: &str, trigger: Trigger) -> Rule {
        Rule {
            id: slug.to_owned(),
            label: slug.to_owned(),
            slug: slug.to_owned(),
            enabled: true,
            trigger,
            constraints: Constraints::default(),
        }
    }

    fn interval(every_secs: u64, align: bool) -> Trigger {
        Trigger::Interval {
            every_secs,
            align_to_wall_clock: align,
        }
    }

    #[test]
    fn interval_unaligned_is_spaced_from_the_call_time() {
        let from = utc(2026, 1, 1, 12, 0, 3);
        let schedule = ScheduleConfig {
            station: None,
            rules: vec![rule("r", interval(60, false))],
            ..ScheduleConfig::default()
        };
        let shots = occurrences(&schedule, from, Duration::seconds(150));
        let times: Vec<_> = shots.iter().map(|s| s.at).collect();
        assert_eq!(
            times,
            vec![utc(2026, 1, 1, 12, 1, 3), utc(2026, 1, 1, 12, 2, 3),]
        );
    }

    #[test]
    fn interval_aligned_snaps_to_wall_clock_boundaries_regardless_of_call_time() {
        let schedule = ScheduleConfig {
            station: None,
            rules: vec![rule("r", interval(300, true))], // 5 minutes
            ..ScheduleConfig::default()
        };
        let from_a = utc(2026, 1, 1, 12, 0, 3);
        let from_b = utc(2026, 1, 1, 12, 3, 47);
        let shots_a = occurrences(&schedule, from_a, Duration::seconds(600));
        let shots_b = occurrences(&schedule, from_b, Duration::seconds(600 - 224));
        // Both calls should agree on the same absolute aligned instants
        // that fall in their respective windows — restart-stability.
        assert_eq!(shots_a[0].at, utc(2026, 1, 1, 12, 5, 0));
        assert_eq!(shots_a[1].at, utc(2026, 1, 1, 12, 10, 0));
        assert_eq!(shots_b[0].at, utc(2026, 1, 1, 12, 5, 0));
    }

    #[test]
    fn recurring_time_every_day_fires_once_per_day() {
        let schedule = ScheduleConfig {
            station: None,
            rules: vec![rule(
                "daily",
                Trigger::RecurringTime {
                    days: RecurringDays::Every,
                    time: NaiveTime::from_hms_opt(13, 0, 0).unwrap(),
                },
            )],
            ..ScheduleConfig::default()
        };
        let from = utc(2026, 3, 1, 0, 0, 0);
        let shots = occurrences(&schedule, from, Duration::days(3));
        let times: Vec<_> = shots.iter().map(|s| s.at).collect();
        assert_eq!(
            times,
            vec![
                utc(2026, 3, 1, 13, 0, 0),
                utc(2026, 3, 2, 13, 0, 0),
                utc(2026, 3, 3, 13, 0, 0),
            ]
        );
    }

    #[test]
    fn recurring_time_weekday_subset_only_fires_on_those_days() {
        // 2026-03-02 is a Monday.
        let schedule = ScheduleConfig {
            station: None,
            rules: vec![rule(
                "mw",
                Trigger::RecurringTime {
                    days: RecurringDays::Weekdays(vec![Weekday::Mon, Weekday::Wed]),
                    time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
                },
            )],
            ..ScheduleConfig::default()
        };
        let from = utc(2026, 3, 1, 0, 0, 0);
        let shots = occurrences(&schedule, from, Duration::days(7));
        let times: Vec<_> = shots.iter().map(|s| s.at).collect();
        assert_eq!(
            times,
            vec![utc(2026, 3, 2, 9, 0, 0), utc(2026, 3, 4, 9, 0, 0)]
        );
    }

    #[test]
    fn recurring_time_nth_weekday_of_month_lands_on_the_right_calendar_dates() {
        // 2nd Wednesday of March 2026 is 2026-03-11; of April 2026 is 2026-04-08.
        let schedule = ScheduleConfig {
            station: None,
            rules: vec![rule(
                "2nd-wed",
                Trigger::RecurringTime {
                    days: RecurringDays::NthWeekdayOfMonth {
                        n: 2,
                        weekday: Weekday::Wed,
                    },
                    time: NaiveTime::from_hms_opt(10, 0, 0).unwrap(),
                },
            )],
            ..ScheduleConfig::default()
        };
        let from = utc(2026, 3, 1, 0, 0, 0);
        let shots = occurrences(&schedule, from, Duration::days(60));
        let times: Vec<_> = shots.iter().map(|s| s.at).collect();
        assert_eq!(
            times,
            vec![utc(2026, 3, 11, 10, 0, 0), utc(2026, 4, 8, 10, 0, 0)]
        );
    }

    #[test]
    fn recurring_time_stays_pinned_to_local_wall_clock_across_a_dst_transition() {
        // US/Canada spring-forward 2026: 2026-03-08, clocks jump 02:00->03:00
        // local in America/Vancouver. A naive fixed-UTC-offset implementation
        // would report 13:00 local as a *different* UTC instant on the 7th
        // vs. the 9th; chrono-tz must keep it pinned to 13:00 local both days.
        let schedule = ScheduleConfig {
            station: Some(Station {
                timezone: "America/Vancouver".to_owned(),
                ..Station::default()
            }),
            rules: vec![rule(
                "daily",
                Trigger::RecurringTime {
                    days: RecurringDays::Every,
                    time: NaiveTime::from_hms_opt(13, 0, 0).unwrap(),
                },
            )],
            ..ScheduleConfig::default()
        };
        let from = utc(2026, 3, 7, 0, 0, 0);
        let shots = occurrences(&schedule, from, Duration::days(3));
        let vancouver = chrono_tz::America::Vancouver;
        let local_times: Vec<_> = shots
            .iter()
            .map(|s| s.at.with_timezone(&vancouver).time())
            .collect();
        assert_eq!(
            local_times,
            vec![
                NaiveTime::from_hms_opt(13, 0, 0).unwrap(),
                NaiveTime::from_hms_opt(13, 0, 0).unwrap(),
                NaiveTime::from_hms_opt(13, 0, 0).unwrap(),
            ],
            "wall-clock time must stay pinned to 13:00 local across the DST transition"
        );
    }

    #[test]
    fn time_window_gates_occurrences_inside_and_outside_the_same_day() {
        let inside = utc(2026, 1, 1, 10, 0, 0);
        let outside = utc(2026, 1, 1, 20, 0, 0);
        let window = TimeWindow {
            days: RecurringDays::Every,
            start: NaiveTime::from_hms_opt(8, 0, 0).unwrap(),
            end: NaiveTime::from_hms_opt(16, 0, 0).unwrap(),
        };
        assert!(window.holds(inside));
        assert!(!window.holds(outside));
    }

    #[test]
    fn time_window_overnight_wrap_covers_both_sides_of_midnight() {
        let late_night = utc(2026, 1, 1, 23, 0, 0);
        let early_morning = utc(2026, 1, 2, 4, 0, 0);
        let midday = utc(2026, 1, 1, 12, 0, 0);
        let window = TimeWindow {
            days: RecurringDays::Every,
            start: NaiveTime::from_hms_opt(22, 0, 0).unwrap(),
            end: NaiveTime::from_hms_opt(8, 0, 0).unwrap(),
        };
        assert!(window.holds(late_night));
        assert!(window.holds(early_morning));
        assert!(!window.holds(midday));
    }

    #[test]
    fn composition_unions_two_non_overlapping_rules() {
        let schedule = ScheduleConfig {
            station: None,
            rules: vec![
                rule("daytime", interval(300, true)),
                rule(
                    "night",
                    Trigger::RecurringTime {
                        days: RecurringDays::Every,
                        time: NaiveTime::from_hms_opt(2, 0, 0).unwrap(),
                    },
                ),
            ],
            ..ScheduleConfig::default()
        };
        let from = utc(2026, 1, 1, 0, 0, 0);
        let shots = occurrences(&schedule, from, Duration::minutes(11));
        // 5-minute-aligned daytime shots at :05 and :10, plus no night shot
        // (02:00 is outside this short horizon) — just confirms both rules'
        // occurrences appear in one sorted, unioned timeline.
        let times: Vec<_> = shots.iter().map(|s| s.at).collect();
        assert_eq!(
            times,
            vec![utc(2026, 1, 1, 0, 5, 0), utc(2026, 1, 1, 0, 10, 0)]
        );
        assert!(
            shots
                .iter()
                .all(|s| s.rule_slugs == vec!["daytime".to_owned()])
        );
    }

    #[test]
    fn merge_combines_occurrences_within_the_window_and_tags_both_rules() {
        let raw = vec![
            (utc(2026, 1, 1, 10, 5, 0), "weekday-baseline"),
            (utc(2026, 1, 1, 10, 5, 3), "site-visit"),
        ];
        let shots = merge(raw);
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].at, utc(2026, 1, 1, 10, 5, 0));
        assert_eq!(
            shots[0].rule_slugs,
            vec!["site-visit".to_owned(), "weekday-baseline".to_owned()]
        );
    }

    #[test]
    fn merge_keeps_occurrences_outside_the_window_separate() {
        let raw = vec![
            (utc(2026, 1, 1, 10, 5, 0), "a"),
            (utc(2026, 1, 1, 10, 5, 11), "b"), // 11s > 10s MERGE_WINDOW
        ];
        let shots = merge(raw);
        assert_eq!(shots.len(), 2);
        assert_eq!(shots[0].rule_slugs, vec!["a".to_owned()]);
        assert_eq!(shots[1].rule_slugs, vec!["b".to_owned()]);
    }

    #[test]
    fn merge_collapses_a_single_fast_rule_with_itself() {
        // A 2s interval is faster than MERGE_WINDOW (10s) — deliberately
        // merges with itself, per the doc comment on `merge()`.
        let raw = vec![
            (utc(2026, 1, 1, 10, 0, 0), "fast"),
            (utc(2026, 1, 1, 10, 0, 2), "fast"),
            (utc(2026, 1, 1, 10, 0, 4), "fast"),
        ];
        let shots = merge(raw);
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].rule_slugs, vec!["fast".to_owned()]);
    }

    #[test]
    fn dead_rule_produces_zero_occurrences_without_erroring() {
        // A zero-width window (start == end) can never hold — `local_time
        // >= start && local_time < end` is false for every `local_time`
        // when start == end. Exercises that a constraint which can never
        // be satisfied produces an empty result, not a panic. (An earlier
        // version of this test used two conflicting `TimeWindow`s to
        // achieve the same "always false" effect — no longer
        // constructible now that `Constraints` allows at most one.)
        let schedule = ScheduleConfig {
            station: None,
            rules: vec![Rule {
                id: "dead".into(),
                label: "dead".into(),
                slug: "dead".into(),
                enabled: true,
                trigger: interval(60, true),
                constraints: Constraints {
                    time_window: Some(TimeWindow {
                        days: RecurringDays::Every,
                        start: NaiveTime::from_hms_opt(8, 0, 0).unwrap(),
                        end: NaiveTime::from_hms_opt(8, 0, 0).unwrap(),
                    }),
                    ..Default::default()
                },
            }],
            ..ScheduleConfig::default()
        };
        let from = utc(2026, 1, 1, 0, 0, 0);
        let shots = occurrences(&schedule, from, Duration::days(2));
        assert!(shots.is_empty());
    }

    #[test]
    fn dead_rule_slugs_flags_only_enabled_rules_with_zero_contributed_shots() {
        let dead = Rule {
            constraints: Constraints {
                time_window: Some(TimeWindow {
                    days: RecurringDays::Every,
                    start: NaiveTime::from_hms_opt(8, 0, 0).unwrap(),
                    end: NaiveTime::from_hms_opt(8, 0, 0).unwrap(),
                }),
                ..Default::default()
            },
            ..rule("dead", interval(60, true))
        };
        let mut disabled_dead = dead.clone();
        disabled_dead.id = "disabled-dead".into();
        disabled_dead.slug = "disabled-dead".into();
        disabled_dead.enabled = false;
        let alive = rule("alive", interval(300, true));
        let schedule = ScheduleConfig {
            station: None,
            rules: vec![dead, disabled_dead, alive],
            ..ScheduleConfig::default()
        };
        let from = utc(2026, 1, 1, 0, 0, 0);
        let shots = occurrences(&schedule, from, Duration::hours(1));

        // Only the *enabled* dead rule is flagged — a disabled rule
        // producing zero shots is expected, not a misconfiguration to
        // warn about.
        assert_eq!(dead_rule_slugs(&schedule, &shots), vec!["dead".to_owned()]);
    }

    #[test]
    fn overlap_advisories_finds_nothing_for_rules_with_disjoint_time_windows() {
        // Design doc §4's own worked example: Peak/Off hours rules
        // partition the day cleanly via non-overlapping `TimeWindow`
        // constraints, so their occurrence ranges never overlap at all —
        // the case the advisory must correctly report nothing for.
        let morning = Rule {
            constraints: Constraints {
                time_window: Some(TimeWindow {
                    days: RecurringDays::Every,
                    start: NaiveTime::from_hms_opt(8, 0, 0).unwrap(),
                    end: NaiveTime::from_hms_opt(12, 0, 0).unwrap(),
                }),
                ..Default::default()
            },
            ..rule("morning-only", interval(300, true))
        };
        let evening = Rule {
            constraints: Constraints {
                time_window: Some(TimeWindow {
                    days: RecurringDays::Every,
                    start: NaiveTime::from_hms_opt(18, 0, 0).unwrap(),
                    end: NaiveTime::from_hms_opt(22, 0, 0).unwrap(),
                }),
                ..Default::default()
            },
            ..rule("evening-only", interval(300, true))
        };
        let schedule = ScheduleConfig {
            station: None,
            rules: vec![morning, evening],
            ..ScheduleConfig::default()
        };
        let from = utc(2026, 1, 1, 0, 0, 0);
        let shots = occurrences(&schedule, from, Duration::hours(24));
        assert!(!shots.is_empty());
        assert!(overlap_advisories(&schedule, &shots).is_empty());
    }

    #[test]
    fn overlap_advisories_reports_a_shared_span_for_two_sustained_different_cadence_rules() {
        // Mirrors design doc §3.1's own example: a 5-minute-cadence
        // "baseline" rule and a 30-second-cadence "site visit" rule, both
        // active over the same hour — most of their individual instants
        // land more than MERGE_WINDOW (10s) apart, so they stay
        // separately attributed (no single merged, multi-tagged shot),
        // but the combined rate over their shared span is visibly higher
        // than either alone — exactly what the merge+tag mechanism does
        // *not* surface on its own (§3.1's "what this does not solve").
        let schedule = ScheduleConfig {
            station: None,
            rules: vec![
                rule("baseline", interval(300, true)),
                rule("site-visit", interval(30, true)),
            ],
            ..ScheduleConfig::default()
        };
        let from = utc(2026, 1, 1, 10, 0, 0);
        let shots = occurrences(&schedule, from, Duration::minutes(30));

        let advisories = overlap_advisories(&schedule, &shots);
        assert_eq!(advisories.len(), 1);
        let advisory = &advisories[0];
        assert_eq!(
            advisory.rule_slugs,
            ["baseline".to_owned(), "site-visit".to_owned()]
        );
        assert!(advisory.window_start < advisory.window_end);
        // "baseline" contributes at most 6 shots across the *entire*
        // 30-minute horizon (5-minute cadence) — so any count clearly
        // above that within the (narrower) overlap window can only come
        // from "site-visit"'s much faster 30s cadence also contributing,
        // which is exactly the "combined rate exceeds either rule alone"
        // signal this advisory exists to surface.
        assert!(advisory.combined_shots > 6);
    }

    #[test]
    fn overlap_advisories_ignores_disabled_rules() {
        let mut disabled = rule("site-visit", interval(30, true));
        disabled.enabled = false;
        let schedule = ScheduleConfig {
            station: None,
            rules: vec![rule("baseline", interval(300, true)), disabled],
            ..ScheduleConfig::default()
        };
        let from = utc(2026, 1, 1, 10, 0, 0);
        let shots = occurrences(&schedule, from, Duration::minutes(30));
        assert!(overlap_advisories(&schedule, &shots).is_empty());
    }

    #[test]
    fn disabled_rule_produces_no_occurrences() {
        let mut r = rule("r", interval(60, true));
        r.enabled = false;
        let schedule = ScheduleConfig {
            station: None,
            rules: vec![r],
            ..ScheduleConfig::default()
        };
        let from = utc(2026, 1, 1, 0, 0, 0);
        assert!(occurrences(&schedule, from, Duration::minutes(10)).is_empty());
    }

    #[test]
    fn slug_validation_rejects_empty() {
        let rules = vec![rule("", interval(60, true))];
        assert_eq!(
            validate_rule_slugs(&rules),
            Err(SlugError::Empty { rule_index: 0 })
        );
    }

    #[test]
    fn slug_validation_rejects_invalid_characters() {
        let rules = vec![rule("Not Valid!", interval(60, true))];
        assert_eq!(
            validate_rule_slugs(&rules),
            Err(SlugError::InvalidFormat {
                rule_index: 0,
                slug: "Not Valid!".to_owned()
            })
        );
    }

    #[test]
    fn slug_validation_rejects_duplicates_even_when_the_other_rule_is_disabled() {
        // Format validation already forces lowercase-only, so two
        // format-valid slugs can never differ *only* by case — the
        // case-insensitive comparison in `validate_rule_slugs` is
        // defense-in-depth against that becoming false later, not
        // something distinguishable from plain equality today. This test
        // covers what's actually reachable: a disabled rule's slug still
        // blocks reuse, which the rejected "case insensitive" framing
        // would have obscured.
        let mut second = rule("daytime", interval(60, true));
        second.enabled = false;
        let rules = vec![rule("daytime", interval(60, true)), second];
        assert_eq!(
            validate_rule_slugs(&rules),
            Err(SlugError::Duplicate {
                rule_index: 1,
                other_index: 0,
                slug: "daytime".to_owned()
            })
        );
    }

    #[test]
    fn slug_validation_accepts_distinct_valid_slugs() {
        let rules = vec![
            rule("daytime", interval(60, true)),
            rule("night-2", interval(60, true)),
        ];
        assert_eq!(validate_rule_slugs(&rules), Ok(()));
    }

    /// Regression test: `RecurringDays::Weekdays(Vec<Weekday>)` is a
    /// newtype variant wrapping a sequence — serde cannot represent that
    /// under pure internal tagging (`tag = "kind"` alone) at all; it
    /// panics at serialize time. Caught by hand while building the
    /// frontend (nothing in this file's other tests ever round-tripped
    /// `RecurringDays` through `serde_json`), fixed with adjacent tagging
    /// (`tag = "kind", content = "value"`). This test is what makes sure
    /// it stays fixed.
    #[test]
    fn recurring_days_round_trips_through_json_including_weekdays() {
        for value in [
            RecurringDays::Every,
            RecurringDays::Weekdays(vec![Weekday::Mon, Weekday::Wed]),
            RecurringDays::NthWeekdayOfMonth {
                n: 2,
                weekday: Weekday::Wed,
            },
        ] {
            let json = serde_json::to_string(&value).expect("RecurringDays must serialize");
            let parsed: RecurringDays =
                serde_json::from_str(&json).expect("RecurringDays must round-trip");
            assert_eq!(parsed, value);
        }
    }

    #[test]
    fn rule_with_weekdays_time_window_round_trips_through_json() {
        let original = rule(
            "weekday-daytime",
            Trigger::RecurringTime {
                days: RecurringDays::Weekdays(vec![Weekday::Mon, Weekday::Wed, Weekday::Fri]),
                time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            },
        );
        let json = serde_json::to_string(&original).expect("Rule must serialize");
        let parsed: Rule = serde_json::from_str(&json).expect("Rule must round-trip");
        assert_eq!(parsed, original);
    }

    /// Unlike the test above (which despite its name never actually
    /// populates a constraint), this one exercises the `Constraints`
    /// struct itself — the gap that motivated adding it: nothing in this
    /// suite previously round-tripped a populated `TimeWindow` constraint
    /// through JSON at all.
    #[test]
    fn rule_with_time_window_constraint_round_trips_through_json() {
        let mut original = rule("daytime-only", interval(300, true));
        original.constraints = Constraints {
            time_window: Some(TimeWindow {
                days: RecurringDays::Weekdays(vec![Weekday::Mon, Weekday::Tue]),
                start: NaiveTime::from_hms_opt(8, 0, 0).unwrap(),
                end: NaiveTime::from_hms_opt(18, 0, 0).unwrap(),
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&original).expect("Rule must serialize");
        let parsed: Rule = serde_json::from_str(&json).expect("Rule must round-trip");
        assert_eq!(parsed, original);
    }
}

// `OpticCamera::spawn_native()` on real Linux/libcamera hardware enforces a
// hard "only one CameraManager per process" rule — confirmed the hard way,
// deploying to the Pi: several of these tests spawning their own instance
// concurrently (the test harness runs tests in parallel by default) segfaults
// libcamera with "Multiple CameraManager objects are not allowed." On macOS
// there's no real camera manager to collide over, so this never surfaced
// locally. `optic_camera.rs`'s own actor test has the identical gate for the
// identical reason — matched here, not invented fresh.
#[cfg(all(test, not(target_os = "linux")))]
mod actor_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn schedule_config_without_exposure_defaults_to_dashboard() {
        // A config.json committed before exposure ramping existed.
        let schedule: ScheduleConfig =
            serde_json::from_str(r#"{"station": null, "rules": []}"#).unwrap();
        assert_eq!(schedule.exposure, ScheduleExposure::Dashboard);
    }

    #[test]
    fn schedule_config_round_trips_an_auto_ramp_exposure() {
        let schedule = ScheduleConfig {
            exposure: ScheduleExposure::AutoRamp(exposure_ramp::RampSettings {
                max_gain: 4.0,
                ..exposure_ramp::RampSettings::default()
            }),
            ..ScheduleConfig::default()
        };
        let json = serde_json::to_string(&schedule).unwrap();
        assert!(json.contains(r#""mode":"AutoRamp""#));
        let parsed: ScheduleConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, schedule);
    }

    fn committed_config() -> AppConfig {
        AppConfig {
            profile: CaptureProfile::Dci4k,
            save_dng: true,
            settings: crate::camera::CameraSettings {
                rotation: 180,
                awb: "daylight".to_owned(),
                shutter_us: 20_000,
                gain: 2.0,
                ..crate::camera::CameraSettings::default()
            },
            ..AppConfig::default()
        }
    }

    #[test]
    fn dashboard_mode_request_matches_the_committed_settings() {
        // Regression: Dashboard mode must build exactly the pre-ramping request.
        let config = committed_config();
        let slugs = vec!["daytime".to_owned()];
        let request = build_capture_request(&config, &slugs, None);
        assert_eq!(
            serde_json::to_value(&request.settings).unwrap(),
            serde_json::to_value(&config.settings).unwrap()
        );
        assert_eq!(request.profile, CaptureProfile::Dci4k);
        assert!(request.save_dng);
        assert_eq!(
            request.source,
            CaptureSource::Scheduler { rule_slugs: slugs }
        );
    }

    #[test]
    fn ramp_plans_override_only_exposure_and_white_balance() {
        let config = committed_config();
        let seed = build_capture_request(&config, &[], Some(&RampPlan::Seed { bias_ev: -2.5 }));
        assert_eq!((seed.settings.shutter_us, seed.settings.gain), (0, 0.0));
        assert_eq!(seed.settings.awb, "auto");
        assert_eq!(seed.settings.colour_gains, None);
        // Auto exposure, biased toward the ramp's target.
        assert_eq!(seed.settings.ev, -2.5);
        seed.validate().unwrap();
        // Beyond libcamera's compensation range it clamps and stays valid.
        let deep = build_capture_request(&config, &[], Some(&RampPlan::Seed { bias_ev: -9.0 }));
        assert_eq!(deep.settings.ev, crate::camera::MIN_EV);
        deep.validate().unwrap();

        let manual = build_capture_request(
            &config,
            &[],
            Some(&RampPlan::Manual {
                shutter_us: 2_000_000,
                gain: 3.5,
                colour_gains: [2.1, 1.7],
                log2_exposure: 22.7,
            }),
        );
        assert_eq!(manual.settings.shutter_us, 2_000_000);
        assert_eq!(manual.settings.gain, 3.5);
        assert_eq!(manual.settings.colour_gains, Some([2.1, 1.7]));
        assert_eq!(manual.settings.rotation, 180);
        assert!(manual.save_dng);
        manual.validate().unwrap();
    }

    #[test]
    fn plan_ramp_shot_resets_state_in_dashboard_mode_and_seeds_in_auto_ramp() {
        let now = Utc::now();
        let shot = ForecastedShot {
            at: now.with_timezone(&chrono_tz::UTC),
            rule_slugs: vec!["dusk".to_owned()],
        };
        let mut ramp = Some(RampState {
            updated_at: now,
            scene_ev: -12.0,
            planned_log2_exposure: Some(10.0),
            colour_gains: [2.0, 1.6],
        });
        let dashboard = committed_config();
        assert!(plan_ramp_shot(&dashboard, &shot, None, &mut ramp, now).is_none());
        assert!(ramp.is_none());

        let mut auto = committed_config();
        auto.schedule.exposure = ScheduleExposure::AutoRamp(exposure_ramp::RampSettings::default());
        let ramp_shot =
            plan_ramp_shot(&auto, &shot, Some(Duration::seconds(30)), &mut ramp, now).unwrap();
        assert!(matches!(ramp_shot.plan, RampPlan::Seed { .. }));
        assert_eq!(ramp_shot.max_shutter_us, 2_045_454);
        // No station: the target stays at the day level.
        assert_eq!(ramp_shot.sun_elevation_deg, None);
        assert_eq!(ramp_shot.target_bias_ev, 0.0);
    }

    #[test]
    fn live_exposure_plan_is_none_in_dashboard_mode() {
        let schedule = ScheduleConfig::default();
        assert_eq!(live_exposure_plan(&schedule, None, Utc::now()), None);
    }

    #[test]
    fn live_exposure_plan_seeds_without_state_and_tracks_the_ramp_with_it() {
        use chrono::TimeZone as _;
        let now = Utc.with_ymd_and_hms(2026, 9, 22, 8, 40, 0).unwrap();
        let station = Station {
            latitude: 49.22,
            longitude: -123.0,
            elevation_m: 86.0,
            timezone: "America/Vancouver".to_owned(),
        };
        let settings = exposure_ramp::RampSettings::default();
        let schedule = ScheduleConfig {
            station: Some(station.clone()),
            rules: vec![Rule {
                id: "every-minute".to_owned(),
                label: "Every minute".to_owned(),
                slug: "every-minute".to_owned(),
                enabled: true,
                trigger: Trigger::Interval {
                    every_secs: 60,
                    align_to_wall_clock: true,
                },
                constraints: Constraints::default(),
            }],
            exposure: ScheduleExposure::AutoRamp(settings),
        };

        let seeding = live_exposure_plan(&schedule, None, now).unwrap();
        assert!(seeding.seeding);
        assert_eq!((seeding.shutter_us, seeding.gain), (None, None));
        // 60 s interval -> the capture-time budget caps the shutter.
        assert_eq!(seeding.max_shutter_us, 4_227_272);
        // 01:40 local: deep night, full night drop.
        assert!(seeding.sun_elevation_deg.unwrap() < -18.0);
        assert_eq!(seeding.target_bias_ev, -2.0);

        let state = RampState {
            updated_at: now - Duration::minutes(1),
            scene_ev: -26.0,
            planned_log2_exposure: Some(21.0),
            colour_gains: [2.6, 1.8],
        };
        let live = live_exposure_plan(&schedule, Some(&state), now).unwrap();
        let RampPlan::Manual {
            shutter_us,
            gain,
            colour_gains,
            ..
        } = exposure_ramp::plan(
            Some(&state),
            now,
            seeding.sun_elevation_deg,
            &settings,
            4_227_272,
        )
        else {
            panic!("expected a manual plan");
        };
        assert!(!live.seeding);
        assert_eq!(live.shutter_us, Some(shutter_us));
        assert_eq!(live.gain, Some(gain));
        assert_eq!(live.colour_gains, Some(colour_gains));
        assert_eq!(live.ramp_updated_at, Some(state.updated_at));
    }

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "optic-scheduler-test-{label}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn spawn_test_handle(initial: ScheduleRunState) -> (SchedulerHandle, std::path::PathBuf) {
        let capture_dir = unique_temp_dir("capture");
        let config_cache_path = unique_temp_dir("config-cache").join("config.json");
        let run_state_dir = unique_temp_dir("run-state");
        let run_state_path = run_state_dir.join("schedule_run_state.json");
        let run_state_cache_path =
            unique_temp_dir("run-state-cache").join("schedule_run_state.json");
        let handle = SchedulerHandle::spawn(
            crate::optic_camera::OpticCamera::spawn_native(),
            None,
            capture_dir,
            config_cache_path,
            run_state_path.clone(),
            run_state_cache_path,
            initial,
        );
        (handle, run_state_path)
    }

    #[tokio::test]
    async fn status_reflects_the_initial_run_state_immediately() {
        // `SchedulerHandle::spawn` sets up the watch channel's initial
        // value before the actor task ever runs, so this is available
        // synchronously — no need to wait for the task to be scheduled.
        let (handle, _run_state_path) = spawn_test_handle(ScheduleRunState::Paused);
        assert_eq!(handle.status().run_state, ScheduleRunState::Paused);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn resume_updates_status_before_returning_no_race() {
        let (handle, run_state_path) = spawn_test_handle(ScheduleRunState::Paused);
        handle.resume().await.unwrap();
        // Per `handle_command`'s doc comment: status must already reflect
        // Running by the time `resume()` returns, not "eventually."
        assert_eq!(handle.status().run_state, ScheduleRunState::Running);
        // And durably persisted, not just published in memory.
        let persisted = tokio::fs::read_to_string(&run_state_path).await.unwrap();
        assert_eq!(
            serde_json::from_str::<ScheduleRunState>(&persisted).unwrap(),
            ScheduleRunState::Running
        );
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn pause_updates_status_before_returning_no_race() {
        let (handle, run_state_path) = spawn_test_handle(ScheduleRunState::Running);
        handle.pause().await.unwrap();
        assert_eq!(handle.status().run_state, ScheduleRunState::Paused);
        let persisted = tokio::fs::read_to_string(&run_state_path).await.unwrap();
        assert_eq!(
            serde_json::from_str::<ScheduleRunState>(&persisted).unwrap(),
            ScheduleRunState::Paused
        );
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_stops_the_actor_cleanly() {
        let (handle, _run_state_path) = spawn_test_handle(ScheduleRunState::Paused);
        handle.shutdown().await.unwrap();
        // A second command after shutdown must fail gracefully
        // (SchedulerUnavailable), not hang or panic.
        assert!(handle.pause().await.is_err());
    }

    #[tokio::test]
    async fn notify_config_changed_does_not_hang_or_panic_while_idle() {
        let (handle, _run_state_path) = spawn_test_handle(ScheduleRunState::Running);
        handle.notify_config_changed().await;
        // Still responsive afterward.
        assert_eq!(handle.status().run_state, ScheduleRunState::Running);
        handle.shutdown().await.unwrap();
    }
}
