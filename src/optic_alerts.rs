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
    sync::{Arc, LazyLock},
};

use chrono::{DateTime, Duration, NaiveTime, TimeZone as _, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::AsyncWriteExt as _,
    process::Command,
    sync::{mpsc, oneshot, watch},
};

use crate::{
    durable_state,
    optic_capture_log::{CaptureLog, CaptureQueryFilter},
    optic_digest::{
        self, CaptureRow, Decision, Digest, DigestInput, DigestState, EventRow, HeartbeatSnapshot,
        Sample, SyncSnapshot,
    },
    optic_events::SystemEventLog,
    optic_heartbeat::{self, ArmedOn, HeartbeatPlan},
    optic_scheduler::{self, LastCapture, ScheduleConfig, ScheduleRunState, SchedulerHandle},
    optic_sync::{DataSyncManager, SyncStatus},
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

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NtfyConfig {
    #[serde(default = "default_ntfy_server")]
    pub server: String,
    pub topic: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

/// Daily digest settings (`docs/optic-daemon-digest-heartbeat.md` §5.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DigestSettings {
    pub enabled: bool,
    /// `HH:MM`, 24 h, in the Station timezone.
    pub send_at: String,
}

impl Default for DigestSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            send_at: "08:00".to_owned(),
        }
    }
}

/// External heartbeat settings. The section is opt-in: absent means off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeartbeatSettings {
    pub enabled: bool,
    pub interval_secs: u64,
    pub alert_after_secs: u64,
    pub sequence_id: String,
}

impl Default for HeartbeatSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: optic_heartbeat::DEFAULT_INTERVAL_SECS,
            alert_after_secs: optic_heartbeat::DEFAULT_ALERT_AFTER_SECS,
            sequence_id: optic_heartbeat::DEFAULT_SEQUENCE_ID.to_owned(),
        }
    }
}

/// The `notifications` section of `config.json`, and the shape of the
/// legacy `alerts.json` (imported once). Unknown fields are rejected so a
/// misspelt setting is reported rather than ignored. `Debug` is safe: the
/// ntfy topic and token are redacted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub station_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ntfy: Option<NtfyConfig>,
    #[serde(default)]
    pub digest: DigestSettings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<HeartbeatSettings>,
    #[serde(default)]
    pub thresholds: Thresholds,
}

fn default_true() -> bool {
    true
}

/// Settings resolved into something runnable. Always usable: any problem
/// falls back to a dry-run notifier with the reason recorded.
#[derive(Debug, Clone)]
pub struct AlertsSetup {
    notifier: Notifier,
    station_name: Option<String>,
    thresholds: Thresholds,
    digest_send_at: Option<NaiveTime>,
    heartbeat: Option<HeartbeatPlan>,
    config_error: Option<String>,
    config_warnings: Vec<String>,
    /// The parsed settings (with secrets); `None` when missing or invalid.
    settings: Option<NotificationSettings>,
}

/// `OPTIC_ALERTS_CONFIG` overrides the default
/// `~/.config/optic-daemon/alerts.json`, the legacy file that is imported
/// into `config.json` once.
pub fn resolve_config_path() -> PathBuf {
    if let Ok(configured) = env::var("OPTIC_ALERTS_CONFIG") {
        return PathBuf::from(configured);
    }
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
    PathBuf::from(home).join(DEFAULT_CONFIG_RELATIVE_PATH)
}

/// Reads the legacy `alerts.json`. Never fails; a missing file is normal
/// once it has been imported.
pub fn load_config(path: &Path) -> AlertsSetup {
    let setup = match std::fs::read_to_string(path) {
        Ok(content) => {
            let mut setup = parse_config(&content);
            if let Some(warning) = permission_warning(path) {
                setup.config_warnings.push(warning);
            }
            setup
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => dry_run_setup(
            "no notification settings (alerts config file not found); notifications are logged only (dry run)",
        ),
        Err(error) => dry_run_setup(&format!(
            "alerts config file unreadable ({}); notifications are logged only (dry run)",
            error.kind()
        )),
    };
    if setup.settings.is_none()
        && let Some(error) = &setup.config_error
        && !error.contains("not found")
    {
        tracing::warn!(path = %path.display(), %error, "legacy alerts config not usable");
    }
    setup
}

fn dry_run_setup(reason: &str) -> AlertsSetup {
    AlertsSetup {
        notifier: Notifier::DryRun,
        station_name: None,
        thresholds: Thresholds::default(),
        digest_send_at: None,
        heartbeat: None,
        config_error: Some(reason.to_owned()),
        config_warnings: Vec::new(),
        settings: None,
    }
}

fn parse_config(content: &str) -> AlertsSetup {
    match serde_json::from_str::<NotificationSettings>(content) {
        Ok(settings) => resolve(&settings),
        // serde_json's message names the field/line, never echoes values
        // of other fields, so it is safe to surface.
        Err(error) => dry_run_setup(&format!(
            "notification settings are invalid ({error}); notifications are logged only (dry run)"
        )),
    }
}

/// Lenient resolution for settings read from disk: out-of-range values are
/// clamped or switched off with a warning rather than rejected.
fn resolve(settings: &NotificationSettings) -> AlertsSetup {
    let mut warnings = settings.thresholds.warnings();
    let digest_send_at = if settings.digest.enabled {
        let parsed = optic_digest::parse_send_at(&settings.digest.send_at);
        if parsed.is_none() {
            warnings.push("digest.send_at must be HH:MM (24 h); the digest is off".to_owned());
        }
        parsed
    } else {
        None
    };
    let heartbeat = settings
        .heartbeat
        .as_ref()
        .filter(|heartbeat| heartbeat.enabled)
        .and_then(|heartbeat| match heartbeat_plan(heartbeat, &mut warnings) {
            Ok(plan) => Some(plan),
            Err(error) => {
                warnings.push(format!("{error}; the heartbeat is off"));
                None
            }
        });
    let base = |notifier, config_error| AlertsSetup {
        notifier,
        station_name: settings.station_name.clone(),
        thresholds: settings.thresholds.clone(),
        digest_send_at,
        heartbeat: heartbeat.clone(),
        config_error,
        config_warnings: Vec::new(),
        settings: Some(settings.clone()),
    };
    if !settings.enabled {
        let mut setup = base(Notifier::DryRun, None);
        setup.config_warnings = warnings;
        return setup;
    }
    let Some(mut ntfy) = settings.ntfy.clone() else {
        return base(
            Notifier::DryRun,
            Some(
                "notification settings have no \"ntfy\" section; notifications are logged only (dry run)"
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

fn heartbeat_plan(
    heartbeat: &HeartbeatSettings,
    warnings: &mut Vec<String>,
) -> Result<HeartbeatPlan, String> {
    use optic_heartbeat::{
        MAX_ALERT_AFTER_SECS, MAX_INTERVAL_SECS, MIN_ALERT_AFTER_SECS, MIN_ALERT_MARGIN_SECS,
        MIN_INTERVAL_SECS,
    };
    if !optic_heartbeat::valid_sequence_id(&heartbeat.sequence_id) {
        return Err(
            "heartbeat.sequence_id must be 1-64 characters of A-Z, a-z, 0-9, '-' or '_'".to_owned(),
        );
    }
    let interval = heartbeat
        .interval_secs
        .clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS);
    let alert_after = heartbeat
        .alert_after_secs
        .clamp(MIN_ALERT_AFTER_SECS, MAX_ALERT_AFTER_SECS);
    if interval != heartbeat.interval_secs || alert_after != heartbeat.alert_after_secs {
        warnings.push(format!(
            "heartbeat interval must be {MIN_INTERVAL_SECS}-{MAX_INTERVAL_SECS} s and alert delay {MIN_ALERT_AFTER_SECS}-{MAX_ALERT_AFTER_SECS} s; clamped"
        ));
    }
    if alert_after < interval + MIN_ALERT_MARGIN_SECS {
        warnings.push(format!(
            "heartbeat.alert_after_secs should be at least interval_secs + {MIN_ALERT_MARGIN_SECS}; one late check-in may false-alarm"
        ));
    }
    Ok(HeartbeatPlan {
        interval: Duration::seconds(interval as i64),
        alert_after: Duration::seconds(alert_after as i64),
        sequence_id: heartbeat.sequence_id.clone(),
    })
}

/// Strict validation for a save from the Config page: anything out of range
/// is rejected with a reason instead of clamped.
pub fn validate_for_save(settings: &NotificationSettings) -> Result<(), String> {
    use optic_heartbeat::{
        MAX_ALERT_AFTER_SECS, MAX_INTERVAL_SECS, MIN_ALERT_AFTER_SECS, MIN_ALERT_MARGIN_SECS,
        MIN_INTERVAL_SECS,
    };
    if let Some(name) = &settings.station_name
        && (name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control))
    {
        return Err("station name must be 1-64 characters without control characters".to_owned());
    }
    match &settings.ntfy {
        Some(ntfy) => validate_ntfy(ntfy)?,
        None if settings.enabled => {
            return Err("an ntfy topic is required to turn notifications on".to_owned());
        }
        None => {}
    }
    if optic_digest::parse_send_at(&settings.digest.send_at).is_none() {
        return Err("digest send time must be HH:MM (24 h)".to_owned());
    }
    if let Some(heartbeat) = &settings.heartbeat {
        if !optic_heartbeat::valid_sequence_id(&heartbeat.sequence_id) {
            return Err(
                "heartbeat sequence ID must be 1-64 characters of A-Z, a-z, 0-9, '-' or '_'"
                    .to_owned(),
            );
        }
        if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&heartbeat.interval_secs) {
            return Err(format!(
                "heartbeat interval must be {}-{} minutes",
                MIN_INTERVAL_SECS / 60,
                MAX_INTERVAL_SECS / 60
            ));
        }
        if !(MIN_ALERT_AFTER_SECS..=MAX_ALERT_AFTER_SECS).contains(&heartbeat.alert_after_secs) {
            return Err(format!(
                "heartbeat alert delay must be {} minutes to {} days",
                MIN_ALERT_AFTER_SECS / 60,
                MAX_ALERT_AFTER_SECS / 86_400
            ));
        }
        if heartbeat.alert_after_secs < heartbeat.interval_secs + MIN_ALERT_MARGIN_SECS {
            return Err(format!(
                "heartbeat alert delay must be at least the interval plus {} minutes",
                MIN_ALERT_MARGIN_SECS / 60
            ));
        }
    }
    Ok(())
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
    // '*' is excluded so a masked token (`tk_a****wxyz`) can never be saved
    // back; real ntfy tokens are `tk_` plus letters and digits.
    if let Some(token) = &ntfy.token
        && (token.is_empty() || !token.chars().all(|c| c.is_ascii_graphic() && c != '*'))
    {
        return Err(
            "ntfy.token must be non-empty printable ASCII without spaces or '*'".to_owned(),
        );
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
// Settings in config.json: masking, view, update, save
// ---------------------------------------------------------------------------

/// Paths of the global config: committed (durable + tmpfs mirror) and the
/// staging copy the dashboard's stage/commit flow uses.
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    pub config_path: PathBuf,
    pub config_cache_path: PathBuf,
    pub preview_config_path: PathBuf,
}

/// Serializes read-modify-write saves of the `notifications` section (the
/// Config page and the one-time import).
static SAVE_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Masks the ntfy topic and token in a `notifications` value before it is
/// served (`GET /api/status`). Any `topic`/`token` key at any depth is
/// masked, so even a hand-edited, misshapen section cannot leak. The masks
/// contain `****`, which topic and token validation reject, so a masked
/// value can never be saved back as a real secret.
pub fn redact_notifications(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                match (key.as_str(), &*entry) {
                    ("topic" | "token", Value::String(secret)) => {
                        *entry = Value::String(mask_secret(secret));
                    }
                    ("token", Value::Null) => {}
                    ("topic" | "token", _) => *entry = Value::String(SECRET_MASK.to_owned()),
                    _ => redact_notifications(entry),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_notifications),
        _ => {}
    }
}

const SECRET_MASK: &str = "****";
/// Secrets shorter than this are masked completely, so at least 8
/// characters always stay hidden.
const MIN_PARTLY_SHOWN_SECRET: usize = 16;

/// First 4 and last 4 characters with `****` between (`opti****a7f3`),
/// enough to recognise which secret is set. A generated topic (`optic-` +
/// 24 random characters) keeps 22 random characters hidden.
fn mask_secret(secret: &str) -> String {
    let count = secret.chars().count();
    if count < MIN_PARTLY_SHOWN_SECRET {
        return SECRET_MASK.to_owned();
    }
    let head: String = secret.chars().take(4).collect();
    let tail: String = secret.chars().skip(count - 4).collect();
    format!("{head}{SECRET_MASK}{tail}")
}

/// What the Config page sees: never the topic or token themselves.
#[derive(Debug, Clone, Serialize)]
pub struct NotificationsView {
    /// `config` (config.json), `alerts_file` (legacy file, import pending),
    /// or `none`.
    pub source: &'static str,
    pub enabled: bool,
    pub station_name: Option<String>,
    pub server: String,
    pub topic_set: bool,
    /// Masked (`opti****a7f3`), never the topic itself.
    pub topic_hint: Option<String>,
    pub token_set: bool,
    /// Masked like `topic_hint`.
    pub token_hint: Option<String>,
    pub digest: DigestSettings,
    pub heartbeat: HeartbeatSettings,
    /// Why the saved settings cannot be used, if they cannot.
    pub error: Option<String>,
}

impl NotificationsView {
    fn new(source: &'static str, settings: Result<&NotificationSettings, &str>) -> Self {
        let error = settings.err().map(str::to_owned);
        let settings = settings.ok();
        let ntfy = settings.and_then(|settings| settings.ntfy.as_ref());
        Self {
            source,
            enabled: settings.is_some_and(|settings| settings.enabled),
            station_name: settings.and_then(|settings| settings.station_name.clone()),
            server: ntfy.map_or_else(default_ntfy_server, |ntfy| ntfy.server.clone()),
            topic_set: ntfy.is_some(),
            topic_hint: ntfy.map(|ntfy| mask_secret(&ntfy.topic)),
            token_set: ntfy.is_some_and(|ntfy| ntfy.token.is_some()),
            token_hint: ntfy.and_then(|ntfy| ntfy.token.as_deref()).map(mask_secret),
            digest: settings
                .map_or_else(DigestSettings::default, |settings| settings.digest.clone()),
            heartbeat: settings
                .and_then(|settings| settings.heartbeat.clone())
                .unwrap_or(HeartbeatSettings {
                    enabled: false,
                    ..HeartbeatSettings::default()
                }),
            error,
        }
    }
}

/// A save from the Config page. An absent or empty `topic`/`token` keeps
/// the current one; `clear_token` removes the token.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationsUpdate {
    pub enabled: bool,
    #[serde(default)]
    pub station_name: Option<String>,
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub clear_token: bool,
    pub digest: DigestSettings,
    pub heartbeat: HeartbeatSettings,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SaveError {
    Invalid(String),
    Io(String),
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Merges an update into the current settings (pure). Thresholds are not
/// edited in the UI and are always kept.
fn merge_update(
    current: Option<&NotificationSettings>,
    update: NotificationsUpdate,
) -> NotificationSettings {
    let current_ntfy = current.and_then(|current| current.ntfy.as_ref());
    let server = non_empty(update.server.as_deref())
        .map(|server| server.trim_end_matches('/').to_owned())
        .or_else(|| current_ntfy.map(|ntfy| ntfy.server.clone()))
        .unwrap_or_else(default_ntfy_server);
    let topic =
        non_empty(update.topic.as_deref()).or_else(|| current_ntfy.map(|ntfy| ntfy.topic.clone()));
    let token = if update.clear_token {
        None
    } else {
        non_empty(update.token.as_deref())
            .or_else(|| current_ntfy.and_then(|ntfy| ntfy.token.clone()))
    };
    NotificationSettings {
        enabled: update.enabled,
        station_name: non_empty(update.station_name.as_deref()),
        ntfy: topic.map(|topic| NtfyConfig {
            server,
            topic,
            token,
        }),
        digest: update.digest,
        // Kept even when off, so the values are remembered.
        heartbeat: Some(update.heartbeat),
        thresholds: current.map_or_else(Thresholds::default, |current| current.thresholds.clone()),
    }
}

async fn read_committed(cache_path: &Path) -> Option<Value> {
    let content = durable_state::read_cached(cache_path).await.ok()?;
    serde_json::from_str(&content).ok()
}

fn notifications_of(config: &Value) -> Option<&Value> {
    config.get("notifications").filter(|value| !value.is_null())
}

/// The settings in effect: `config.json`'s section if present, else the
/// legacy file (only until it is imported), else none.
fn effective_settings(
    config: Option<&Value>,
    legacy: &AlertsSetup,
) -> (&'static str, Result<NotificationSettings, String>) {
    if let Some(section) = config.and_then(notifications_of) {
        return (
            "config",
            serde_json::from_value(section.clone()).map_err(|error| {
                format!(
                    "notification settings in config.json are invalid ({error}); notifications are logged only (dry run)"
                )
            }),
        );
    }
    match &legacy.settings {
        Some(settings) => ("alerts_file", Ok(settings.clone())),
        None => (
            "none",
            Err(legacy.config_error.clone().unwrap_or_else(|| {
                "no notification settings; notifications are logged only (dry run)".to_owned()
            })),
        ),
    }
}

fn set_notifications(config: &mut Value, section: Value) -> std::io::Result<()> {
    match config.as_object_mut() {
        Some(map) => {
            map.insert("notifications".to_owned(), section);
            Ok(())
        }
        None => Err(std::io::Error::other(
            "config.json is not a JSON object; not overwriting it",
        )),
    }
}

/// Writes `settings` as the `notifications` section of the committed
/// config (durable first, then the tmpfs mirror) and of any staging copy,
/// so a later commit of unrelated staged edits cannot bring back the old
/// settings. When nothing was staged, nothing is staged afterwards (the
/// staging copy is written byte-identical to the committed config). Every
/// other field is kept as is. Callers hold `SAVE_LOCK`.
async fn write_notifications(
    paths: &ConfigPaths,
    settings: &NotificationSettings,
) -> std::io::Result<()> {
    let section = serde_json::to_value(settings).map_err(std::io::Error::other)?;
    let committed_text = durable_state::read_cached(&paths.config_cache_path)
        .await
        .ok();
    let preview_text = tokio::fs::read_to_string(&paths.preview_config_path)
        .await
        .ok();
    let was_staged = match (&preview_text, &committed_text) {
        (Some(preview), Some(committed)) => preview != committed,
        (Some(_), None) => true,
        _ => false,
    };
    let mut committed = match &committed_text {
        Some(text) => serde_json::from_str::<Value>(text).map_err(|error| {
            std::io::Error::other(format!(
                "config.json is not valid JSON ({error}); not overwriting it"
            ))
        })?,
        None => Value::Object(serde_json::Map::new()),
    };
    set_notifications(&mut committed, section.clone())?;
    let committed_new = serde_json::to_string_pretty(&committed).map_err(std::io::Error::other)?;
    durable_state::write_through(&paths.config_path, &paths.config_cache_path, &committed_new)
        .await?;
    let Some(preview) = preview_text else {
        return Ok(());
    };
    let preview_new = if was_staged {
        // A malformed staging copy is left alone; committing it would fail
        // on its own anyway.
        let Ok(mut staged) = serde_json::from_str::<Value>(&preview) else {
            return Ok(());
        };
        if set_notifications(&mut staged, section).is_err() {
            return Ok(());
        }
        serde_json::to_string_pretty(&staged).map_err(std::io::Error::other)?
    } else {
        committed_new
    };
    let temp = paths.preview_config_path.with_extension("json.tmp");
    tokio::fs::write(&temp, preview_new).await?;
    tokio::fs::rename(temp, &paths.preview_config_path).await
}

/// One-time import of the legacy `alerts.json` into `config.json`.
async fn import_legacy(paths: &ConfigPaths, legacy: &AlertsSetup) {
    let Some(settings) = &legacy.settings else {
        return;
    };
    let _guard = SAVE_LOCK.lock().await;
    let committed = read_committed(&paths.config_cache_path).await;
    if committed.as_ref().and_then(notifications_of).is_some() {
        tracing::info!(
            path = %resolve_config_path().display(),
            "config.json already has notification settings; the legacy alerts config is not used and can be deleted"
        );
        return;
    }
    match write_notifications(paths, settings).await {
        Ok(()) => tracing::info!(
            path = %resolve_config_path().display(),
            "imported the legacy alerts config into config.json; it is no longer read and can be deleted"
        ),
        Err(error) => tracing::warn!(
            %error,
            "could not import the legacy alerts config into config.json; using it for this run only"
        ),
    }
}

// ---------------------------------------------------------------------------
// Notification delivery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Notifier {
    /// Log only. Used when no channel is configured, and in local tests.
    DryRun,
    Ntfy(NtfySender),
}

fn transition_style(transition: Transition) -> (u8, &'static str) {
    match transition {
        Transition::Fired | Transition::Reminder => (4, "warning"),
        Transition::Resolved => (3, "white_check_mark"),
    }
}

impl Notifier {
    fn channel(&self) -> &'static str {
        match self {
            Self::DryRun => "dry_run",
            Self::Ntfy(_) => "ntfy",
        }
    }

    async fn publish(
        &self,
        title: &str,
        message: &str,
        priority: u8,
        tag: &str,
    ) -> Result<(), String> {
        match self {
            Self::DryRun => {
                tracing::warn!(
                    %title,
                    %message,
                    "notification (dry run; no notification channel configured)"
                );
                Ok(())
            }
            Self::Ntfy(sender) => {
                sender
                    .run(publish_curl_config(
                        &sender.config,
                        title,
                        message,
                        priority,
                        tag,
                    ))
                    .await?;
                tracing::info!(%title, "notification delivered via ntfy");
                Ok(())
            }
        }
    }

    async fn deliver(
        &self,
        notification: &Notification,
        station_name: Option<&str>,
    ) -> Result<(), String> {
        let (priority, tag) = transition_style(notification.transition);
        self.publish(
            &notification.title(station_name),
            &notification.message(),
            priority,
            tag,
        )
        .await
    }
}

#[derive(Debug, Clone)]
struct NtfySender {
    config: NtfyConfig,
    /// The `curl` binary; overridable only by tests.
    curl: PathBuf,
}

impl NtfySender {
    /// Runs curl with `config` (a curl config file) on stdin.
    async fn run(&self, config: String) -> Result<(), String> {
        run_curl(&self.curl, config).await
    }
}

async fn run_curl(curl: &Path, config: String) -> Result<(), String> {
    // `-q` ignores any ~/.curlrc; `--config -` reads URL, headers, and
    // body from stdin so the token never appears in argv.
    let mut child = Command::new(curl)
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

/// Builds the curl config file (fed on stdin) for one ntfy JSON publish.
fn publish_curl_config(
    ntfy: &NtfyConfig,
    title: &str,
    message: &str,
    priority: u8,
    tag: &str,
) -> String {
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
pub(crate) fn curl_escape(value: &str) -> String {
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

#[derive(Debug, Clone, Default, Serialize)]
pub struct DigestStatus {
    pub enabled: bool,
    pub send_at: Option<String>,
    pub timezone: String,
    pub next_due_at: Option<DateTime<Utc>>,
    /// End of the last window sent (or skipped).
    pub last_window_end: Option<DateTime<Utc>>,
    pub last_sent_at: Option<DateTime<Utc>>,
    /// A digest is waiting for delivery.
    pub pending: bool,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeartbeatStatus {
    /// `disabled`, `dry_run`, `ok`, `withheld`, or `failing`.
    pub state: &'static str,
    pub interval_secs: Option<u64>,
    pub alert_after_secs: Option<u64>,
    pub last_checkin_at: Option<DateTime<Utc>>,
    pub next_checkin_at: Option<DateTime<Utc>>,
    pub withheld_reason: Option<String>,
    pub last_error: Option<String>,
}

impl Default for HeartbeatStatus {
    fn default() -> Self {
        Self {
            state: "disabled",
            interval_secs: None,
            alert_after_secs: None,
            last_checkin_at: None,
            next_checkin_at: None,
            withheld_reason: None,
            last_error: None,
        }
    }
}

/// Never contains the ntfy topic, token, sequence ID, or config path.
#[derive(Debug, Clone, Serialize)]
pub struct AlertsStatus {
    pub channel: &'static str,
    /// Where the settings came from: `config`, `alerts_file`, or `none`.
    pub settings_source: &'static str,
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
    pub digest: DigestStatus,
    pub heartbeat: HeartbeatStatus,
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

    /// Titles of conditions that are notified and not yet resolved; with
    /// `capture_path_only`, just the ones that withhold the heartbeat
    /// (`docs/optic-daemon-digest-heartbeat.md` §4.3).
    fn active_titles(&self, capture_path_only: bool) -> Vec<&'static str> {
        self.condition_statuses()
            .into_iter()
            .filter(|status| matches!(status.state, "firing" | "recovering"))
            .filter(|status| {
                !capture_path_only
                    || matches!(
                        status.condition,
                        Condition::CaptureStalled
                            | Condition::CaptureOverdue
                            | Condition::CaptureFailing
                    )
            })
            .map(|status| status.condition.title())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Actor
// ---------------------------------------------------------------------------

/// The existing handles the monitor reads from. All reads are
/// non-mutating, except that the actor writes `config.json`'s
/// `notifications` section once to import the legacy file, and its own
/// digest state file.
pub struct AlertSources {
    pub scheduler: SchedulerHandle,
    pub sync: DataSyncManager,
    pub capture_log: Option<CaptureLog>,
    pub system_status: SystemStatusReader,
    pub config_paths: ConfigPaths,
    pub events: Option<SystemEventLog>,
    /// `digest_state.json` in the state directory.
    pub digest_state_path: PathBuf,
    /// Whether `OpticCamera::probe` found the camera at startup.
    pub camera_detected: bool,
    pub version: &'static str,
}

/// Why a test or on-demand digest was not sent.
#[derive(Debug, Clone, PartialEq)]
pub enum SendError {
    /// No usable channel (dry run); carries the reason.
    NotConfigured(String),
    /// At most one test/on-demand send per `MANUAL_SEND_SPACING`.
    RateLimited,
    Failed(String),
}

const MANUAL_SEND_SPACING: Duration = Duration::seconds(10);

enum ActorCommand {
    /// Re-read the settings and evaluate now (sent after a save).
    Reload,
    Test {
        reply: oneshot::Sender<Result<(), SendError>>,
    },
    DigestNow {
        reply: oneshot::Sender<Result<(), SendError>>,
    },
}

#[derive(Clone)]
pub struct AlertsHandle {
    status: watch::Receiver<AlertsStatus>,
    commands: mpsc::Sender<ActorCommand>,
    legacy: Arc<AlertsSetup>,
    paths: ConfigPaths,
}

impl AlertsHandle {
    pub fn spawn(legacy: AlertsSetup, sources: AlertSources) -> Self {
        let monitor = Monitor::new(legacy.thresholds.clone(), Utc::now());
        let (status_tx, status) = watch::channel(AlertsStatus {
            channel: Notifier::DryRun.channel(),
            settings_source: "none",
            config_error: Some("not evaluated yet".to_owned()),
            config_warnings: Vec::new(),
            poll_interval_secs: POLL_INTERVAL.as_secs(),
            last_evaluated_at: None,
            active_count: 0,
            conditions: monitor.condition_statuses(),
            outbox_len: 0,
            last_delivered_at: None,
            last_delivery_error: None,
            thresholds: legacy.thresholds.clone(),
            digest: DigestStatus::default(),
            heartbeat: HeartbeatStatus::default(),
        });
        let (commands, receiver) = mpsc::channel(8);
        let handle = Self {
            status,
            commands,
            legacy: Arc::new(legacy.clone()),
            paths: sources.config_paths.clone(),
        };
        tokio::spawn(run_actor(legacy, sources, monitor, status_tx, receiver));
        handle
    }

    pub fn status(&self) -> AlertsStatus {
        self.status.borrow().clone()
    }

    pub async fn reload(&self) {
        let _ = self.commands.send(ActorCommand::Reload).await;
    }

    async fn request(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<(), SendError>>) -> ActorCommand,
    ) -> Result<(), SendError> {
        let (reply, answer) = oneshot::channel();
        let not_running = || SendError::Failed("the alerts monitor is not running".to_owned());
        self.commands
            .send(make(reply))
            .await
            .map_err(|_| not_running())?;
        answer.await.map_err(|_| not_running())?
    }

    /// Sends a test notification through the saved channel.
    pub async fn send_test(&self) -> Result<(), SendError> {
        self.request(|reply| ActorCommand::Test { reply }).await
    }

    /// Sends a digest of the last 24 h now (does not affect the daily one).
    pub async fn send_digest_now(&self) -> Result<(), SendError> {
        self.request(|reply| ActorCommand::DigestNow { reply })
            .await
    }

    /// The settings as the Config page sees them (no secrets).
    pub async fn settings_view(&self) -> NotificationsView {
        let config = read_committed(&self.paths.config_cache_path).await;
        let (source, settings) = effective_settings(config.as_ref(), &self.legacy);
        NotificationsView::new(source, settings.as_ref().map_err(String::as_str))
    }

    /// Validates and saves a Config page update into `config.json`, then
    /// applies it without a restart.
    pub async fn save_settings(
        &self,
        update: NotificationsUpdate,
    ) -> Result<NotificationsView, SaveError> {
        let merged = {
            let _guard = SAVE_LOCK.lock().await;
            let config = read_committed(&self.paths.config_cache_path).await;
            let (_, current) = effective_settings(config.as_ref(), &self.legacy);
            let merged = merge_update(current.ok().as_ref(), update);
            validate_for_save(&merged).map_err(SaveError::Invalid)?;
            write_notifications(&self.paths, &merged)
                .await
                .map_err(|error| SaveError::Io(error.to_string()))?;
            merged
        };
        tracing::info!("notification settings saved from the dashboard");
        self.reload().await;
        Ok(NotificationsView::new("config", Ok(&merged)))
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

/// Signals gathered alongside the `Observation`, used by the digest.
#[derive(Debug, Clone)]
struct Extras {
    sync: SyncStatus,
    state_disk: Option<(u64, u64)>,
    uptime_secs: u64,
}

#[derive(Debug, Default)]
struct HeartbeatRuntime {
    /// Where the pending scheduled message lives, as far as this process
    /// knows.
    armed_on: Option<ArmedOn>,
    /// A message orphaned by a settings change, to cancel before
    /// `deadline` (when it would be delivered anyway).
    pending_cancel: Option<(ArmedOn, DateTime<Utc>)>,
    withheld_reason: Option<String>,
    last_error: Option<String>,
    dry_run_logged_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Default)]
struct DigestRuntime {
    pending: Option<(DateTime<Utc>, Digest)>,
    last_sent_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
}

struct Actor {
    sources: AlertSources,
    legacy: AlertsSetup,
    source: &'static str,
    /// The settings currently applied; `None` before the first tick.
    effective: Option<Result<NotificationSettings, String>>,
    setup: AlertsSetup,
    monitor: Monitor,
    tracker: OutcomeTracker,
    outbox: Outbox,
    delivery: DeliveryState,
    digest_state: DigestState,
    dirty: bool,
    started_at: DateTime<Utc>,
    heartbeat: HeartbeatRuntime,
    digest: DigestRuntime,
    last_manual_send_at: Option<DateTime<Utc>>,
    tz: Tz,
    last_observation: Option<(Observation, Extras)>,
    /// The `curl` binary used to cancel an orphaned heartbeat.
    curl: PathBuf,
}

fn station_tz(schedule: &ScheduleConfig) -> Tz {
    schedule
        .station
        .as_ref()
        .and_then(|station| station.timezone.parse().ok())
        .unwrap_or(chrono_tz::UTC)
}

fn local_time(at: DateTime<Utc>, tz: Tz) -> String {
    at.with_timezone(&tz).format("%a %H:%M %Z").to_string()
}

async fn run_actor(
    legacy: AlertsSetup,
    sources: AlertSources,
    monitor: Monitor,
    status_tx: watch::Sender<AlertsStatus>,
    mut commands: mpsc::Receiver<ActorCommand>,
) {
    import_legacy(&sources.config_paths, &legacy).await;
    let (digest_state, warning) = DigestState::load(&sources.digest_state_path);
    if let Some(warning) = warning {
        tracing::warn!(%warning, path = %sources.digest_state_path.display(), "digest state");
    }
    let mut actor = Actor {
        sources,
        legacy,
        source: "none",
        effective: None,
        setup: dry_run_setup("not evaluated yet"),
        monitor,
        tracker: OutcomeTracker::default(),
        outbox: Outbox::default(),
        delivery: DeliveryState::default(),
        digest_state,
        dirty: false,
        started_at: Utc::now(),
        heartbeat: HeartbeatRuntime::default(),
        digest: DigestRuntime::default(),
        last_manual_send_at: None,
        tz: chrono_tz::UTC,
        last_observation: None,
        curl: PathBuf::from("curl"),
    };
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut commands_open = true;
    loop {
        tokio::select! {
            _ = interval.tick() => actor.tick().await,
            command = commands.recv(), if commands_open => match command {
                Some(ActorCommand::Reload) => actor.tick().await,
                Some(ActorCommand::Test { reply }) => {
                    let _ = reply.send(actor.send_test().await);
                }
                Some(ActorCommand::DigestNow { reply }) => {
                    let _ = reply.send(actor.send_digest_now().await);
                }
                None => commands_open = false,
            },
        }
        status_tx.send_modify(|status| actor.fill_status(status));
    }
}

impl Actor {
    async fn tick(&mut self) {
        let now = Utc::now();
        let config = read_committed(&self.sources.config_paths.config_cache_path).await;
        let schedule = config
            .as_ref()
            .and_then(|config| config.get("schedule"))
            .and_then(|schedule| serde_json::from_value::<ScheduleConfig>(schedule.clone()).ok())
            .unwrap_or_default();
        self.tz = station_tz(&schedule);
        self.apply_settings(now, config.as_ref());

        let (observation, extras) = gather(
            &self.sources,
            &mut self.tracker,
            &self.monitor.thresholds,
            schedule,
            now,
        )
        .await;
        for notification in self.monitor.observe(&observation) {
            tracing::info!(
                condition = ?notification.condition,
                transition = ?notification.transition,
                detail = %notification.detail,
                "health alert state change"
            );
            if notification.transition == Transition::Fired {
                self.digest_state
                    .record_alert(now, notification.condition.title());
                self.dirty = true;
            }
            self.outbox.push(notification);
        }
        flush(
            &mut self.outbox,
            &self.setup.notifier,
            self.setup.station_name.as_deref(),
            &mut self.delivery,
        )
        .await;

        self.dirty |= self
            .digest_state
            .record_run_state(now, observation.scheduler_running);
        self.dirty |= self.digest_state.record_sample(
            now,
            Sample {
                cpu_temp_celsius: observation.cpu_temp_celsius,
                capture_disk: observation.capture_disk,
                transferred: Some((extras.sync.transferred_files, extras.sync.transferred_bytes)),
            },
        );
        self.digest_state.prune(now);

        self.heartbeat_step(now).await;
        self.digest_step(now, &observation, &extras).await;
        self.last_observation = Some((observation, extras));
        self.persist().await;
    }

    /// Applies the settings in effect if they changed since the last tick.
    fn apply_settings(&mut self, now: DateTime<Utc>, config: Option<&Value>) {
        let (source, effective) = effective_settings(config, &self.legacy);
        if self.source == source && self.effective.as_ref() == Some(&effective) {
            return;
        }
        let setup = match (&effective, source) {
            // Keeps the legacy file's own warnings (e.g. file mode).
            (Ok(_), "alerts_file") => self.legacy.clone(),
            (Ok(settings), _) => resolve(settings),
            (Err(reason), _) => dry_run_setup(reason),
        };
        let new_armed = match (&setup.heartbeat, &setup.notifier) {
            (Some(plan), Notifier::Ntfy(sender)) => {
                Some(ArmedOn::new(&sender.config, &plan.sequence_id))
            }
            _ => None,
        };
        let last_checkin = self.digest_state.heartbeat_last_checkin;
        if self.effective.is_none() {
            // First tick after a start: a message armed by the previous run
            // is assumed to be on the current channel.
            if let Some(plan) = &setup.heartbeat
                && optic_heartbeat::is_armed(last_checkin, now, plan.alert_after)
            {
                self.heartbeat.armed_on = new_armed.clone();
            }
        } else if optic_heartbeat::cancel_needed(
            self.heartbeat.armed_on.as_ref(),
            new_armed.as_ref(),
        ) && let Some(armed) = self.heartbeat.armed_on.take()
        {
            let old_alert_after = self.setup.heartbeat.as_ref().map_or(
                Duration::seconds(optic_heartbeat::MAX_ALERT_AFTER_SECS as i64),
                |plan| plan.alert_after,
            );
            if let Some(last) = last_checkin
                && optic_heartbeat::is_armed(Some(last), now, old_alert_after)
            {
                tracing::info!(
                    "notification settings changed; cancelling the armed heartbeat on the previous channel"
                );
                self.heartbeat.pending_cancel = Some((armed, last + old_alert_after));
            }
            // The new channel starts fresh: check in at once, and no false
            // "checking in again" notice.
            self.digest_state.heartbeat_last_checkin = None;
            self.dirty = true;
        }
        match &setup.config_error {
            Some(error) => tracing::warn!(source, %error, "notifications running in dry-run mode"),
            None => tracing::info!(
                source,
                channel = setup.notifier.channel(),
                digest = setup.digest_send_at.is_some(),
                heartbeat = setup.heartbeat.is_some(),
                "notification settings applied"
            ),
        }
        for warning in &setup.config_warnings {
            tracing::warn!(%warning, "notification settings warning");
        }
        self.monitor.thresholds = setup.thresholds.clone();
        self.setup = setup;
        self.source = source;
        self.effective = Some(effective);
    }

    fn heartbeat_state(&self) -> &'static str {
        match (&self.setup.heartbeat, &self.setup.notifier) {
            (None, _) => "disabled",
            (Some(_), Notifier::DryRun) => "dry_run",
            _ if self.heartbeat.withheld_reason.is_some() => "withheld",
            _ if self.heartbeat.last_error.is_some() => "failing",
            _ => "ok",
        }
    }

    async fn heartbeat_step(&mut self, now: DateTime<Utc>) {
        if let Some((armed, deadline)) = self.heartbeat.pending_cancel.clone() {
            if now >= deadline {
                tracing::warn!(
                    "the heartbeat on the previous channel could not be cancelled before it was delivered"
                );
                self.heartbeat.pending_cancel = None;
            } else {
                match run_curl(&self.curl, optic_heartbeat::cancel_curl_config(&armed)).await {
                    Ok(()) => {
                        tracing::info!("cancelled the heartbeat armed on the previous channel");
                        self.heartbeat.pending_cancel = None;
                    }
                    // Already delivered or never stored: nothing to cancel.
                    Err(error) if error.contains("404") => {
                        self.heartbeat.pending_cancel = None;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "cancelling the previous heartbeat failed; will retry");
                        self.heartbeat.last_error = Some(format!("cancel failed: {error}"));
                    }
                }
            }
        }

        let Some(plan) = self.setup.heartbeat.clone() else {
            self.heartbeat.withheld_reason = None;
            return;
        };
        let withheld_by = self.monitor.active_titles(true);
        match optic_heartbeat::gate(&withheld_by, self.sources.camera_detected) {
            Err(reason) => {
                if self.heartbeat.withheld_reason.is_none() {
                    tracing::warn!(%reason, "heartbeat withheld");
                    self.digest_state.record_withheld(now);
                    self.dirty = true;
                }
                self.heartbeat.withheld_reason = Some(reason);
                return;
            }
            Ok(()) => {
                if self.heartbeat.withheld_reason.take().is_some() {
                    tracing::info!("heartbeat resumed");
                }
            }
        }
        let last = self.digest_state.heartbeat_last_checkin;
        let station = self.setup.station_name.clone();
        let sender = match &self.setup.notifier {
            Notifier::DryRun => {
                if self
                    .heartbeat
                    .dry_run_logged_at
                    .is_none_or(|at| now - at >= plan.interval)
                {
                    tracing::info!("heartbeat (dry run): would check in");
                    self.heartbeat.dry_run_logged_at = Some(now);
                }
                return;
            }
            Notifier::Ntfy(sender) => sender.clone(),
        };
        if !optic_heartbeat::is_due(last, now, &plan) {
            return;
        }
        let now_local = local_time(now, self.tz);
        let (title, message) =
            optic_heartbeat::silent_alert(station.as_deref(), plan.alert_after, &now_local);
        match sender
            .run(optic_heartbeat::arm_curl_config(
                &sender.config,
                &plan,
                &title,
                &message,
            ))
            .await
        {
            Ok(()) => {
                tracing::debug!("heartbeat checked in");
                self.digest_state.heartbeat_last_checkin = Some(now);
                self.dirty = true;
                self.heartbeat.last_error = None;
                self.heartbeat.armed_on = Some(ArmedOn::new(&sender.config, &plan.sequence_id));
                if let (Some(previous), Some(gap)) = (
                    last,
                    optic_heartbeat::silent_period(last, now, plan.alert_after),
                ) {
                    let (title, message) = optic_heartbeat::back_notice(
                        station.as_deref(),
                        &local_time(previous, self.tz),
                        &now_local,
                        &optic_digest::format_duration(gap),
                    );
                    if let Err(error) = self
                        .setup
                        .notifier
                        .publish(&title, &message, 3, "white_check_mark")
                        .await
                    {
                        tracing::warn!(%error, "could not send the heartbeat recovery notice");
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "heartbeat check-in failed; will retry next tick");
                self.heartbeat.last_error = Some(error);
            }
        }
    }

    async fn digest_step(
        &mut self,
        now: DateTime<Utc>,
        observation: &Observation,
        extras: &Extras,
    ) {
        if let Some(send_at) = self.setup.digest_send_at {
            match optic_digest::decide(now, self.tz, send_at, self.digest_state.last_digest_due) {
                Decision::Idle => {}
                Decision::Initialize { due } => {
                    self.digest_state.last_digest_due = Some(due);
                    self.dirty = true;
                }
                Decision::Skip { due } => {
                    tracing::warn!(%due, "daily digest skipped: more than 12 h late");
                    self.digest_state.last_digest_due = Some(due);
                    self.dirty = true;
                }
                Decision::Send { start, end } => {
                    let digest = self
                        .build_digest(start, end, now, observation, extras)
                        .await;
                    self.digest.pending = Some((end, digest));
                    self.digest_state.last_digest_due = Some(end);
                    self.dirty = true;
                }
            }
        }
        if let Some((due, digest)) = self.digest.pending.clone() {
            if now - due > optic_digest::CATCH_UP {
                tracing::warn!(%due, "daily digest dropped: undelivered for 12 h");
                self.digest.pending = None;
                return;
            }
            match self
                .setup
                .notifier
                .publish(&digest.title, &digest.message, 2, "bar_chart")
                .await
            {
                Ok(()) => {
                    tracing::info!(%due, "daily digest sent");
                    self.digest.pending = None;
                    self.digest.last_sent_at = Some(now);
                    self.digest.last_error = None;
                }
                Err(error) => {
                    tracing::warn!(%error, "daily digest delivery failed; will retry");
                    self.digest.last_error = Some(error);
                }
            }
        }
    }

    async fn build_digest(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        now: DateTime<Utc>,
        observation: &Observation,
        extras: &Extras,
    ) -> Digest {
        let rows: Vec<CaptureRow> = match &self.sources.capture_log {
            Some(log) => log
                .window_rows(start.timestamp(), end.timestamp())
                .await
                .into_iter()
                .map(|row| CaptureRow {
                    at: Utc
                        .timestamp_opt(row.captured_at_unix, 0)
                        .single()
                        .unwrap_or(start),
                    scheduled: row.source == "scheduler",
                    success: row.success,
                    bytes: row.bytes_total,
                    error: row.error,
                })
                .collect(),
            None => Vec::new(),
        };
        let events: Vec<EventRow> = match &self.sources.events {
            Some(log) => log
                .list(200)
                .await
                .into_iter()
                .map(|event| EventRow {
                    at: Utc
                        .timestamp_millis_opt(event.occurred_at_unix_ms)
                        .single()
                        .unwrap_or(start),
                    unexpected: event
                        .detail
                        .pointer("/previous_boot/ended")
                        .and_then(Value::as_str)
                        == Some("unexpected"),
                    kind: event.kind,
                })
                .collect(),
            None => Vec::new(),
        };
        let heartbeat = self.setup.heartbeat.as_ref().map(|_| HeartbeatSnapshot {
            state: self.heartbeat_state().to_owned(),
            last_checkin: self.digest_state.heartbeat_last_checkin,
        });
        let sync = &extras.sync;
        optic_digest::build(&DigestInput {
            start,
            end,
            tz: self.tz,
            station_name: self.setup.station_name.as_deref(),
            schedule: &observation.schedule,
            rows: &rows,
            state: &self.digest_state,
            sync: Some(SyncSnapshot {
                enabled: sync.enabled,
                paused: sync.paused,
                queued_files: sync.queued_files,
                queued_bytes: sync.queued_bytes,
                connectivity: serde_json::to_value(sync.connectivity)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "unknown".to_owned()),
                last_error: sync.last_error.clone(),
            }),
            capture_disk: observation.capture_disk,
            state_disk: extras.state_disk,
            active_alerts: self
                .monitor
                .active_titles(false)
                .into_iter()
                .map(str::to_owned)
                .collect(),
            events: &events,
            heartbeat,
            host_uptime_secs: Some(extras.uptime_secs),
            daemon_started_at: self.started_at,
            version: self.sources.version,
            now,
        })
    }

    /// Re-reads the settings (a save may have just happened) and applies the
    /// manual-send rules shared by the test and on-demand digest.
    async fn manual_send_allowed(&mut self, now: DateTime<Utc>) -> Result<(), SendError> {
        let config = read_committed(&self.sources.config_paths.config_cache_path).await;
        self.apply_settings(now, config.as_ref());
        if matches!(self.setup.notifier, Notifier::DryRun) {
            return Err(SendError::NotConfigured(
                self.setup
                    .config_error
                    .clone()
                    .unwrap_or_else(|| "notifications are turned off".to_owned()),
            ));
        }
        if self
            .last_manual_send_at
            .is_some_and(|at| now - at < MANUAL_SEND_SPACING)
        {
            return Err(SendError::RateLimited);
        }
        self.last_manual_send_at = Some(now);
        Ok(())
    }

    async fn send_test(&mut self) -> Result<(), SendError> {
        let now = Utc::now();
        self.manual_send_allowed(now).await?;
        let station = self
            .setup
            .station_name
            .as_deref()
            .map(|name| format!(" {name}"))
            .unwrap_or_default();
        self.setup
            .notifier
            .publish(
                &format!("Optic{station}: test notification"),
                &format!(
                    "Sent from the Config page at {}. Notifications are working.",
                    local_time(now, self.tz)
                ),
                3,
                "test_tube",
            )
            .await
            .map_err(SendError::Failed)
    }

    async fn send_digest_now(&mut self) -> Result<(), SendError> {
        let now = Utc::now();
        self.manual_send_allowed(now).await?;
        let Some((observation, extras)) = self.last_observation.clone() else {
            return Err(SendError::Failed(
                "no data gathered yet; try again in a few seconds".to_owned(),
            ));
        };
        let digest = self
            .build_digest(now - Duration::hours(24), now, now, &observation, &extras)
            .await;
        self.setup
            .notifier
            .publish(&digest.title, &digest.message, 2, "bar_chart")
            .await
            .map_err(SendError::Failed)
    }

    async fn persist(&mut self) {
        if !self.dirty {
            return;
        }
        let state = self.digest_state.clone();
        let path = self.sources.digest_state_path.clone();
        match tokio::task::spawn_blocking(move || state.save(&path)).await {
            Ok(Ok(())) => self.dirty = false,
            Ok(Err(error)) => tracing::warn!(%error, "could not save the digest state; will retry"),
            Err(error) => tracing::warn!(%error, "digest state save task failed"),
        }
    }

    fn fill_status(&self, status: &mut AlertsStatus) {
        let now = Utc::now();
        status.channel = self.setup.notifier.channel();
        status.settings_source = self.source;
        status.config_error = self.setup.config_error.clone();
        status.config_warnings = self.setup.config_warnings.clone();
        status.last_evaluated_at = self.monitor.last_evaluated_at;
        status.conditions = self.monitor.condition_statuses();
        status.active_count = status
            .conditions
            .iter()
            .filter(|c| matches!(c.state, "firing" | "recovering"))
            .count();
        status.outbox_len = self.outbox.queue.len();
        status.last_delivered_at = self.delivery.last_delivered_at;
        status.last_delivery_error = self.delivery.last_error.clone();
        status.thresholds = self.setup.thresholds.clone();
        status.digest = DigestStatus {
            enabled: self.setup.digest_send_at.is_some(),
            send_at: self
                .setup
                .digest_send_at
                .map(|at| at.format("%H:%M").to_string()),
            timezone: self.tz.name().to_owned(),
            next_due_at: self.setup.digest_send_at.map(|send_at| {
                optic_digest::next_due(
                    optic_digest::due_at(now, self.tz, send_at),
                    self.tz,
                    send_at,
                )
            }),
            last_window_end: self.digest_state.last_digest_due,
            last_sent_at: self.digest.last_sent_at,
            pending: self.digest.pending.is_some(),
            last_error: self.digest.last_error.clone(),
        };
        let plan = self.setup.heartbeat.as_ref();
        let state = self.heartbeat_state();
        let last_checkin = self.digest_state.heartbeat_last_checkin;
        status.heartbeat = HeartbeatStatus {
            state,
            interval_secs: plan.map(|plan| plan.interval.num_seconds() as u64),
            alert_after_secs: plan.map(|plan| plan.alert_after.num_seconds() as u64),
            last_checkin_at: last_checkin,
            next_checkin_at: match (state, plan) {
                ("ok" | "failing", Some(plan)) => {
                    Some(last_checkin.map_or(now, |at| at + plan.interval))
                }
                _ => None,
            },
            withheld_reason: self.heartbeat.withheld_reason.clone(),
            last_error: self.heartbeat.last_error.clone(),
        };
    }
}

async fn gather(
    sources: &AlertSources,
    tracker: &mut OutcomeTracker,
    thresholds: &Thresholds,
    schedule: ScheduleConfig,
    now: DateTime<Utc>,
) -> (Observation, Extras) {
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
    let sync = sources.sync.status();
    let system = sources.system_status.snapshot().await;
    let disk = |label: &str| {
        system
            .disks
            .iter()
            .find(|disk| disk.label == label)
            .map(|disk| (disk.total_bytes, disk.available_bytes))
    };
    let observation = Observation {
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
            last_error: sync.last_error.clone(),
        }),
        capture_disk: disk("capture"),
        cpu_temp_celsius: system.cpu_temp_celsius,
        throttled: read_throttled().await,
    };
    let extras = Extras {
        state_disk: disk("state"),
        uptime_secs: system.uptime_seconds,
        sync,
    };
    (observation, extras)
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

    fn curl_config(
        ntfy: &NtfyConfig,
        title: &str,
        message: &str,
        transition: Transition,
    ) -> String {
        let (priority, tag) = transition_style(transition);
        publish_curl_config(ntfy, title, message, priority, tag)
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
            settings_source: "config",
            digest: DigestStatus::default(),
            heartbeat: HeartbeatStatus::default(),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"channel\":\"ntfy\""));
        assert!(json.contains("\"condition\":\"capture_stalled\""));
        assert!(!json.contains("secret-topic") && !json.contains("tk_secret"));
    }

    // --- notification settings in config.json ------------------------------

    const SECRET_TOPIC: &str = "optic-secret-topic-a7f3";
    const SECRET_TOKEN: &str = "tk_supersecret";

    fn settings_json() -> String {
        format!(
            r#"{{"station_name": "optic",
                "ntfy": {{"topic": "{SECRET_TOPIC}", "token": "{SECRET_TOKEN}"}},
                "digest": {{"send_at": "07:30"}},
                "heartbeat": {{"interval_secs": 300, "alert_after_secs": 1200}},
                "thresholds": {{"sync_backlog_after_secs": 600}}}}"#
        )
    }

    #[test]
    fn digest_and_heartbeat_sections_resolve_and_the_heartbeat_is_opt_in() {
        let setup = parse_config(&settings_json());
        assert_eq!(setup.config_error, None);
        assert_eq!(setup.digest_send_at, NaiveTime::from_hms_opt(7, 30, 0));
        let plan = setup.heartbeat.unwrap();
        assert_eq!(plan.interval, Duration::minutes(5));
        assert_eq!(plan.alert_after, Duration::minutes(20));
        assert_eq!(plan.sequence_id, optic_heartbeat::DEFAULT_SEQUENCE_ID);

        let minimal = parse_config(r#"{"ntfy": {"topic": "x"}}"#);
        assert_eq!(minimal.digest_send_at, NaiveTime::from_hms_opt(8, 0, 0));
        assert!(minimal.heartbeat.is_none(), "no section = no heartbeat");
        let off = parse_config(
            r#"{"ntfy": {"topic": "x"}, "heartbeat": {"enabled": false}, "digest": {"enabled": false}}"#,
        );
        assert!(off.heartbeat.is_none() && off.digest_send_at.is_none());
    }

    #[test]
    fn out_of_range_values_on_disk_are_clamped_or_switched_off_with_warnings() {
        let setup = parse_config(
            r#"{"ntfy": {"topic": "x"}, "digest": {"send_at": "8am"},
                "heartbeat": {"interval_secs": 5, "alert_after_secs": 999999999}}"#,
        );
        assert_eq!(setup.config_error, None);
        assert!(setup.digest_send_at.is_none());
        let plan = setup.heartbeat.unwrap();
        assert_eq!(plan.interval, Duration::seconds(60));
        assert_eq!(plan.alert_after, Duration::days(3));
        assert!(
            setup
                .config_warnings
                .iter()
                .any(|w| w.contains("digest is off"))
        );
        assert!(setup.config_warnings.iter().any(|w| w.contains("clamped")));
        let bad_seq =
            parse_config(r#"{"ntfy": {"topic": "x"}, "heartbeat": {"sequence_id": "a b"}}"#);
        assert!(bad_seq.heartbeat.is_none());
        assert!(
            bad_seq
                .config_warnings
                .iter()
                .any(|w| w.contains("heartbeat is off"))
        );
    }

    fn valid_settings() -> NotificationSettings {
        serde_json::from_str(&settings_json()).unwrap()
    }

    #[test]
    fn save_validation_is_strict_and_rejects_masked_secrets() {
        assert_eq!(validate_for_save(&valid_settings()), Ok(()));
        type Mutation = Box<dyn Fn(&mut NotificationSettings)>;
        let cases: Vec<(Mutation, &str)> = vec![
            (Box::new(|s| s.ntfy = None), "topic is required"),
            (
                Box::new(|s| s.ntfy.as_mut().unwrap().topic = "opti****a7f3".to_owned()),
                "ntfy.topic",
            ),
            (
                Box::new(|s| s.ntfy.as_mut().unwrap().token = Some("tk_s****cret".to_owned())),
                "ntfy.token",
            ),
            (Box::new(|s| s.digest.send_at = "25:00".to_owned()), "HH:MM"),
            (
                Box::new(|s| s.heartbeat.as_mut().unwrap().interval_secs = 30),
                "interval",
            ),
            (
                Box::new(|s| s.heartbeat.as_mut().unwrap().alert_after_secs = 400),
                "at least the interval",
            ),
            (
                Box::new(|s| s.heartbeat.as_mut().unwrap().sequence_id = "x/y".to_owned()),
                "sequence ID",
            ),
            (
                Box::new(|s| s.station_name = Some("a\nb".to_owned())),
                "station name",
            ),
        ];
        for (mutate, needle) in cases {
            let mut settings = valid_settings();
            mutate(&mut settings);
            let error = validate_for_save(&settings).unwrap_err();
            assert!(error.contains(needle), "{needle}: {error}");
        }
        let mut disabled = valid_settings();
        disabled.enabled = false;
        disabled.ntfy = None;
        assert_eq!(
            validate_for_save(&disabled),
            Ok(()),
            "off without a topic is fine"
        );
    }

    #[test]
    fn secrets_show_only_4_plus_4_characters_and_short_ones_nothing() {
        assert_eq!(mask_secret("optic-abcdefghijkmnpqrstuvwx"), "opti****uvwx");
        assert_eq!(
            mask_secret("tk_abcdefghijklmnopqrstuvwxyz123"),
            "tk_a****z123"
        );
        assert_eq!(mask_secret("exactly16chars!!"), "exac****rs!!");
        assert_eq!(mask_secret("fifteen-chars-x"), "****");
        assert_eq!(mask_secret(""), "****");
        // Masks never validate as secrets.
        let masked = NtfyConfig {
            server: DEFAULT_NTFY_SERVER.to_owned(),
            topic: "opti****uvwx".to_owned(),
            token: None,
        };
        assert!(validate_ntfy(&masked).is_err());
        let masked_token = NtfyConfig {
            topic: "optic-x".to_owned(),
            token: Some("tk_a****z123".to_owned()),
            ..masked
        };
        assert!(validate_ntfy(&masked_token).is_err());
    }

    #[test]
    fn redaction_masks_topic_and_token_at_any_depth() {
        let mut value: Value = serde_json::from_str(&settings_json()).unwrap();
        value["misplaced"] =
            serde_json::json!({"topic": "short", "token": 42, "list": [{"token": "tk_x"}]});
        redact_notifications(&mut value);
        let text = value.to_string();
        assert!(
            !text.contains(SECRET_TOPIC) && !text.contains(SECRET_TOKEN),
            "{text}"
        );
        assert!(!text.contains("tk_x") && !text.contains("short"));
        assert_eq!(value["ntfy"]["topic"], "opti****a7f3");
        assert_eq!(
            value["ntfy"]["token"], "****",
            "14 characters: fully masked"
        );
        assert_eq!(value["station_name"], "optic");
        let mut no_token: Value = serde_json::json!({"ntfy": {"topic": "abc", "token": null}});
        redact_notifications(&mut no_token);
        assert_eq!(no_token["ntfy"]["token"], Value::Null);
        assert_eq!(no_token["ntfy"]["topic"], "****");
        // A masked value can never be saved back.
        let masked: NotificationSettings = serde_json::from_value(
            value
                .get("ntfy")
                .map(|ntfy| serde_json::json!({"ntfy": ntfy}))
                .unwrap(),
        )
        .unwrap();
        assert!(validate_for_save(&masked).is_err());
    }

    #[test]
    fn app_config_keeps_the_notifications_section_through_a_staging_round_trip() {
        // What every staging handler does: parse, change one field, re-serialize.
        let text = format!(
            r#"{{"save_dng": false, "notifications": {}}}"#,
            settings_json()
        );
        let mut config: crate::camera::AppConfig = serde_json::from_str(&text).unwrap();
        config.save_dng = true;
        let restaged: crate::camera::AppConfig =
            serde_json::from_str(&serde_json::to_string_pretty(&config).unwrap()).unwrap();
        let section: NotificationSettings =
            serde_json::from_value(restaged.notifications.unwrap()).unwrap();
        assert_eq!(section, valid_settings());
        // A malformed section never breaks the rest of the config.
        let broken: crate::camera::AppConfig =
            serde_json::from_str(r#"{"save_dng": true, "notifications": {"ntfy": 7}}"#).unwrap();
        assert!(broken.save_dng);
        // And a config without the section serializes without it.
        let plain = serde_json::to_string(&crate::camera::AppConfig::default()).unwrap();
        assert!(!plain.contains("notifications"));
    }

    fn update(topic: Option<&str>, token: Option<&str>, clear_token: bool) -> NotificationsUpdate {
        NotificationsUpdate {
            enabled: true,
            station_name: Some("  roof  ".to_owned()),
            server: Some("https://ntfy.example/".to_owned()),
            topic: topic.map(str::to_owned),
            token: token.map(str::to_owned),
            clear_token,
            digest: DigestSettings::default(),
            heartbeat: HeartbeatSettings::default(),
        }
    }

    #[test]
    fn merge_keeps_secrets_unless_replaced_or_cleared_and_always_keeps_thresholds() {
        let current = valid_settings();
        let kept = merge_update(Some(&current), update(Some(""), None, false));
        let ntfy = kept.ntfy.as_ref().unwrap();
        assert_eq!(
            (ntfy.topic.as_str(), ntfy.token.as_deref()),
            (SECRET_TOPIC, Some(SECRET_TOKEN))
        );
        assert_eq!(ntfy.server, "https://ntfy.example");
        assert_eq!(kept.station_name.as_deref(), Some("roof"));
        assert_eq!(kept.thresholds.sync_backlog_after_secs, 600);
        assert!(kept.heartbeat.is_some());

        let replaced = merge_update(
            Some(&current),
            update(Some(" optic-new "), Some("tk_new"), false),
        );
        let ntfy = replaced.ntfy.unwrap();
        assert_eq!(
            (ntfy.topic.as_str(), ntfy.token.as_deref()),
            ("optic-new", Some("tk_new"))
        );

        let cleared = merge_update(Some(&current), update(None, Some("ignored"), true));
        assert_eq!(cleared.ntfy.unwrap().token, None);

        let fresh = merge_update(None, update(None, None, false));
        assert!(fresh.ntfy.is_none());
        assert_eq!(fresh.thresholds, Thresholds::default());
    }

    fn config_paths(dir: &Path) -> ConfigPaths {
        ConfigPaths {
            config_path: dir.join("state/config.json"),
            config_cache_path: dir.join("cache/config.json"),
            preview_config_path: dir.join("capture/preview_config.json"),
        }
    }

    #[tokio::test]
    async fn saving_updates_committed_and_staged_config_and_preserves_everything_else() {
        let dir = temp_dir("save-staged");
        let paths = config_paths(&dir);
        let committed = r#"{"save_dng": true, "schedule": {"rules": []}}"#;
        durable_state::write_through(&paths.config_path, &paths.config_cache_path, committed)
            .await
            .unwrap();
        std::fs::create_dir_all(dir.join("capture")).unwrap();
        // A staged, uncommitted edit (save_dng off).
        std::fs::write(
            &paths.preview_config_path,
            r#"{"save_dng": false, "schedule": {"rules": []}}"#,
        )
        .unwrap();

        write_notifications(&paths, &valid_settings())
            .await
            .unwrap();
        let durable: Value =
            serde_json::from_str(&std::fs::read_to_string(&paths.config_path).unwrap()).unwrap();
        let cache = std::fs::read_to_string(&paths.config_cache_path).unwrap();
        assert_eq!(durable, serde_json::from_str::<Value>(&cache).unwrap());
        assert_eq!(durable["save_dng"], true);
        assert_eq!(durable["notifications"]["ntfy"]["topic"], SECRET_TOPIC);
        let staged: Value =
            serde_json::from_str(&std::fs::read_to_string(&paths.preview_config_path).unwrap())
                .unwrap();
        assert_eq!(staged["save_dng"], false, "the staged edit survives");
        assert_eq!(
            staged["notifications"]["ntfy"]["topic"], SECRET_TOPIC,
            "a later commit keeps the new settings"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn saving_with_an_unstaged_preview_leaves_nothing_staged() {
        let dir = temp_dir("save-unstaged");
        let paths = config_paths(&dir);
        let committed = r#"{"save_dng": true}"#;
        durable_state::write_through(&paths.config_path, &paths.config_cache_path, committed)
            .await
            .unwrap();
        std::fs::create_dir_all(dir.join("capture")).unwrap();
        std::fs::write(&paths.preview_config_path, committed).unwrap();
        write_notifications(&paths, &valid_settings())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&paths.preview_config_path).unwrap(),
            std::fs::read_to_string(&paths.config_cache_path).unwrap(),
            "byte-identical, so config_is_staged stays false"
        );

        // No preview at all: none is created.
        std::fs::remove_file(&paths.preview_config_path).unwrap();
        write_notifications(&paths, &valid_settings())
            .await
            .unwrap();
        assert!(!paths.preview_config_path.exists());

        // A committed config that is not JSON is never overwritten.
        durable_state::write_through(&paths.config_path, &paths.config_cache_path, "{broken")
            .await
            .unwrap();
        assert!(
            write_notifications(&paths, &valid_settings())
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(&paths.config_path).unwrap(),
            "{broken"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn legacy_file_is_imported_once_and_never_overwrites_config_json() {
        let dir = temp_dir("import");
        let paths = config_paths(&dir);
        durable_state::write_through(
            &paths.config_path,
            &paths.config_cache_path,
            r#"{"save_dng": true}"#,
        )
        .await
        .unwrap();
        let legacy = parse_config(&settings_json());
        import_legacy(&paths, &legacy).await;
        let config = read_committed(&paths.config_cache_path).await.unwrap();
        assert_eq!(config["save_dng"], true);
        let (source, settings) = effective_settings(Some(&config), &legacy);
        assert_eq!((source, settings.unwrap()), ("config", valid_settings()));

        // A second import with different legacy content changes nothing.
        let other = parse_config(r#"{"ntfy": {"topic": "other-topic"}}"#);
        import_legacy(&paths, &other).await;
        let again = read_committed(&paths.config_cache_path).await.unwrap();
        assert_eq!(again["notifications"]["ntfy"]["topic"], SECRET_TOPIC);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn effective_settings_prefer_config_json_then_the_legacy_file() {
        let legacy = parse_config(&settings_json());
        let empty = serde_json::json!({"save_dng": true});
        assert_eq!(effective_settings(Some(&empty), &legacy).0, "alerts_file");
        let with_section = serde_json::json!({"notifications": {"enabled": false}});
        let (source, settings) = effective_settings(Some(&with_section), &legacy);
        assert_eq!(source, "config");
        assert!(!settings.unwrap().enabled);
        let invalid = serde_json::json!({"notifications": {"ntfy": {"topic": "x", "tokn": "y"}}});
        let (source, settings) = effective_settings(Some(&invalid), &legacy);
        assert_eq!(source, "config");
        assert!(settings.unwrap_err().contains("config.json are invalid"));
        let none = load_config(Path::new("/nonexistent/optic/alerts.json"));
        let (source, settings) = effective_settings(None, &none);
        assert_eq!(source, "none");
        assert!(settings.unwrap_err().contains("not found"));
    }

    #[test]
    fn settings_view_never_contains_the_topic_or_token() {
        let view = NotificationsView::new("config", Ok(&valid_settings()));
        let json = serde_json::to_string(&view).unwrap();
        assert!(
            !json.contains(SECRET_TOPIC) && !json.contains(SECRET_TOKEN),
            "{json}"
        );
        assert!(view.topic_set && view.token_set);
        assert_eq!(view.topic_hint.as_deref(), Some("opti****a7f3"));
        assert_eq!(view.token_hint.as_deref(), Some("****"));
        assert!(view.heartbeat.enabled);
        let none = NotificationsView::new("none", Err("no settings"));
        assert!(!none.enabled && !none.topic_set && !none.heartbeat.enabled);
        assert_eq!(none.server, DEFAULT_NTFY_SERVER);
        assert_eq!(none.error.as_deref(), Some("no settings"));
    }

    #[test]
    fn active_titles_split_capture_path_conditions() {
        let mut monitor = Monitor::new(Thresholds::default(), t(0, 0, 0));
        let failing = |now| Observation {
            recent_outcomes: (0..3)
                .map(|i| outcome(now - Duration::seconds(i), false))
                .collect(),
            capture_disk: Some((100, 1)),
            ..running(now)
        };
        monitor.observe(&failing(t(0, 1, 0)));
        assert_eq!(monitor.active_titles(true), vec!["captures failing"]);
        assert_eq!(
            monitor.active_titles(false),
            vec!["captures failing", "capture tmpfs low"]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn heartbeat_arm_passes_the_token_on_stdin_only() {
        let dir = temp_dir("heartbeat-curl");
        let sender = NtfySender {
            config: ntfy(Some("tk_secret")),
            curl: fake_curl(&dir, 0),
        };
        let plan = HeartbeatPlan {
            interval: Duration::minutes(10),
            alert_after: Duration::minutes(30),
            sequence_id: "optic-heartbeat".to_owned(),
        };
        let (title, message) =
            optic_heartbeat::silent_alert(None, plan.alert_after, "Mon 14:05 PDT");
        sender
            .run(optic_heartbeat::arm_curl_config(
                &sender.config,
                &plan,
                &title,
                &message,
            ))
            .await
            .unwrap();
        let stdin = std::fs::read_to_string(dir.join("stdin.txt")).unwrap();
        assert!(stdin.contains("url = \"https://ntfy.sh/optic-test/optic-heartbeat\""));
        assert!(stdin.contains("X-Delay: 30m"));
        assert!(stdin.contains("Authorization: Bearer tk_secret"));
        let args = std::fs::read_to_string(dir.join("args.txt")).unwrap();
        assert!(
            !args.contains("tk_secret") && !args.contains("optic-test"),
            "{args}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
