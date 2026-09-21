//! In-daemon health alerts (`docs/optic-daemon-alerts.md`).
//!
//! A passive observer, like `optic_capture_log`: every 30s it reads the
//! scheduler, sync, capture-history, and system-status signals the daemon
//! already has, evaluates a fixed set of failure conditions, and sends one
//! notification when a condition starts, a reminder while it persists, and
//! a recovery notice when it clears. It never changes camera, scheduler, or
//! sync state, and nothing that goes wrong in here (bad config, a failed
//! delivery) affects capture or transfer.
//!
//! Split the same way as `optic_scheduler`: a pure half (`Monitor`,
//! `Debouncer`, `Outbox`, config parsing) that takes an explicit `now` and
//! is unit-tested with a fake clock on macOS, and a small actor
//! (`AlertsHandle`) that gathers real signals and delivers notifications.
//! Delivery shells out to the system `curl` (the same pattern `optic_sync`
//! uses for `ssh`) rather than adding an HTTP client crate.

use std::{
    collections::VecDeque,
    env,
    path::{Path, PathBuf},
    process::Stdio,
};

use chrono::{DateTime, Duration, TimeZone as _, Utc};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt as _, process::Command, sync::watch};

use crate::{
    camera::AppConfig,
    durable_state,
    optic_capture_log::{CaptureLog, CaptureQueryFilter},
    optic_scheduler::{self, LastCapture, ScheduleConfig, ScheduleRunState, SchedulerHandle},
    optic_sync::DataSyncManager,
    system_status::SystemStatusReader,
};

pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
/// Undelivered notifications kept for retry; the oldest is dropped beyond this.
const OUTBOX_CAPACITY: usize = 32;
/// Scheduled-capture outcomes remembered by the fallback tracker used when
/// the capture history database is unavailable.
const OUTCOME_HISTORY: usize = 16;
/// `capture_stalled` never replays the schedule further back than this, so a
/// long-dead station doesn't make every tick compute days of occurrences.
const STALL_LOOKBACK: Duration = Duration::hours(24);
const CURL_TIMEOUT_SECS: u64 = 10;
const DEFAULT_NTFY_SERVER: &str = "https://ntfy.sh";
const DEFAULT_CONFIG_RELATIVE_PATH: &str = ".config/optic-daemon/alerts.json";

// ---------------------------------------------------------------------------
// Conditions and thresholds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    CaptureStalled,
    CaptureOverdue,
    CaptureFailing,
    SyncBacklog,
    CaptureTmpfsLow,
    TemperatureHigh,
    Throttled,
}

impl Condition {
    pub const ALL: [Condition; 7] = [
        Condition::CaptureStalled,
        Condition::CaptureOverdue,
        Condition::CaptureFailing,
        Condition::SyncBacklog,
        Condition::CaptureTmpfsLow,
        Condition::TemperatureHigh,
        Condition::Throttled,
    ];

    fn index(self) -> usize {
        self as usize
    }

    fn title(self) -> &'static str {
        match self {
            Self::CaptureStalled => "captures stalled",
            Self::CaptureOverdue => "scheduled capture overdue",
            Self::CaptureFailing => "captures failing",
            Self::SyncBacklog => "sync backlog",
            Self::CaptureTmpfsLow => "capture tmpfs low",
            Self::TemperatureHigh => "CPU temperature high",
            Self::Throttled => "throttled / under-voltage",
        }
    }
}

/// Defaults are the "Balanced" preset confirmed on 2026-09-20 (design doc
/// §5). Every field is optional in `alerts.json`; unknown fields are
/// rejected so a misspelt override is reported instead of ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Thresholds {
    pub capture_stalled_after_secs: u64,
    pub capture_overdue_after_secs: u64,
    pub capture_failures_in_a_row: u32,
    pub sync_backlog_after_secs: u64,
    pub tmpfs_low_fire_below_percent: f64,
    pub tmpfs_low_clear_above_percent: f64,
    pub temperature_fire_at_celsius: f32,
    pub temperature_clear_below_celsius: f32,
    /// How long `temperature_high` and `throttled` must hold before firing.
    pub sustained_secs: u64,
    pub recover_after_secs: u64,
    pub repeat_every_secs: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            capture_stalled_after_secs: 15 * 60,
            capture_overdue_after_secs: 5 * 60,
            capture_failures_in_a_row: 3,
            sync_backlog_after_secs: 30 * 60,
            tmpfs_low_fire_below_percent: 25.0,
            tmpfs_low_clear_above_percent: 35.0,
            temperature_fire_at_celsius: 80.0,
            temperature_clear_below_celsius: 75.0,
            sustained_secs: 5 * 60,
            recover_after_secs: 5 * 60,
            repeat_every_secs: 6 * 3600,
        }
    }
}

impl Thresholds {
    fn fire_after(&self, condition: Condition) -> Duration {
        match condition {
            Condition::TemperatureHigh | Condition::Throttled => secs(self.sustained_secs),
            _ => Duration::zero(),
        }
    }

    fn timing(&self, condition: Condition) -> Timing {
        Timing {
            fire_after: self.fire_after(condition),
            recover_after: secs(self.recover_after_secs),
            repeat_every: secs(self.repeat_every_secs),
        }
    }

    fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.tmpfs_low_clear_above_percent < self.tmpfs_low_fire_below_percent {
            warnings.push(
                "tmpfs_low_clear_above_percent is below tmpfs_low_fire_below_percent; the alert may flap"
                    .to_owned(),
            );
        }
        if self.temperature_clear_below_celsius > self.temperature_fire_at_celsius {
            warnings.push(
                "temperature_clear_below_celsius is above temperature_fire_at_celsius; the alert may flap"
                    .to_owned(),
            );
        }
        let durations = [
            self.capture_stalled_after_secs,
            self.capture_overdue_after_secs,
            self.sync_backlog_after_secs,
            self.sustained_secs,
            self.recover_after_secs,
            self.repeat_every_secs,
        ];
        if durations.iter().any(|&value| value > MAX_THRESHOLD_SECS) {
            warnings.push(format!(
                "durations above {MAX_THRESHOLD_SECS}s (30 days) are clamped to 30 days"
            ));
        }
        if self.repeat_every_secs == 0 {
            warnings.push("repeat_every_secs is 0; reminders are disabled".to_owned());
        }
        warnings
    }
}

/// Threshold durations are clamped to this so an absurd config value can't
/// overflow `DateTime` arithmetic (release builds use `panic = "abort"`, so
/// an overflow panic here would take the whole daemon down).
const MAX_THRESHOLD_SECS: u64 = 30 * 24 * 3600;

fn secs(value: u64) -> Duration {
    Duration::seconds(value.min(MAX_THRESHOLD_SECS) as i64)
}

// ---------------------------------------------------------------------------
// Debounce state machine (pure; design doc §5 "Debounce, recovery, repeat")
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Timing {
    fire_after: Duration,
    recover_after: Duration,
    /// Zero disables reminders.
    repeat_every: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Clear,
    Pending {
        since: DateTime<Utc>,
    },
    Firing {
        since: DateTime<Utc>,
        last_notified: DateTime<Utc>,
    },
    Recovering {
        since: DateTime<Utc>,
        clear_since: DateTime<Utc>,
        last_notified: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    Fired,
    Reminder,
    Resolved,
}

impl Transition {
    fn label(self) -> &'static str {
        match self {
            Self::Fired => "FIRING",
            Self::Reminder => "STILL FIRING",
            Self::Resolved => "RESOLVED",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Debouncer {
    phase: Phase,
}

impl Debouncer {
    fn new() -> Self {
        Self {
            phase: Phase::Clear,
        }
    }

    /// Advances one tick. `active = None` means the signal was unavailable:
    /// nothing changes, so an unknown signal neither fires nor resolves.
    /// Returns the onset time alongside any notification to send.
    fn step(
        &mut self,
        active: Option<bool>,
        now: DateTime<Utc>,
        timing: Timing,
    ) -> Option<(Transition, DateTime<Utc>)> {
        let active = active?;
        let reminder_due = |last_notified: DateTime<Utc>| {
            timing.repeat_every > Duration::zero() && now - last_notified >= timing.repeat_every
        };
        match (self.phase, active) {
            (Phase::Clear, false) => None,
            (Phase::Clear, true) => {
                self.phase = Phase::Pending { since: now };
                self.fire_if_due(now, timing)
            }
            (Phase::Pending { .. }, true) => self.fire_if_due(now, timing),
            (Phase::Pending { .. }, false) => {
                self.phase = Phase::Clear;
                None
            }
            // Still active, or flapped back while Recovering before
            // `recover_after` elapsed: (back to) Firing with no new FIRING
            // notification, only a reminder if one is due.
            (
                Phase::Firing {
                    since,
                    last_notified,
                }
                | Phase::Recovering {
                    since,
                    last_notified,
                    ..
                },
                true,
            ) => {
                let remind = reminder_due(last_notified);
                self.phase = Phase::Firing {
                    since,
                    last_notified: if remind { now } else { last_notified },
                };
                remind.then_some((Transition::Reminder, since))
            }
            (
                Phase::Firing {
                    since,
                    last_notified,
                },
                false,
            ) => {
                self.phase = Phase::Recovering {
                    since,
                    clear_since: now,
                    last_notified,
                };
                self.resolve_if_due(now, timing)
            }
            (
                Phase::Recovering {
                    since,
                    clear_since,
                    last_notified,
                },
                false,
            ) => {
                if let Some(event) = self.resolve_if_due(now, timing) {
                    return Some(event);
                }
                if reminder_due(last_notified) {
                    self.phase = Phase::Recovering {
                        since,
                        clear_since,
                        last_notified: now,
                    };
                    return Some((Transition::Reminder, since));
                }
                None
            }
        }
    }

    fn fire_if_due(
        &mut self,
        now: DateTime<Utc>,
        timing: Timing,
    ) -> Option<(Transition, DateTime<Utc>)> {
        let Phase::Pending { since } = self.phase else {
            return None;
        };
        if now - since < timing.fire_after {
            return None;
        }
        self.phase = Phase::Firing {
            since,
            last_notified: now,
        };
        Some((Transition::Fired, since))
    }

    fn resolve_if_due(
        &mut self,
        now: DateTime<Utc>,
        timing: Timing,
    ) -> Option<(Transition, DateTime<Utc>)> {
        let Phase::Recovering {
            since, clear_since, ..
        } = self.phase
        else {
            return None;
        };
        if now - clear_since < timing.recover_after {
            return None;
        }
        self.phase = Phase::Clear;
        Some((Transition::Resolved, since))
    }
}

// ---------------------------------------------------------------------------
// Observations and condition evaluation (pure)
// ---------------------------------------------------------------------------

/// One scheduled capture's result, newest first in `Observation`.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub at: DateTime<Utc>,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyncObservation {
    pub enabled: bool,
    pub paused: bool,
    pub queued_files: u64,
    pub queued_bytes: u64,
    pub transferred_files: u64,
    pub last_error: Option<String>,
}

/// Everything one evaluation tick needs, gathered by the actor (or built
/// directly by tests). `None` fields mean "signal unavailable this tick".
#[derive(Debug, Clone)]
pub struct Observation {
    pub now: DateTime<Utc>,
    pub scheduler_running: bool,
    pub next_capture_at: Option<DateTime<Utc>>,
    /// The committed schedule, replayed over a past window by
    /// `capture_stalled` to find shots that should have fired.
    pub schedule: ScheduleConfig,
    /// Most recent scheduled-capture outcomes, newest first.
    pub recent_outcomes: Vec<Outcome>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub sync: Option<SyncObservation>,
    /// `(total_bytes, available_bytes)` of the capture tmpfs.
    pub capture_disk: Option<(u64, u64)>,
    pub cpu_temp_celsius: Option<f32>,
    /// Raw `vcgencmd get_throttled` bit field.
    pub throttled: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub condition: Condition,
    pub transition: Transition,
    pub at: DateTime<Utc>,
    /// When the condition started (onset of Pending).
    pub since: DateTime<Utc>,
    pub detail: String,
}

impl Notification {
    fn title(&self, station_name: Option<&str>) -> String {
        let station = station_name
            .map(|name| format!(" {name}"))
            .unwrap_or_default();
        format!(
            "Optic{station}: {} {}",
            self.transition.label(),
            self.condition.title()
        )
    }

    fn message(&self) -> String {
        match self.transition {
            Transition::Resolved => format!(
                "{}\nWas active since {} ({}).",
                self.detail,
                format_time(self.since),
                format_duration(self.at - self.since)
            ),
            Transition::Fired | Transition::Reminder => format!(
                "{}\nActive since {} ({}).",
                self.detail,
                format_time(self.since),
                format_duration(self.at - self.since)
            ),
        }
    }
}

#[derive(Debug, Clone)]
struct Tracker {
    debouncer: Debouncer,
    /// Last known raw evaluation, for hysteresis. Unchanged by unknown ticks.
    raw_active: bool,
    detail: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct Backlog {
    since: DateTime<Utc>,
    transferred_files: u64,
}

/// Pure monitor state. Feed it one `Observation` per tick; it returns the
/// notifications to send.
pub struct Monitor {
    thresholds: Thresholds,
    started_at: DateTime<Utc>,
    /// When the scheduler was last seen switching to Running (or the
    /// monitor start if it was already running). Shots due before this are
    /// never blamed on the station.
    running_since: Option<DateTime<Utc>>,
    backlog: Option<Backlog>,
    trackers: Vec<Tracker>,
    last_evaluated_at: Option<DateTime<Utc>>,
}

impl Monitor {
    pub fn new(thresholds: Thresholds, started_at: DateTime<Utc>) -> Self {
        Self {
            thresholds,
            started_at,
            running_since: None,
            backlog: None,
            trackers: vec![
                Tracker {
                    debouncer: Debouncer::new(),
                    raw_active: false,
                    detail: None,
                };
                Condition::ALL.len()
            ],
            last_evaluated_at: None,
        }
    }

    pub fn observe(&mut self, obs: &Observation) -> Vec<Notification> {
        self.update_run_state(obs);
        self.update_backlog(obs);
        let mut notifications = Vec::new();
        for condition in Condition::ALL {
            let evaluation = self.evaluate(condition, obs);
            let timing = self.thresholds.timing(condition);
            let tracker = &mut self.trackers[condition.index()];
            let active = evaluation.as_ref().map(|(active, _)| *active);
            if let Some((active, detail)) = evaluation {
                tracker.raw_active = active;
                tracker.detail = Some(detail);
            }
            if let Some((transition, since)) = tracker.debouncer.step(active, obs.now, timing) {
                notifications.push(Notification {
                    condition,
                    transition,
                    at: obs.now,
                    since,
                    detail: tracker.detail.clone().unwrap_or_default(),
                });
            }
        }
        self.last_evaluated_at = Some(obs.now);
        notifications
    }

    fn update_run_state(&mut self, obs: &Observation) {
        self.running_since = match (self.running_since, obs.scheduler_running) {
            (_, false) => None,
            (Some(since), true) => Some(since),
            // First tick after startup: don't blame shots from before the
            // monitor existed. Later: a pause→resume transition.
            (None, true) if self.last_evaluated_at.is_none() => Some(self.started_at),
            (None, true) => Some(obs.now),
        };
    }

    fn update_backlog(&mut self, obs: &Observation) {
        let Some(sync) = &obs.sync else {
            return;
        };
        if !sync.enabled || sync.paused || sync.queued_files == 0 {
            self.backlog = None;
            return;
        }
        self.backlog = match self.backlog {
            Some(backlog) if backlog.transferred_files == sync.transferred_files => Some(backlog),
            // New backlog, or transfers made progress: restart the clock.
            _ => Some(Backlog {
                since: obs.now,
                transferred_files: sync.transferred_files,
            }),
        };
    }

    /// `None` = signal unavailable. Otherwise `(active, human detail)`.
    fn evaluate(&self, condition: Condition, obs: &Observation) -> Option<(bool, String)> {
        let was_active = self.trackers[condition.index()].raw_active;
        match condition {
            Condition::CaptureStalled => Some(self.capture_stalled(obs)),
            Condition::CaptureOverdue => Some(self.capture_overdue(obs)),
            Condition::CaptureFailing => Some(self.capture_failing(obs)),
            Condition::SyncBacklog => self.sync_backlog(obs),
            Condition::CaptureTmpfsLow => tmpfs_low(&self.thresholds, obs, was_active),
            Condition::TemperatureHigh => temperature_high(&self.thresholds, obs, was_active),
            Condition::Throttled => obs.throttled.map(throttled),
        }
    }

    fn capture_stalled(&self, obs: &Observation) -> (bool, String) {
        let Some(running_since) = self.running_since else {
            return (false, "Scheduler is paused.".to_owned());
        };
        let stall_after = secs(self.thresholds.capture_stalled_after_secs);
        let cutoff = obs.now - stall_after;
        let mut window_start = running_since.max(obs.now - STALL_LOOKBACK);
        if let Some(last_success) = obs.last_success_at {
            if last_success >= cutoff {
                return (
                    false,
                    format!(
                        "Last successful scheduled capture at {}.",
                        format_time(last_success)
                    ),
                );
            }
            window_start = window_start.max(last_success);
        }
        if window_start >= cutoff {
            return (
                false,
                "No scheduled shot has been due long enough to judge.".to_owned(),
            );
        }
        // +1s so a shot exactly at `cutoff` is included regardless of
        // whether the forecast horizon's end is inclusive.
        let missed = optic_scheduler::forecast(
            &obs.schedule,
            window_start,
            cutoff - window_start + Duration::seconds(1),
        )
        .into_iter()
        .map(|shot| shot.at.with_timezone(&Utc))
        .find(|at| *at > window_start && *at <= cutoff);
        match missed {
            Some(due) => {
                let last = obs
                    .last_success_at
                    .map_or_else(|| "none since monitoring started".to_owned(), format_time);
                (
                    true,
                    format!(
                        "A scheduled shot was due at {} and no scheduled capture has succeeded since (last success: {last}).",
                        format_time(due)
                    ),
                )
            }
            None => (false, "No missed scheduled shots.".to_owned()),
        }
    }

    fn capture_overdue(&self, obs: &Observation) -> (bool, String) {
        if self.running_since.is_none() {
            return (false, "Scheduler is paused.".to_owned());
        }
        let overdue_after = secs(self.thresholds.capture_overdue_after_secs);
        match obs.next_capture_at {
            Some(due) if obs.now - due > overdue_after => (
                true,
                format!(
                    "The scheduled capture due at {} has not completed ({} late); the scheduler or camera may be hung.",
                    format_time(due),
                    format_duration(obs.now - due)
                ),
            ),
            Some(due) => (false, format!("Next capture due at {}.", format_time(due))),
            None => (false, "No capture currently due.".to_owned()),
        }
    }

    fn capture_failing(&self, obs: &Observation) -> (bool, String) {
        let Some(running_since) = self.running_since else {
            return (false, "Scheduler is paused.".to_owned());
        };
        let needed = self.thresholds.capture_failures_in_a_row.max(1);
        // Only outcomes since the scheduler last started running count, so
        // a resume (or daemon restart) doesn't re-fire on stale failures.
        let failures: Vec<&Outcome> = obs
            .recent_outcomes
            .iter()
            .take_while(|outcome| outcome.at >= running_since && !outcome.success)
            .collect();
        let count = u32::try_from(failures.len()).unwrap_or(u32::MAX);
        if count >= needed {
            let last_error = failures
                .first()
                .and_then(|outcome| outcome.error.as_deref())
                .unwrap_or("unknown error");
            (
                true,
                format!("The last {count} scheduled captures failed. Last error: {last_error}"),
            )
        } else {
            (
                false,
                format!("{count} consecutive scheduled capture failure(s)."),
            )
        }
    }

    fn sync_backlog(&self, obs: &Observation) -> Option<(bool, String)> {
        let sync = obs.sync.as_ref()?;
        if !sync.enabled {
            return Some((false, "Sync is disabled.".to_owned()));
        }
        if sync.paused {
            return Some((false, "Sync is paused.".to_owned()));
        }
        let Some(backlog) = self.backlog else {
            return Some((false, "Sync queue is empty.".to_owned()));
        };
        let stuck_for = obs.now - backlog.since;
        let active = stuck_for >= secs(self.thresholds.sync_backlog_after_secs);
        let error = sync
            .last_error
            .as_deref()
            .map(|error| format!(" Last error: {error}"))
            .unwrap_or_default();
        Some((
            active,
            format!(
                "{} file(s) ({}) queued with no successful transfer for {}.{error}",
                sync.queued_files,
                format_bytes(sync.queued_bytes),
                format_duration(stuck_for)
            ),
        ))
    }
}

fn tmpfs_low(
    thresholds: &Thresholds,
    obs: &Observation,
    was_active: bool,
) -> Option<(bool, String)> {
    let (total, available) = obs.capture_disk?;
    if total == 0 {
        return None;
    }
    let free_percent = available as f64 * 100.0 / total as f64;
    let active = if was_active {
        free_percent <= thresholds.tmpfs_low_clear_above_percent
    } else {
        free_percent < thresholds.tmpfs_low_fire_below_percent
    };
    Some((
        active,
        format!(
            "Capture tmpfs has {} free of {} ({free_percent:.0}%).",
            format_bytes(available),
            format_bytes(total)
        ),
    ))
}

fn temperature_high(
    thresholds: &Thresholds,
    obs: &Observation,
    was_active: bool,
) -> Option<(bool, String)> {
    let celsius = obs.cpu_temp_celsius?;
    let active = if was_active {
        celsius >= thresholds.temperature_clear_below_celsius
    } else {
        celsius >= thresholds.temperature_fire_at_celsius
    };
    Some((active, format!("CPU temperature is {celsius:.1} °C.")))
}

/// Bits 0–3 of `vcgencmd get_throttled` are the *current* state; bits
/// 16–19 are "has occurred since boot" and are ignored here.
fn throttled(bits: u32) -> (bool, String) {
    const FLAGS: [(u32, &str); 4] = [
        (0x1, "under-voltage"),
        (0x2, "ARM frequency capped"),
        (0x4, "throttled"),
        (0x8, "soft temperature limit"),
    ];
    let current: Vec<&str> = FLAGS
        .iter()
        .filter(|(mask, _)| bits & mask != 0)
        .map(|(_, name)| *name)
        .collect();
    if current.is_empty() {
        (false, format!("Not throttled (get_throttled=0x{bits:x})."))
    } else {
        (
            true,
            format!(
                "Currently {} (get_throttled=0x{bits:x}).",
                current.join(", ")
            ),
        )
    }
}

/// Parses `vcgencmd get_throttled`'s `throttled=0x50005\n` output.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_throttled(text: &str) -> Option<u32> {
    let hex = text.trim().strip_prefix("throttled=0x")?;
    u32::from_str_radix(hex, 16).ok()
}

fn format_time(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

fn format_duration(duration: Duration) -> String {
    let total = duration.num_seconds().max(0);
    if total < 60 {
        format!("{total}s")
    } else if total < 3600 {
        format!("{}m", total / 60)
    } else {
        format!("{}h {}m", total / 3600, (total % 3600) / 60)
    }
}

fn format_bytes(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

// ---------------------------------------------------------------------------
// Scheduled-capture outcome fallback (when the history DB is unavailable)
// ---------------------------------------------------------------------------

/// Remembers each distinct `SchedulerStatus.last_capture` it sees. Only a
/// fallback: polling can miss an outcome that is immediately superseded,
/// which the capture history database does not.
#[derive(Debug, Default)]
struct OutcomeTracker {
    recent: VecDeque<Outcome>,
    last_success_at: Option<DateTime<Utc>>,
}

impl OutcomeTracker {
    fn record(&mut self, last: Option<&LastCapture>) {
        let Some(last) = last else {
            return;
        };
        if self
            .recent
            .front()
            .is_some_and(|newest| newest.at == last.at)
        {
            return;
        }
        if last.success {
            self.last_success_at = Some(last.at);
        }
        self.recent.push_front(Outcome {
            at: last.at,
            success: last.success,
            error: last.error.clone(),
        });
        self.recent.truncate(OUTCOME_HISTORY);
    }
}

// ---------------------------------------------------------------------------
// Outbox (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Outbox {
    queue: VecDeque<Notification>,
}

impl Outbox {
    /// Returns `false` if the notification was not queued: a reminder is
    /// redundant while another notification for that condition is waiting.
    fn push(&mut self, notification: Notification) -> bool {
        if notification.transition == Transition::Reminder
            && self
                .queue
                .iter()
                .any(|queued| queued.condition == notification.condition)
        {
            return false;
        }
        if self.queue.len() >= OUTBOX_CAPACITY
            && let Some(dropped) = self.queue.pop_front()
        {
            tracing::warn!(
                condition = ?dropped.condition,
                transition = ?dropped.transition,
                "alert outbox full; dropped the oldest undelivered notification"
            );
        }
        self.queue.push_back(notification);
        true
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NtfyConfig {
    #[serde(default = "default_ntfy_server")]
    pub server: String,
    pub topic: String,
    #[serde(default)]
    pub token: Option<String>,
}

fn default_ntfy_server() -> String {
    DEFAULT_NTFY_SERVER.to_owned()
}

/// The topic on a public server is effectively a password, so neither it
/// nor the token is ever printed.
impl std::fmt::Debug for NtfyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NtfyConfig")
            .field("server", &self.server)
            .field("topic", &"<redacted>")
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AlertsFileConfig {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    station_name: Option<String>,
    #[serde(default)]
    ntfy: Option<NtfyConfig>,
    #[serde(default)]
    thresholds: Thresholds,
}

fn default_true() -> bool {
    true
}

/// Result of loading `alerts.json` at startup. Always usable: any problem
/// falls back to a dry-run notifier with the reason recorded.
#[derive(Debug)]
pub struct AlertsSetup {
    notifier: Notifier,
    station_name: Option<String>,
    thresholds: Thresholds,
    config_error: Option<String>,
    config_warnings: Vec<String>,
}

/// `OPTIC_ALERTS_CONFIG` overrides the default
/// `~/.config/optic-daemon/alerts.json` (outside git and outside the
/// deploy-replaced `~/.local/bin`).
pub fn resolve_config_path() -> PathBuf {
    if let Ok(configured) = env::var("OPTIC_ALERTS_CONFIG") {
        return PathBuf::from(configured);
    }
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
    PathBuf::from(home).join(DEFAULT_CONFIG_RELATIVE_PATH)
}

/// Reads and validates the config file, logging the outcome. Never fails.
pub fn load_config(path: &Path) -> AlertsSetup {
    let setup = match std::fs::read_to_string(path) {
        Ok(content) => {
            let mut setup = parse_config(&content);
            if let Some(warning) = permission_warning(path) {
                setup.config_warnings.push(warning);
            }
            setup
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            dry_run_setup("alerts config file not found; notifications are logged only (dry run)")
        }
        Err(error) => dry_run_setup(&format!(
            "alerts config file unreadable ({}); notifications are logged only (dry run)",
            error.kind()
        )),
    };
    match (&setup.config_error, &setup.notifier) {
        (Some(error), _) => {
            tracing::warn!(path = %path.display(), %error, "health alerts running in dry-run mode");
        }
        (None, Notifier::Ntfy(sender)) => {
            tracing::info!(server = %sender.config.server, "health alerts will notify via ntfy");
        }
        (None, Notifier::DryRun) => {
            tracing::info!(path = %path.display(), "health alerts disabled in config; dry-run mode");
        }
    }
    for warning in &setup.config_warnings {
        tracing::warn!(path = %path.display(), %warning, "health alerts config warning");
    }
    setup
}

fn dry_run_setup(reason: &str) -> AlertsSetup {
    AlertsSetup {
        notifier: Notifier::DryRun,
        station_name: None,
        thresholds: Thresholds::default(),
        config_error: Some(reason.to_owned()),
        config_warnings: Vec::new(),
    }
}

fn parse_config(content: &str) -> AlertsSetup {
    let file: AlertsFileConfig = match serde_json::from_str(content) {
        Ok(file) => file,
        Err(error) => {
            // serde_json's message names the field/line, never echoes values
            // of other fields, so it is safe to surface.
            return dry_run_setup(&format!(
                "alerts config is invalid ({error}); notifications are logged only (dry run)"
            ));
        }
    };
    let mut warnings = file.thresholds.warnings();
    let base = |notifier, config_error| AlertsSetup {
        notifier,
        station_name: file.station_name.clone(),
        thresholds: file.thresholds.clone(),
        config_error,
        config_warnings: Vec::new(),
    };
    if !file.enabled {
        let mut setup = base(Notifier::DryRun, None);
        setup.config_warnings = warnings;
        return setup;
    }
    let Some(mut ntfy) = file.ntfy.clone() else {
        return base(
            Notifier::DryRun,
            Some(
                "alerts config has no \"ntfy\" section; notifications are logged only (dry run)"
                    .to_owned(),
            ),
        );
    };
    ntfy.server = ntfy.server.trim_end_matches('/').to_owned();
    if let Err(error) = validate_ntfy(&ntfy) {
        return base(
            Notifier::DryRun,
            Some(format!("{error}; notifications are logged only (dry run)")),
        );
    }
    if ntfy.server.starts_with("http://") && ntfy.token.is_some() {
        warnings
            .push("ntfy server uses plain http; the access token is sent unencrypted".to_owned());
    }
    let mut setup = base(
        Notifier::Ntfy(NtfySender {
            config: ntfy,
            curl: PathBuf::from("curl"),
        }),
        None,
    );
    setup.config_warnings = warnings;
    setup
}

fn validate_ntfy(ntfy: &NtfyConfig) -> Result<(), String> {
    let valid_topic = !ntfy.topic.is_empty()
        && ntfy.topic.len() <= 64
        && ntfy
            .topic
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid_topic {
        return Err("ntfy.topic must be 1-64 characters of A-Z, a-z, 0-9, '-' or '_'".to_owned());
    }
    let valid_server = (ntfy.server.starts_with("https://") || ntfy.server.starts_with("http://"))
        && ntfy
            .server
            .chars()
            .all(|c| c.is_ascii_graphic() && c != '"' && c != '\\');
    if !valid_server {
        return Err("ntfy.server must be an http(s):// URL".to_owned());
    }
    if let Some(token) = &ntfy.token
        && (token.is_empty() || !token.chars().all(|c| c.is_ascii_graphic()))
    {
        return Err("ntfy.token must be non-empty printable ASCII without spaces".to_owned());
    }
    Ok(())
}

#[cfg(unix)]
fn permission_warning(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(path).ok()?.permissions().mode();
    (mode & 0o077 != 0).then(|| {
        format!(
            "alerts config file is accessible by group/others (mode {:o}); chmod 600 it",
            mode & 0o777
        )
    })
}

#[cfg(not(unix))]
fn permission_warning(_path: &Path) -> Option<String> {
    None
}

// ---------------------------------------------------------------------------
// Notification delivery
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Notifier {
    /// Log only. Used when no channel is configured, and in local tests.
    DryRun,
    Ntfy(NtfySender),
}

impl Notifier {
    fn channel(&self) -> &'static str {
        match self {
            Self::DryRun => "dry_run",
            Self::Ntfy(_) => "ntfy",
        }
    }

    async fn deliver(
        &self,
        notification: &Notification,
        station_name: Option<&str>,
    ) -> Result<(), String> {
        let title = notification.title(station_name);
        match self {
            Self::DryRun => {
                tracing::warn!(
                    %title,
                    message = %notification.message(),
                    "health alert (dry run; no notification channel configured)"
                );
                Ok(())
            }
            Self::Ntfy(sender) => {
                sender
                    .send(&title, &notification.message(), notification.transition)
                    .await?;
                tracing::info!(%title, "health alert delivered via ntfy");
                Ok(())
            }
        }
    }
}

#[derive(Debug)]
struct NtfySender {
    config: NtfyConfig,
    /// The `curl` binary; overridable only by tests.
    curl: PathBuf,
}

impl NtfySender {
    async fn send(&self, title: &str, message: &str, transition: Transition) -> Result<(), String> {
        let config = curl_config(&self.config, title, message, transition);
        // `-q` ignores any ~/.curlrc; `--config -` reads URL, headers, and
        // body from stdin so the token never appears in argv.
        let mut child = Command::new(&self.curl)
            .args([
                "-q",
                "--silent",
                "--show-error",
                "--fail",
                "--proto",
                "=https,http",
                "--max-time",
                &CURL_TIMEOUT_SECS.to_string(),
                "--config",
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("failed to start curl: {error}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(config.as_bytes())
                .await
                .map_err(|error| format!("failed to write curl config: {error}"))?;
        }
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(CURL_TIMEOUT_SECS + 5),
            child.wait_with_output(),
        )
        .await
        .map_err(|_| "curl did not exit in time".to_owned())?
        .map_err(|error| format!("curl failed: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr: String = stderr.trim().chars().take(200).collect();
        Err(format!("curl exited with {}: {stderr}", output.status))
    }
}

/// Builds the curl config file (fed on stdin) for one ntfy JSON publish.
fn curl_config(ntfy: &NtfyConfig, title: &str, message: &str, transition: Transition) -> String {
    let (priority, tag) = match transition {
        Transition::Fired | Transition::Reminder => (4, "warning"),
        Transition::Resolved => (3, "white_check_mark"),
    };
    let body = serde_json::json!({
        "topic": ntfy.topic,
        "title": title,
        "message": message,
        "priority": priority,
        "tags": [tag],
    })
    .to_string();
    let mut config = format!("url = \"{}/\"\n", curl_escape(&ntfy.server));
    config.push_str("header = \"Content-Type: application/json\"\n");
    if let Some(token) = &ntfy.token {
        config.push_str(&format!(
            "header = \"Authorization: Bearer {}\"\n",
            curl_escape(token)
        ));
    }
    // `data-raw`, not `data`: never interpret a leading '@' as a file name.
    config.push_str(&format!("data-raw = \"{}\"\n", curl_escape(&body)));
    config
}

/// Escapes a value for a double-quoted curl config string.
fn curl_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(c),
        }
    }
    escaped
}

// ---------------------------------------------------------------------------
// Status snapshot (served read-only at GET /api/alerts)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ConditionStatus {
    pub condition: Condition,
    /// `clear`, `pending`, `firing`, or `recovering`.
    pub state: &'static str,
    pub since: Option<DateTime<Utc>>,
    pub last_notified_at: Option<DateTime<Utc>>,
    pub detail: Option<String>,
}

/// Never contains the ntfy topic, token, or config path.
#[derive(Debug, Clone, Serialize)]
pub struct AlertsStatus {
    pub channel: &'static str,
    pub config_error: Option<String>,
    pub config_warnings: Vec<String>,
    pub poll_interval_secs: u64,
    pub last_evaluated_at: Option<DateTime<Utc>>,
    /// Conditions currently Firing or Recovering (notified, not resolved).
    pub active_count: usize,
    pub conditions: Vec<ConditionStatus>,
    pub outbox_len: usize,
    pub last_delivered_at: Option<DateTime<Utc>>,
    pub last_delivery_error: Option<String>,
    pub thresholds: Thresholds,
}

impl Monitor {
    fn condition_statuses(&self) -> Vec<ConditionStatus> {
        Condition::ALL
            .iter()
            .map(|&condition| {
                let tracker = &self.trackers[condition.index()];
                let (state, since, last_notified_at) = match tracker.debouncer.phase {
                    Phase::Clear => ("clear", None, None),
                    Phase::Pending { since } => ("pending", Some(since), None),
                    Phase::Firing {
                        since,
                        last_notified,
                    } => ("firing", Some(since), Some(last_notified)),
                    Phase::Recovering {
                        since,
                        last_notified,
                        ..
                    } => ("recovering", Some(since), Some(last_notified)),
                };
                ConditionStatus {
                    condition,
                    state,
                    since,
                    last_notified_at,
                    detail: tracker.detail.clone(),
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Actor
// ---------------------------------------------------------------------------

/// The existing handles the monitor reads from. All reads are
/// non-mutating.
pub struct AlertSources {
    pub scheduler: SchedulerHandle,
    pub sync: DataSyncManager,
    pub capture_log: Option<CaptureLog>,
    pub system_status: SystemStatusReader,
    /// The committed config's tmpfs cache — the same file the scheduler
    /// reads on every wake.
    pub config_cache_path: PathBuf,
}

#[derive(Clone)]
pub struct AlertsHandle {
    status: watch::Receiver<AlertsStatus>,
}

impl AlertsHandle {
    pub fn spawn(setup: AlertsSetup, sources: AlertSources) -> Self {
        let monitor = Monitor::new(setup.thresholds.clone(), Utc::now());
        let (status_tx, status) = watch::channel(AlertsStatus {
            channel: setup.notifier.channel(),
            config_error: setup.config_error.clone(),
            config_warnings: setup.config_warnings.clone(),
            poll_interval_secs: POLL_INTERVAL.as_secs(),
            last_evaluated_at: None,
            active_count: 0,
            conditions: monitor.condition_statuses(),
            outbox_len: 0,
            last_delivered_at: None,
            last_delivery_error: None,
            thresholds: setup.thresholds.clone(),
        });
        tokio::spawn(run_actor(setup, sources, monitor, status_tx));
        Self { status }
    }

    pub fn status(&self) -> AlertsStatus {
        self.status.borrow().clone()
    }
}

async fn run_actor(
    setup: AlertsSetup,
    sources: AlertSources,
    mut monitor: Monitor,
    status_tx: watch::Sender<AlertsStatus>,
) {
    let mut tracker = OutcomeTracker::default();
    let mut outbox = Outbox::default();
    let mut delivery = DeliveryState::default();
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let observation = gather(&sources, &mut tracker, &monitor.thresholds).await;
        for notification in monitor.observe(&observation) {
            tracing::info!(
                condition = ?notification.condition,
                transition = ?notification.transition,
                detail = %notification.detail,
                "health alert state change"
            );
            outbox.push(notification);
        }
        flush(
            &mut outbox,
            &setup.notifier,
            setup.station_name.as_deref(),
            &mut delivery,
        )
        .await;
        status_tx.send_modify(|status| {
            status.last_evaluated_at = monitor.last_evaluated_at;
            status.conditions = monitor.condition_statuses();
            status.active_count = status
                .conditions
                .iter()
                .filter(|c| matches!(c.state, "firing" | "recovering"))
                .count();
            status.outbox_len = outbox.queue.len();
            status.last_delivered_at = delivery.last_delivered_at;
            status.last_delivery_error = delivery.last_error.clone();
        });
    }
}

#[derive(Debug, Default)]
struct DeliveryState {
    last_delivered_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
}

/// Delivers queued notifications in order, stopping at the first failure so
/// an outage costs one delivery attempt per tick.
async fn flush(
    outbox: &mut Outbox,
    notifier: &Notifier,
    station_name: Option<&str>,
    delivery: &mut DeliveryState,
) {
    while let Some(notification) = outbox.queue.front() {
        match notifier.deliver(notification, station_name).await {
            Ok(()) => {
                outbox.queue.pop_front();
                delivery.last_delivered_at = Some(Utc::now());
                delivery.last_error = None;
            }
            Err(error) => {
                tracing::warn!(%error, queued = outbox.queue.len(), "health alert delivery failed; will retry");
                delivery.last_error = Some(error);
                return;
            }
        }
    }
}

async fn gather(
    sources: &AlertSources,
    tracker: &mut OutcomeTracker,
    thresholds: &Thresholds,
) -> Observation {
    let now = Utc::now();
    let scheduler = sources.scheduler.status();
    tracker.record(scheduler.last_capture.as_ref());
    let (recent_outcomes, last_success_at) = match &sources.capture_log {
        Some(log) => {
            let limit = thresholds.capture_failures_in_a_row.clamp(1, 100);
            let recent = log
                .query(scheduled_filter(None, limit))
                .await
                .entries
                .into_iter()
                .map(|entry| Outcome {
                    at: unix_ms_to_utc(entry.completed_at_unix_ms),
                    success: entry.success,
                    error: entry.error,
                })
                .collect();
            let last_success = log
                .query(scheduled_filter(Some(true), 1))
                .await
                .entries
                .first()
                .map(|entry| unix_ms_to_utc(entry.completed_at_unix_ms));
            (recent, last_success)
        }
        None => (
            tracker.recent.iter().cloned().collect(),
            tracker.last_success_at,
        ),
    };
    let schedule = match durable_state::read_cached(&sources.config_cache_path).await {
        Ok(content) => {
            serde_json::from_str::<AppConfig>(&content)
                .unwrap_or_default()
                .schedule
        }
        Err(_) => ScheduleConfig::default(),
    };
    let sync = sources.sync.status();
    let system = sources.system_status.snapshot().await;
    Observation {
        now,
        scheduler_running: scheduler.run_state == ScheduleRunState::Running,
        next_capture_at: scheduler.next_capture_at,
        schedule,
        recent_outcomes,
        last_success_at,
        sync: Some(SyncObservation {
            enabled: sync.enabled,
            paused: sync.paused,
            queued_files: sync.queued_files,
            queued_bytes: sync.queued_bytes,
            transferred_files: sync.transferred_files,
            last_error: sync.last_error,
        }),
        capture_disk: system
            .disks
            .iter()
            .find(|disk| disk.label == "capture")
            .map(|disk| (disk.total_bytes, disk.available_bytes)),
        cpu_temp_celsius: system.cpu_temp_celsius,
        throttled: read_throttled().await,
    }
}

fn scheduled_filter(success: Option<bool>, limit: u32) -> CaptureQueryFilter {
    CaptureQueryFilter {
        source: Some("scheduler".to_owned()),
        success,
        limit,
        ..CaptureQueryFilter::default()
    }
}

fn unix_ms_to_utc(ms: u128) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(i64::try_from(ms).unwrap_or(i64::MAX))
        .single()
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

#[cfg(target_os = "linux")]
async fn read_throttled() -> Option<u32> {
    let output = Command::new("vcgencmd")
        .arg("get_throttled")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_throttled(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(not(target_os = "linux"))]
async fn read_throttled() -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optic_scheduler::{Constraints, Rule, Trigger};

    fn t(h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 20, h, m, s).unwrap()
    }

    fn min(n: i64) -> Duration {
        Duration::minutes(n)
    }

    fn timing(fire_after: i64, recover_after: i64, repeat_every: i64) -> Timing {
        Timing {
            fire_after: min(fire_after),
            recover_after: min(recover_after),
            repeat_every: min(repeat_every),
        }
    }

    fn obs(now: DateTime<Utc>) -> Observation {
        Observation {
            now,
            scheduler_running: false,
            next_capture_at: None,
            schedule: ScheduleConfig::default(),
            recent_outcomes: Vec::new(),
            last_success_at: None,
            sync: None,
            capture_disk: None,
            cpu_temp_celsius: None,
            throttled: None,
        }
    }

    fn every_five_minutes() -> ScheduleConfig {
        ScheduleConfig {
            station: None,
            rules: vec![Rule {
                id: "base".to_owned(),
                label: "Base".to_owned(),
                slug: "base".to_owned(),
                enabled: true,
                trigger: Trigger::Interval {
                    every_secs: 300,
                    align_to_wall_clock: true,
                },
                constraints: Constraints::default(),
            }],
        }
    }

    fn running(now: DateTime<Utc>) -> Observation {
        Observation {
            scheduler_running: true,
            schedule: every_five_minutes(),
            ..obs(now)
        }
    }

    fn state(monitor: &Monitor, condition: Condition) -> &'static str {
        monitor
            .condition_statuses()
            .into_iter()
            .find(|status| status.condition == condition)
            .unwrap()
            .state
    }

    fn transitions(notifications: &[Notification], condition: Condition) -> Vec<Transition> {
        notifications
            .iter()
            .filter(|n| n.condition == condition)
            .map(|n| n.transition)
            .collect()
    }

    // --- Debouncer ---------------------------------------------------------

    #[test]
    fn debouncer_with_no_fire_delay_fires_on_the_first_active_tick() {
        let mut d = Debouncer::new();
        assert_eq!(
            d.step(Some(true), t(0, 0, 0), timing(0, 5, 360)),
            Some((Transition::Fired, t(0, 0, 0)))
        );
    }

    #[test]
    fn debouncer_waits_for_fire_after_and_reports_the_onset_time() {
        let mut d = Debouncer::new();
        let timing = timing(5, 5, 360);
        assert_eq!(d.step(Some(true), t(0, 0, 0), timing), None);
        assert_eq!(d.step(Some(true), t(0, 4, 59), timing), None);
        assert_eq!(
            d.step(Some(true), t(0, 5, 0), timing),
            Some((Transition::Fired, t(0, 0, 0)))
        );
    }

    #[test]
    fn debouncer_pending_that_clears_goes_back_to_clear_silently() {
        let mut d = Debouncer::new();
        let timing = timing(5, 5, 360);
        d.step(Some(true), t(0, 0, 0), timing);
        assert_eq!(d.step(Some(false), t(0, 2, 0), timing), None);
        assert_eq!(d.phase, Phase::Clear);
        // The clock restarts: a later onset needs a full fire_after again.
        assert_eq!(d.step(Some(true), t(0, 3, 0), timing), None);
        assert_eq!(d.step(Some(true), t(0, 7, 0), timing), None);
        assert!(d.step(Some(true), t(0, 8, 0), timing).is_some());
    }

    #[test]
    fn debouncer_resolves_only_after_recover_after_of_continuous_clear() {
        let mut d = Debouncer::new();
        let timing = timing(0, 5, 360);
        d.step(Some(true), t(0, 0, 0), timing);
        assert_eq!(d.step(Some(false), t(0, 1, 0), timing), None);
        assert_eq!(d.step(Some(false), t(0, 5, 59), timing), None);
        assert_eq!(
            d.step(Some(false), t(0, 6, 0), timing),
            Some((Transition::Resolved, t(0, 0, 0)))
        );
        assert_eq!(d.phase, Phase::Clear);
    }

    #[test]
    fn debouncer_flapping_while_recovering_sends_nothing() {
        let mut d = Debouncer::new();
        let timing = timing(0, 5, 360);
        assert!(d.step(Some(true), t(0, 0, 0), timing).is_some());
        for minute in 1..20 {
            let active = minute % 2 == 0;
            assert_eq!(
                d.step(Some(active), t(0, minute, 0), timing),
                None,
                "minute {minute}"
            );
        }
        assert!(matches!(d.phase, Phase::Recovering { .. }));
    }

    #[test]
    fn debouncer_repeats_once_per_interval_while_firing_or_recovering() {
        let mut d = Debouncer::new();
        let timing = timing(0, 600, 60);
        d.step(Some(true), t(0, 0, 0), timing);
        assert_eq!(d.step(Some(true), t(0, 59, 0), timing), None);
        assert_eq!(
            d.step(Some(true), t(1, 0, 0), timing),
            Some((Transition::Reminder, t(0, 0, 0)))
        );
        assert_eq!(d.step(Some(true), t(1, 30, 0), timing), None);
        // Still reminded while Recovering (recover_after is long here).
        assert_eq!(d.step(Some(false), t(1, 45, 0), timing), None);
        assert_eq!(
            d.step(Some(false), t(2, 0, 0), timing),
            Some((Transition::Reminder, t(0, 0, 0)))
        );
        assert_eq!(d.step(Some(false), t(2, 1, 0), timing), None);
    }

    #[test]
    fn debouncer_zero_repeat_disables_reminders() {
        let mut d = Debouncer::new();
        let timing = timing(0, 5, 0);
        d.step(Some(true), t(0, 0, 0), timing);
        assert_eq!(d.step(Some(true), t(9, 0, 0), timing), None);
    }

    #[test]
    fn debouncer_ignores_unknown_ticks_in_every_phase() {
        let timing = timing(5, 5, 60);
        let mut d = Debouncer::new();
        assert_eq!(d.step(None, t(0, 0, 0), timing), None);
        assert_eq!(d.phase, Phase::Clear);
        d.step(Some(true), t(0, 0, 0), timing);
        let pending = d.phase;
        assert_eq!(d.step(None, t(0, 10, 0), timing), None);
        assert_eq!(d.phase, pending);
        d.step(Some(true), t(0, 10, 0), timing);
        let firing = d.phase;
        assert_eq!(
            d.step(None, t(3, 0, 0), timing),
            None,
            "no reminder on unknown"
        );
        assert_eq!(d.phase, firing);
        d.step(Some(false), t(3, 0, 0), timing);
        let recovering = d.phase;
        assert_eq!(
            d.step(None, t(3, 30, 0), timing),
            None,
            "no resolve on unknown"
        );
        assert_eq!(d.phase, recovering);
    }

    // --- capture_stalled ---------------------------------------------------

    #[test]
    fn capture_stalled_fires_when_a_due_shot_is_15_minutes_old_without_success() {
        // Monitor starts 00:00 with the scheduler running; shots every 5 min
        // at :05, :10, ... none succeed.
        let mut monitor = Monitor::new(Thresholds::default(), t(0, 0, 0));
        assert!(monitor.observe(&running(t(0, 0, 0))).is_empty());
        let before = monitor.observe(&running(t(0, 19, 59)));
        assert!(transitions(&before, Condition::CaptureStalled).is_empty());
        let fired = monitor.observe(&running(t(0, 20, 0)));
        assert_eq!(
            transitions(&fired, Condition::CaptureStalled),
            vec![Transition::Fired]
        );
        let detail = &fired[0].detail;
        assert!(detail.contains("2026-09-20 00:05:00 UTC"), "{detail}");
    }

    #[test]
    fn capture_stalled_counts_from_the_last_success_and_clears_on_a_new_one() {
        let mut monitor = Monitor::new(Thresholds::default(), t(0, 0, 0));
        let with_success = |now, success| Observation {
            last_success_at: Some(success),
            ..running(now)
        };
        monitor.observe(&with_success(t(0, 0, 0), t(0, 5, 5)));
        monitor.observe(&with_success(t(0, 20, 0), t(0, 5, 5)));
        assert_eq!(state(&monitor, Condition::CaptureStalled), "clear");
        monitor.observe(&with_success(t(0, 24, 59), t(0, 5, 5)));
        assert_eq!(state(&monitor, Condition::CaptureStalled), "clear");
        monitor.observe(&with_success(t(0, 25, 0), t(0, 5, 5)));
        assert_eq!(state(&monitor, Condition::CaptureStalled), "firing");
        monitor.observe(&with_success(t(0, 26, 0), t(0, 25, 30)));
        assert_eq!(state(&monitor, Condition::CaptureStalled), "recovering");
        let resolved = monitor.observe(&with_success(t(0, 31, 0), t(0, 30, 5)));
        assert_eq!(
            transitions(&resolved, Condition::CaptureStalled),
            vec![Transition::Resolved]
        );
    }

    #[test]
    fn capture_stalled_ignores_shots_before_a_resume_and_while_paused() {
        let mut monitor = Monitor::new(Thresholds::default(), t(0, 0, 0));
        monitor.observe(&Observation {
            scheduler_running: false,
            ..running(t(0, 0, 0))
        });
        monitor.observe(&Observation {
            scheduler_running: false,
            ..running(t(0, 59, 0))
        });
        assert_eq!(state(&monitor, Condition::CaptureStalled), "clear");
        // Resumed at 01:00: shots from 00:05..00:55 must not count.
        monitor.observe(&running(t(1, 0, 0)));
        monitor.observe(&running(t(1, 19, 59)));
        assert_eq!(state(&monitor, Condition::CaptureStalled), "clear");
        monitor.observe(&running(t(1, 20, 0)));
        assert_eq!(state(&monitor, Condition::CaptureStalled), "firing");
    }

    #[test]
    fn capture_stalled_needs_a_scheduled_shot() {
        let mut monitor = Monitor::new(Thresholds::default(), t(0, 0, 0));
        let no_rules = |now| Observation {
            schedule: ScheduleConfig::default(),
            ..running(now)
        };
        monitor.observe(&no_rules(t(0, 0, 0)));
        monitor.observe(&no_rules(t(3, 0, 0)));
        assert_eq!(state(&monitor, Condition::CaptureStalled), "clear");
    }

    // --- capture_overdue ---------------------------------------------------

    #[test]
    fn capture_overdue_fires_only_past_the_threshold_and_while_running() {
        let mut monitor = Monitor::new(Thresholds::default(), t(0, 0, 0));
        let due = |now, running_flag| Observation {
            scheduler_running: running_flag,
            next_capture_at: Some(t(0, 10, 0)),
            ..running(now)
        };
        monitor.observe(&due(t(0, 14, 0), true));
        assert_eq!(state(&monitor, Condition::CaptureOverdue), "clear");
        monitor.observe(&due(t(0, 15, 1), true));
        assert_eq!(state(&monitor, Condition::CaptureOverdue), "firing");

        let mut paused = Monitor::new(Thresholds::default(), t(0, 0, 0));
        paused.observe(&due(t(0, 30, 0), false));
        assert_eq!(state(&paused, Condition::CaptureOverdue), "clear");
    }

    // --- capture_failing ---------------------------------------------------

    fn outcome(at: DateTime<Utc>, success: bool) -> Outcome {
        Outcome {
            at,
            success,
            error: (!success).then(|| format!("camera error at {at}")),
        }
    }

    #[test]
    fn capture_failing_needs_three_consecutive_failures() {
        let mut monitor = Monitor::new(Thresholds::default(), t(0, 0, 0));
        let with = |now, outcomes: Vec<Outcome>| Observation {
            recent_outcomes: outcomes,
            ..running(now)
        };
        monitor.observe(&with(t(0, 0, 0), vec![]));
        monitor.observe(&with(
            t(0, 11, 0),
            vec![outcome(t(0, 10, 5), false), outcome(t(0, 5, 5), false)],
        ));
        assert_eq!(state(&monitor, Condition::CaptureFailing), "clear");
        let fired = monitor.observe(&with(
            t(0, 16, 0),
            vec![
                outcome(t(0, 15, 5), false),
                outcome(t(0, 10, 5), false),
                outcome(t(0, 5, 5), false),
            ],
        ));
        assert_eq!(
            transitions(&fired, Condition::CaptureFailing),
            vec![Transition::Fired]
        );
        let detail = &fired
            .iter()
            .find(|n| n.condition == Condition::CaptureFailing)
            .unwrap()
            .detail;
        assert!(
            detail.contains("camera error at 2026-09-20 00:15:05"),
            "{detail}"
        );
        monitor.observe(&with(
            t(0, 21, 0),
            vec![outcome(t(0, 20, 5), true), outcome(t(0, 15, 5), false)],
        ));
        assert_eq!(state(&monitor, Condition::CaptureFailing), "recovering");
    }

    #[test]
    fn capture_failing_ignores_failures_from_before_the_scheduler_started_running() {
        let mut monitor = Monitor::new(Thresholds::default(), t(1, 0, 0));
        let stale = vec![
            outcome(t(0, 15, 5), false),
            outcome(t(0, 10, 5), false),
            outcome(t(0, 5, 5), false),
        ];
        monitor.observe(&Observation {
            recent_outcomes: stale,
            ..running(t(1, 0, 0))
        });
        assert_eq!(state(&monitor, Condition::CaptureFailing), "clear");
    }

    // --- sync_backlog ------------------------------------------------------

    fn sync(queued: u64, transferred: u64) -> SyncObservation {
        SyncObservation {
            enabled: true,
            paused: false,
            queued_files: queued,
            queued_bytes: queued * 1024 * 1024,
            transferred_files: transferred,
            last_error: Some("Connection refused".to_owned()),
        }
    }

    fn with_sync(now: DateTime<Utc>, sync: SyncObservation) -> Observation {
        Observation {
            sync: Some(sync),
            ..obs(now)
        }
    }

    #[test]
    fn sync_backlog_fires_after_30_minutes_without_progress() {
        let mut monitor = Monitor::new(Thresholds::default(), t(0, 0, 0));
        monitor.observe(&with_sync(t(0, 0, 0), sync(0, 10)));
        monitor.observe(&with_sync(t(0, 1, 0), sync(3, 10)));
        monitor.observe(&with_sync(t(0, 30, 59), sync(8, 10)));
        assert_eq!(state(&monitor, Condition::SyncBacklog), "clear");
        let fired = monitor.observe(&with_sync(t(0, 31, 0), sync(9, 10)));
        assert_eq!(
            transitions(&fired, Condition::SyncBacklog),
            vec![Transition::Fired]
        );
        assert!(fired[0].detail.contains("Connection refused"));
    }

    #[test]
    fn sync_backlog_clock_restarts_on_transfer_progress_and_clears_on_empty_queue() {
        let mut monitor = Monitor::new(Thresholds::default(), t(0, 0, 0));
        monitor.observe(&with_sync(t(0, 0, 0), sync(3, 10)));
        monitor.observe(&with_sync(t(0, 20, 0), sync(3, 11)));
        monitor.observe(&with_sync(t(0, 45, 0), sync(3, 11)));
        assert_eq!(state(&monitor, Condition::SyncBacklog), "clear");
        monitor.observe(&with_sync(t(0, 50, 0), sync(3, 11)));
        assert_eq!(state(&monitor, Condition::SyncBacklog), "firing");
        monitor.observe(&with_sync(t(0, 51, 0), sync(0, 14)));
        assert_eq!(state(&monitor, Condition::SyncBacklog), "recovering");
    }

    #[test]
    fn sync_backlog_is_suppressed_when_disabled_or_paused() {
        for (enabled, paused) in [(false, false), (true, true)] {
            let mut monitor = Monitor::new(Thresholds::default(), t(0, 0, 0));
            let s = SyncObservation {
                enabled,
                paused,
                ..sync(5, 0)
            };
            monitor.observe(&with_sync(t(0, 0, 0), s.clone()));
            monitor.observe(&with_sync(t(2, 0, 0), s));
            assert_eq!(state(&monitor, Condition::SyncBacklog), "clear");
        }
    }

    // --- capture_tmpfs_low -------------------------------------------------

    const MIB: u64 = 1024 * 1024;

    fn with_disk(now: DateTime<Utc>, free_percent: u64) -> Observation {
        Observation {
            capture_disk: Some((100 * MIB, free_percent * MIB)),
            ..obs(now)
        }
    }

    #[test]
    fn tmpfs_low_uses_hysteresis() {
        let mut monitor = Monitor::new(Thresholds::default(), t(0, 0, 0));
        monitor.observe(&with_disk(t(0, 0, 0), 30));
        assert_eq!(state(&monitor, Condition::CaptureTmpfsLow), "clear");
        monitor.observe(&with_disk(t(0, 1, 0), 24));
        assert_eq!(state(&monitor, Condition::CaptureTmpfsLow), "firing");
        // 30% is above the fire threshold but still active until > 35%.
        monitor.observe(&with_disk(t(0, 2, 0), 30));
        assert_eq!(state(&monitor, Condition::CaptureTmpfsLow), "firing");
        monitor.observe(&with_disk(t(0, 3, 0), 36));
        assert_eq!(state(&monitor, Condition::CaptureTmpfsLow), "recovering");
        let resolved = monitor.observe(&with_disk(t(0, 8, 0), 60));
        assert_eq!(
            transitions(&resolved, Condition::CaptureTmpfsLow),
            vec![Transition::Resolved]
        );
    }

    #[test]
    fn tmpfs_low_is_unknown_without_disk_data() {
        let mut monitor = Monitor::new(Thresholds::default(), t(0, 0, 0));
        monitor.observe(&with_disk(t(0, 0, 0), 10));
        assert_eq!(state(&monitor, Condition::CaptureTmpfsLow), "firing");
        monitor.observe(&obs(t(1, 0, 0)));
        assert_eq!(state(&monitor, Condition::CaptureTmpfsLow), "firing");
        monitor.observe(&Observation {
            capture_disk: Some((0, 0)),
            ..obs(t(1, 1, 0))
        });
        assert_eq!(state(&monitor, Condition::CaptureTmpfsLow), "firing");
    }

    // --- temperature_high / throttled --------------------------------------

    fn with_temp(now: DateTime<Utc>, celsius: f32) -> Observation {
        Observation {
            cpu_temp_celsius: Some(celsius),
            ..obs(now)
        }
    }

    #[test]
    fn temperature_high_is_sustained_and_uses_hysteresis() {
        let mut monitor = Monitor::new(Thresholds::default(), t(0, 0, 0));
        monitor.observe(&with_temp(t(0, 0, 0), 81.0));
        assert_eq!(state(&monitor, Condition::TemperatureHigh), "pending");
        monitor.observe(&with_temp(t(0, 4, 0), 74.0));
        assert_eq!(state(&monitor, Condition::TemperatureHigh), "clear");
        monitor.observe(&with_temp(t(0, 5, 0), 80.0));
        monitor.observe(&with_temp(t(0, 7, 0), 77.0));
        assert_eq!(state(&monitor, Condition::TemperatureHigh), "pending");
        monitor.observe(&with_temp(t(0, 10, 0), 76.0));
        assert_eq!(state(&monitor, Condition::TemperatureHigh), "firing");
        monitor.observe(&with_temp(t(0, 11, 0), 74.9));
        assert_eq!(state(&monitor, Condition::TemperatureHigh), "recovering");
        monitor.observe(&obs(t(0, 20, 0)));
        assert_eq!(
            state(&monitor, Condition::TemperatureHigh),
            "recovering",
            "unknown temperature must not resolve"
        );
    }

    #[test]
    fn parse_throttled_reads_vcgencmd_output() {
        assert_eq!(parse_throttled("throttled=0x0\n"), Some(0));
        assert_eq!(parse_throttled("throttled=0x50005"), Some(0x50005));
        assert_eq!(parse_throttled("garbage"), None);
        assert_eq!(parse_throttled("throttled=0xZZ"), None);
    }

    #[test]
    fn throttled_only_counts_current_bits() {
        assert!(!throttled(0x0).0);
        assert!(!throttled(0x50000).0, "historical bits only");
        let (active, detail) = throttled(0x50005);
        assert!(active);
        assert!(detail.contains("under-voltage") && detail.contains("throttled"));
        assert!(throttled(0x8).0);
    }

    #[test]
    fn throttled_condition_fires_after_the_sustained_window() {
        let mut monitor = Monitor::new(Thresholds::default(), t(0, 0, 0));
        let with = |now, bits| Observation {
            throttled: Some(bits),
            ..obs(now)
        };
        monitor.observe(&with(t(0, 0, 0), 0x1));
        monitor.observe(&with(t(0, 4, 59), 0x1));
        assert_eq!(state(&monitor, Condition::Throttled), "pending");
        monitor.observe(&with(t(0, 5, 0), 0x1));
        assert_eq!(state(&monitor, Condition::Throttled), "firing");
    }

    // --- outcome tracker ---------------------------------------------------

    #[test]
    fn outcome_tracker_records_each_capture_once_and_is_bounded() {
        let mut tracker = OutcomeTracker::default();
        let capture = |minute, success| LastCapture {
            at: t(0, minute, 0),
            rule_slugs: vec!["base".to_owned()],
            success,
            error: None,
        };
        tracker.record(None);
        tracker.record(Some(&capture(1, true)));
        tracker.record(Some(&capture(1, true)));
        tracker.record(Some(&capture(2, false)));
        assert_eq!(tracker.recent.len(), 2);
        assert!(!tracker.recent[0].success);
        assert_eq!(tracker.last_success_at, Some(t(0, 1, 0)));
        for minute in 3..40 {
            tracker.record(Some(&capture(minute, false)));
        }
        assert_eq!(tracker.recent.len(), OUTCOME_HISTORY);
        assert_eq!(tracker.recent[0].at, t(0, 39, 0));
    }

    // --- config ------------------------------------------------------------

    #[test]
    fn full_config_parses_with_overrides() {
        let setup = parse_config(
            r#"{
                "enabled": true,
                "station_name": "rooftop",
                "ntfy": {"server": "https://ntfy.example.com/", "topic": "optic-abc_123", "token": "tk_secret"},
                "thresholds": {"sync_backlog_after_secs": 600, "temperature_fire_at_celsius": 82.5}
            }"#,
        );
        assert_eq!(setup.config_error, None);
        assert_eq!(setup.station_name.as_deref(), Some("rooftop"));
        assert_eq!(setup.thresholds.sync_backlog_after_secs, 600);
        assert_eq!(setup.thresholds.temperature_fire_at_celsius, 82.5);
        assert_eq!(setup.thresholds.capture_failures_in_a_row, 3);
        let Notifier::Ntfy(sender) = &setup.notifier else {
            panic!("expected ntfy notifier");
        };
        assert_eq!(sender.config.server, "https://ntfy.example.com");
    }

    #[test]
    fn minimal_config_uses_default_server_and_thresholds() {
        let setup = parse_config(r#"{"ntfy": {"topic": "x"}}"#);
        assert_eq!(setup.config_error, None);
        assert_eq!(setup.thresholds, Thresholds::default());
        let Notifier::Ntfy(sender) = &setup.notifier else {
            panic!("expected ntfy notifier");
        };
        assert_eq!(sender.config.server, DEFAULT_NTFY_SERVER);
    }

    #[test]
    fn invalid_configs_fall_back_to_dry_run_with_a_reason() {
        for (content, needle) in [
            (
                r#"{"ntfy": {"topic": "x"}, "thresholds": {"sync_backlog_secs": 1}}"#,
                "unknown field",
            ),
            (r#"{"ntfy": {"topic": "x", "tokn": "y"}}"#, "unknown field"),
            (r#"{}"#, "no \"ntfy\" section"),
            (r#"{"ntfy": {"topic": "bad topic"}}"#, "ntfy.topic"),
            (
                r#"{"ntfy": {"topic": "x", "server": "ftp://h"}}"#,
                "ntfy.server",
            ),
            (r#"{"ntfy": {"topic": "x", "token": "a b"}}"#, "ntfy.token"),
            ("not json", "invalid"),
        ] {
            let setup = parse_config(content);
            assert_eq!(setup.notifier.channel(), "dry_run", "{content}");
            let error = setup.config_error.unwrap_or_default();
            assert!(error.contains(needle), "{content}: {error}");
        }
    }

    #[test]
    fn disabled_config_is_dry_run_without_an_error() {
        let setup = parse_config(r#"{"enabled": false}"#);
        assert_eq!(setup.notifier.channel(), "dry_run");
        assert_eq!(setup.config_error, None);
    }

    #[test]
    fn inverted_hysteresis_and_plain_http_token_produce_warnings() {
        let setup = parse_config(
            r#"{"ntfy": {"server": "http://imac.local", "topic": "x", "token": "tk"},
                "thresholds": {"tmpfs_low_clear_above_percent": 10}}"#,
        );
        assert_eq!(setup.config_error, None);
        assert_eq!(
            setup.config_warnings.len(),
            2,
            "{:?}",
            setup.config_warnings
        );
    }

    #[test]
    fn absurd_threshold_durations_are_clamped_instead_of_panicking() {
        let setup = parse_config(
            r#"{"ntfy": {"topic": "x"}, "thresholds": {
                "capture_stalled_after_secs": 18446744073709551615,
                "capture_overdue_after_secs": 18446744073709551615,
                "sync_backlog_after_secs": 18446744073709551615,
                "repeat_every_secs": 18446744073709551615}}"#,
        );
        assert_eq!(setup.config_error, None);
        assert!(setup.config_warnings.iter().any(|w| w.contains("clamped")));
        let mut monitor = Monitor::new(setup.thresholds, t(0, 0, 0));
        let busy = |now| Observation {
            next_capture_at: Some(t(0, 0, 0)),
            sync: Some(sync(5, 0)),
            ..running(now)
        };
        monitor.observe(&busy(t(0, 0, 0)));
        assert!(monitor.observe(&busy(t(23, 0, 0))).is_empty());
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("optic-alerts-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_config_reports_missing_file_and_permissive_mode() {
        let dir = temp_dir("load");
        let missing = load_config(&dir.join("absent.json"));
        assert_eq!(missing.notifier.channel(), "dry_run");
        assert!(missing.config_error.unwrap().contains("not found"));

        let path = dir.join("alerts.json");
        std::fs::write(&path, r#"{"ntfy": {"topic": "x"}}"#).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let loose = load_config(&path);
            assert_eq!(loose.notifier.channel(), "ntfy");
            assert!(
                loose
                    .config_warnings
                    .iter()
                    .any(|w| w.contains("chmod 600"))
            );
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert!(load_config(&path).config_warnings.is_empty());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn debug_output_redacts_topic_and_token() {
        let setup = parse_config(r#"{"ntfy": {"topic": "secret-topic", "token": "tk_secret"}}"#);
        let debug = format!("{setup:?}");
        assert!(
            !debug.contains("secret-topic") && !debug.contains("tk_secret"),
            "{debug}"
        );
    }

    // --- ntfy request ------------------------------------------------------

    fn ntfy(token: Option<&str>) -> NtfyConfig {
        NtfyConfig {
            server: "https://ntfy.sh".to_owned(),
            topic: "optic-test".to_owned(),
            token: token.map(str::to_owned),
        }
    }

    #[test]
    fn curl_config_carries_url_body_and_bearer_token() {
        let config = curl_config(
            &ntfy(Some("tk_secret")),
            "Optic: FIRING sync backlog",
            "3 file(s) \"queued\"\nback\\slash",
            Transition::Fired,
        );
        let lines: Vec<&str> = config.lines().collect();
        assert_eq!(lines[0], "url = \"https://ntfy.sh/\"");
        assert!(lines.contains(&"header = \"Authorization: Bearer tk_secret\""));
        let data = lines
            .iter()
            .find_map(|line| line.strip_prefix("data-raw = \""))
            .unwrap()
            .strip_suffix('"')
            .unwrap();
        // Undo curl-config escaping, then parse the JSON body.
        let unescaped = data
            .replace("\\\\", "\u{0}")
            .replace("\\\"", "\"")
            .replace('\u{0}', "\\");
        let body: serde_json::Value = serde_json::from_str(&unescaped).unwrap();
        assert_eq!(body["topic"], "optic-test");
        assert_eq!(body["title"], "Optic: FIRING sync backlog");
        assert_eq!(body["message"], "3 file(s) \"queued\"\nback\\slash");
        assert_eq!(body["priority"], 4);
        assert_eq!(body["tags"][0], "warning");
        assert!(!config.contains('\r'));
        assert_eq!(
            config.lines().count(),
            4,
            "no raw newline leaks into a value"
        );
    }

    #[test]
    fn curl_config_omits_auth_without_a_token_and_marks_resolved_lower_priority() {
        let config = curl_config(&ntfy(None), "t", "m", Transition::Resolved);
        assert!(!config.contains("Authorization"));
        assert!(config.contains("\\\"priority\\\":3"));
        assert!(config.contains("white_check_mark"));
    }

    #[test]
    fn notification_title_and_message_are_readable() {
        let n = Notification {
            condition: Condition::SyncBacklog,
            transition: Transition::Resolved,
            at: t(1, 5, 0),
            since: t(0, 0, 0),
            detail: "Sync queue is empty.".to_owned(),
        };
        assert_eq!(
            n.title(Some("rooftop")),
            "Optic rooftop: RESOLVED sync backlog"
        );
        assert_eq!(n.title(None), "Optic: RESOLVED sync backlog");
        assert_eq!(
            n.message(),
            "Sync queue is empty.\nWas active since 2026-09-20 00:00:00 UTC (1h 5m)."
        );
    }

    // --- outbox and delivery -----------------------------------------------

    fn note(condition: Condition, transition: Transition) -> Notification {
        Notification {
            condition,
            transition,
            at: t(0, 0, 0),
            since: t(0, 0, 0),
            detail: String::new(),
        }
    }

    #[test]
    fn outbox_is_bounded_and_skips_redundant_reminders() {
        let mut outbox = Outbox::default();
        assert!(outbox.push(note(Condition::SyncBacklog, Transition::Fired)));
        assert!(!outbox.push(note(Condition::SyncBacklog, Transition::Reminder)));
        assert!(outbox.push(note(Condition::SyncBacklog, Transition::Resolved)));
        assert!(outbox.push(note(Condition::Throttled, Transition::Reminder)));
        for _ in 0..OUTBOX_CAPACITY {
            outbox.push(note(Condition::TemperatureHigh, Transition::Fired));
        }
        assert_eq!(outbox.queue.len(), OUTBOX_CAPACITY);
        assert!(
            outbox
                .queue
                .iter()
                .all(|n| n.condition == Condition::TemperatureHigh)
        );
    }

    #[tokio::test]
    async fn dry_run_flush_delivers_everything() {
        let mut outbox = Outbox::default();
        outbox.push(note(Condition::SyncBacklog, Transition::Fired));
        outbox.push(note(Condition::Throttled, Transition::Fired));
        let mut delivery = DeliveryState::default();
        flush(&mut outbox, &Notifier::DryRun, None, &mut delivery).await;
        assert!(outbox.queue.is_empty());
        assert!(delivery.last_delivered_at.is_some());
    }

    #[cfg(unix)]
    fn fake_curl(dir: &Path, exit_code: i32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let script = dir.join(format!("fake-curl-{exit_code}"));
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat >> '{dir}/stdin.txt'\nprintf '%s\\n' \"$@\" >> '{dir}/args.txt'\necho 'The requested URL returned error: 403' >&2\nexit {exit_code}\n",
                dir = dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ntfy_delivery_passes_the_token_on_stdin_only_and_retries_in_order_after_failure() {
        let dir = temp_dir("curl");
        let failing = Notifier::Ntfy(NtfySender {
            config: ntfy(Some("tk_secret")),
            curl: fake_curl(&dir, 22),
        });
        let mut outbox = Outbox::default();
        outbox.push(note(Condition::SyncBacklog, Transition::Fired));
        outbox.push(note(Condition::SyncBacklog, Transition::Resolved));
        let mut delivery = DeliveryState::default();

        flush(&mut outbox, &failing, Some("rooftop"), &mut delivery).await;
        assert_eq!(outbox.queue.len(), 2, "nothing lost on failure");
        let error = delivery.last_error.clone().unwrap();
        assert!(error.contains("403"), "{error}");
        assert!(!error.contains("tk_secret"));
        let attempts = std::fs::read_to_string(dir.join("stdin.txt")).unwrap();
        assert_eq!(
            attempts.matches("url = ").count(),
            1,
            "stops at first failure"
        );

        let working = Notifier::Ntfy(NtfySender {
            config: ntfy(Some("tk_secret")),
            curl: fake_curl(&dir, 0),
        });
        flush(&mut outbox, &working, Some("rooftop"), &mut delivery).await;
        assert!(outbox.queue.is_empty());
        assert_eq!(delivery.last_error, None);

        let stdin = std::fs::read_to_string(dir.join("stdin.txt")).unwrap();
        let fired = stdin.find("FIRING sync backlog").unwrap();
        let resolved = stdin.rfind("RESOLVED sync backlog").unwrap();
        assert!(fired < resolved, "delivered in order");
        assert!(stdin.contains("Authorization: Bearer tk_secret"));
        let args = std::fs::read_to_string(dir.join("args.txt")).unwrap();
        assert!(
            !args.contains("tk_secret") && !args.contains("optic-test"),
            "{args}"
        );
        assert!(args.lines().any(|arg| arg == "--config"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn status_snapshot_never_contains_the_topic_or_token() {
        let setup = parse_config(r#"{"ntfy": {"topic": "secret-topic", "token": "tk_secret"}}"#);
        let monitor = Monitor::new(setup.thresholds.clone(), t(0, 0, 0));
        let status = AlertsStatus {
            channel: setup.notifier.channel(),
            config_error: setup.config_error.clone(),
            config_warnings: setup.config_warnings.clone(),
            poll_interval_secs: POLL_INTERVAL.as_secs(),
            last_evaluated_at: None,
            active_count: 0,
            conditions: monitor.condition_statuses(),
            outbox_len: 0,
            last_delivered_at: None,
            last_delivery_error: None,
            thresholds: setup.thresholds,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"channel\":\"ntfy\""));
        assert!(json.contains("\"condition\":\"capture_stalled\""));
        assert!(!json.contains("secret-topic") && !json.contains("tk_secret"));
    }
}
