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
    durable_state,
    optic_camera::OpticCamera,
    optic_capture_log::{CaptureLog, CaptureLogEntry},
};

/// Occurrences within this window of each other collapse into one physical
/// capture (design doc §3.1). Chosen to be shorter than any real capture
/// takes, so it's physically impossible for the sensor to have honored two
/// occurrences this close together as separate shots anyway.
pub const MERGE_WINDOW: Duration = Duration::seconds(5);

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
    // `Ephemeris` (Solar/Lunar/MilkyWay) is Phase 2+ — not part of this
    // slice. See design doc §2 / the phase-1 worklog.
}

fn default_align_to_wall_clock() -> bool {
    true
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
/// Future Phase 2+ constraint types (`SunElevationWindow`,
/// `MoonElevationWindow`, `MoonIlluminationWindow`,
/// `MilkyWayElevationWindow` — design doc §2) get added here the same way,
/// one field at a time, as each is actually implemented — not stubbed out
/// in advance.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Constraints {
    pub time_window: Option<TimeWindow>,
}

impl Constraints {
    fn holds(&self, when: DateTime<Tz>) -> bool {
        self.time_window.as_ref().is_none_or(|c| c.holds(when))
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

    let mut raw: Vec<(DateTime<Tz>, &str)> = Vec::new();
    for rule in &schedule.rules {
        if !rule.enabled {
            continue;
        }
        for when in rule_occurrences(&rule.trigger, tz, from, until) {
            if rule.constraints.holds(when) {
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
        let next = occurrences(&config.schedule, now, LOOKAHEAD)
            .into_iter()
            .next();

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
                        let outcome = fire_capture(&camera, &capture_log, &capture_dir, &config, &shot).await;
                        let mut status = status_tx.borrow().clone();
                        status.last_capture = Some(outcome);
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

/// Builds a `CaptureRequest` from whatever profile/settings are currently
/// committed (design doc §6 — no scheduler-specific override) and fires
/// it, recording the outcome to the capture history log with `source:
/// "scheduler"` if one is configured, exactly like a manual capture does
/// with `source: "web_ui"`.
async fn fire_capture(
    camera: &OpticCamera,
    capture_log: &Option<CaptureLog>,
    capture_dir: &std::path::Path,
    config: &AppConfig,
    shot: &ForecastedShot,
) -> LastCapture {
    // MasterArchive/Binning2k force their own DNG policy regardless of the
    // committed preference (`validate_raw_policy`); only Dci4k actually
    // reads it. Computed this way, rather than trusting `config.save_dng`
    // outright, so a scheduled capture can never violate that policy.
    let save_dng = match config.profile {
        CaptureProfile::MasterArchive => true,
        CaptureProfile::Binning2k => false,
        CaptureProfile::Dci4k => config.save_dng,
    };
    let request = CaptureRequest {
        settings: config.settings.clone(),
        profile: config.profile,
        save_dng,
        source: CaptureSource::Scheduler {
            rule_slugs: shot.rule_slugs.clone(),
        },
    };
    let requested_at = std::time::SystemTime::now();
    let result = camera.capture_to_stage(capture_dir, request).await;
    let completed_at = std::time::SystemTime::now();

    if let Some(capture_log) = capture_log {
        let mut entry = CaptureLogEntry::new(
            requested_at,
            completed_at,
            completed_at
                .duration_since(requested_at)
                .unwrap_or_default()
                .as_millis(),
            config.profile,
            config.settings.clone(),
            save_dng,
            &result,
        );
        entry.source = "scheduler".to_owned();
        capture_log.record(entry).await;
    }

    match result {
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
    }
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
            (utc(2026, 1, 1, 10, 5, 6), "b"), // 6s > 5s MERGE_WINDOW
        ];
        let shots = merge(raw);
        assert_eq!(shots.len(), 2);
        assert_eq!(shots[0].rule_slugs, vec!["a".to_owned()]);
        assert_eq!(shots[1].rule_slugs, vec!["b".to_owned()]);
    }

    #[test]
    fn merge_collapses_a_single_fast_rule_with_itself() {
        // A 2s interval is faster than MERGE_WINDOW (5s) — deliberately
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
                },
            }],
        };
        let from = utc(2026, 1, 1, 0, 0, 0);
        let shots = occurrences(&schedule, from, Duration::days(2));
        assert!(shots.is_empty());
    }

    #[test]
    fn disabled_rule_produces_no_occurrences() {
        let mut r = rule("r", interval(60, true));
        r.enabled = false;
        let schedule = ScheduleConfig {
            station: None,
            rules: vec![r],
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
