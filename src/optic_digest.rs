//! Daily digest (`docs/optic-daemon-digest-heartbeat.md` §3).
//!
//! Pure half only: when a digest is due, what the daemon remembers between
//! digests (`DigestState`, persisted as `digest_state.json`), and how a
//! digest is built and rendered from gathered data. Everything takes an
//! explicit `now`, so it is unit-tested with a fake clock. The alerts actor
//! (`optic_alerts`) samples signals into `DigestState`, gathers the inputs,
//! and delivers the rendered message.

use std::path::Path;

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone as _, Timelike as _, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::optic_scheduler::{self, ScheduleConfig};

/// A digest is still sent this long after its due time (a daemon restart or
/// outage in the morning); later, it is skipped.
pub const CATCH_UP: Duration = Duration::hours(12);
/// Run-state changes, fired alerts and withheld check-ins older than this
/// are pruned (two windows, so a delayed digest still sees its own window).
const KEEP: Duration = Duration::hours(48);
/// Hourly sample buckets kept (a 25 h DST window plus the current hour).
const MAX_BUCKETS: usize = 26;
const MAX_FIRED_ALERTS: usize = 100;
const MAX_WITHHELD: usize = 200;

pub fn parse_send_at(text: &str) -> Option<NaiveTime> {
    let (hours, minutes) = text.trim().split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    NaiveTime::from_hms_opt(hours.parse().ok()?, minutes.parse().ok()?, 0)
}

// ---------------------------------------------------------------------------
// Schedule
// ---------------------------------------------------------------------------

/// `time` on `date` in `tz`, as UTC. The earlier instant on a DST overlap;
/// the first valid minute after a DST gap.
fn local_instant(date: NaiveDate, time: NaiveTime, tz: Tz) -> DateTime<Utc> {
    let naive = date.and_time(time);
    (0..=180)
        .find_map(|minutes| {
            tz.from_local_datetime(&(naive + Duration::minutes(minutes)))
                .earliest()
        })
        .map_or_else(
            || Utc.from_utc_datetime(&naive),
            |at| at.with_timezone(&Utc),
        )
}

/// The most recent `send_at` in `tz` at or before `now`.
pub fn due_at(now: DateTime<Utc>, tz: Tz, send_at: NaiveTime) -> DateTime<Utc> {
    let today = now.with_timezone(&tz).date_naive();
    let candidate = local_instant(today, send_at, tz);
    if candidate <= now {
        candidate
    } else {
        local_instant(today.pred_opt().unwrap_or(today), send_at, tz)
    }
}

/// The due time one local day before `due` (the start of its window).
pub fn previous_due(due: DateTime<Utc>, tz: Tz, send_at: NaiveTime) -> DateTime<Utc> {
    let date = due.with_timezone(&tz).date_naive();
    local_instant(date.pred_opt().unwrap_or(date), send_at, tz)
}

/// The due time one local day after `due`.
pub fn next_due(due: DateTime<Utc>, tz: Tz, send_at: NaiveTime) -> DateTime<Utc> {
    let date = due.with_timezone(&tz).date_naive();
    local_instant(date.succ_opt().unwrap_or(date), send_at, tz)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing to do this tick.
    Idle,
    /// No saved state yet: mark `due` as sent, so the first digest has a
    /// full window of samples.
    Initialize { due: DateTime<Utc> },
    /// Build and send the digest for `[start, end)`.
    Send {
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    },
    /// Too late (> `CATCH_UP` after `due`): mark it sent without sending.
    Skip { due: DateTime<Utc> },
}

pub fn decide(
    now: DateTime<Utc>,
    tz: Tz,
    send_at: NaiveTime,
    last_due: Option<DateTime<Utc>>,
) -> Decision {
    let due = due_at(now, tz, send_at);
    match last_due {
        None => Decision::Initialize { due },
        Some(last) if last >= due => Decision::Idle,
        Some(_) if now - due > CATCH_UP => Decision::Skip { due },
        Some(_) => Decision::Send {
            start: previous_due(due, tz, send_at),
            end: due,
        },
    }
}

// ---------------------------------------------------------------------------
// Persisted state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RunChange {
    pub at: DateTime<Utc>,
    pub running: bool,
}

/// One hour of samples. `hour` is the UTC start of the hour.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    pub hour: DateTime<Utc>,
    pub temp_min: Option<f32>,
    pub temp_max: Option<f32>,
    pub capture_free_min: Option<u64>,
    pub transferred_bytes: u64,
    pub transferred_files: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FiredAlert {
    pub condition: String,
    pub at: DateTime<Utc>,
}

/// One tick's worth of sampled signals.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    pub cpu_temp_celsius: Option<f32>,
    /// `(total, available)` bytes of the capture tmpfs.
    pub capture_disk: Option<(u64, u64)>,
    /// `SyncStatus.(transferred_files, transferred_bytes)`: counters since
    /// daemon start.
    pub transferred: Option<(u64, u64)>,
}

/// Everything the digest remembers between ticks and across restarts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DigestState {
    pub last_digest_due: Option<DateTime<Utc>>,
    pub run_changes: Vec<RunChange>,
    pub buckets: Vec<Bucket>,
    pub alerts_fired: Vec<FiredAlert>,
    pub heartbeat_last_checkin: Option<DateTime<Utc>>,
    pub heartbeat_withheld: Vec<DateTime<Utc>>,
    /// The sync counters at the previous sample. Not persisted: they restart
    /// with the daemon, and so does this.
    #[serde(skip)]
    last_transferred: Option<(u64, u64)>,
}

fn hour_start(at: DateTime<Utc>) -> DateTime<Utc> {
    at.with_nanosecond(0)
        .and_then(|at| at.with_second(0))
        .and_then(|at| at.with_minute(0))
        .unwrap_or(at)
}

fn min_opt<T: PartialOrd + Copy>(current: Option<T>, value: Option<T>) -> Option<T> {
    match (current, value) {
        (Some(a), Some(b)) => Some(if b < a { b } else { a }),
        (a, b) => a.or(b),
    }
}

fn max_opt<T: PartialOrd + Copy>(current: Option<T>, value: Option<T>) -> Option<T> {
    match (current, value) {
        (Some(a), Some(b)) => Some(if b > a { b } else { a }),
        (a, b) => a.or(b),
    }
}

impl DigestState {
    /// Reads the saved state; a missing or corrupt file gives fresh state
    /// (and a reason for the log when the file existed but was unusable).
    pub fn load(path: &Path) -> (Self, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(state) => (state, None),
                Err(error) => (
                    Self::default(),
                    Some(format!("digest state is corrupt ({error}); starting fresh")),
                ),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Self::default(), None),
            Err(error) => (
                Self::default(),
                Some(format!(
                    "digest state unreadable ({}); starting fresh",
                    error.kind()
                )),
            ),
        }
    }

    /// Atomic write (temp file + rename) next to `path`.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let content = serde_json::to_string(self).map_err(std::io::Error::other)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, content)?;
        std::fs::rename(temp, path)
    }

    /// Records the scheduler's run state; returns true if it changed.
    pub fn record_run_state(&mut self, now: DateTime<Utc>, running: bool) -> bool {
        if self
            .run_changes
            .last()
            .is_some_and(|last| last.running == running)
        {
            return false;
        }
        self.run_changes.push(RunChange { at: now, running });
        true
    }

    /// Adds one sample; returns true when a new hourly bucket was started
    /// (a good moment to persist).
    pub fn record_sample(&mut self, now: DateTime<Utc>, sample: Sample) -> bool {
        let hour = hour_start(now);
        let rolled = self.buckets.last().is_none_or(|last| last.hour != hour);
        if rolled {
            self.buckets.push(Bucket {
                hour,
                temp_min: None,
                temp_max: None,
                capture_free_min: None,
                transferred_bytes: 0,
                transferred_files: 0,
            });
            if self.buckets.len() > MAX_BUCKETS {
                let excess = self.buckets.len() - MAX_BUCKETS;
                self.buckets.drain(..excess);
            }
        }
        let (files_delta, bytes_delta) = match (self.last_transferred, sample.transferred) {
            (Some((prev_files, prev_bytes)), Some((files, bytes))) => {
                // A decrease means the daemon restarted and the counter
                // started again from zero.
                let files_delta = if files >= prev_files {
                    files - prev_files
                } else {
                    files
                };
                let bytes_delta = if bytes >= prev_bytes {
                    bytes - prev_bytes
                } else {
                    bytes
                };
                (files_delta, bytes_delta)
            }
            // First sample after a start: the counters began at zero when
            // this daemon started, which is (almost exactly) now.
            (None, Some((files, bytes))) => (files, bytes),
            _ => (0, 0),
        };
        if sample.transferred.is_some() {
            self.last_transferred = sample.transferred;
        }
        if let Some(bucket) = self.buckets.last_mut() {
            bucket.temp_min = min_opt(bucket.temp_min, sample.cpu_temp_celsius);
            bucket.temp_max = max_opt(bucket.temp_max, sample.cpu_temp_celsius);
            bucket.capture_free_min = min_opt(
                bucket.capture_free_min,
                sample.capture_disk.map(|(_, available)| available),
            );
            bucket.transferred_files += files_delta;
            bucket.transferred_bytes += bytes_delta;
        }
        rolled
    }

    pub fn record_alert(&mut self, now: DateTime<Utc>, condition: &str) {
        self.alerts_fired.push(FiredAlert {
            condition: condition.to_owned(),
            at: now,
        });
        if self.alerts_fired.len() > MAX_FIRED_ALERTS {
            self.alerts_fired.remove(0);
        }
    }

    pub fn record_withheld(&mut self, now: DateTime<Utc>) {
        self.heartbeat_withheld.push(now);
        if self.heartbeat_withheld.len() > MAX_WITHHELD {
            self.heartbeat_withheld.remove(0);
        }
    }

    /// Drops history no digest will need again. Keeps the last run change
    /// before the cutoff, since it defines the state at the window start.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        let cutoff = now - KEEP;
        let keep_from = self
            .run_changes
            .iter()
            .rposition(|change| change.at <= cutoff)
            .unwrap_or(0);
        self.run_changes.drain(..keep_from);
        self.alerts_fired.retain(|alert| alert.at > cutoff);
        self.heartbeat_withheld.retain(|at| *at > cutoff);
    }
}

// ---------------------------------------------------------------------------
// Window arithmetic
// ---------------------------------------------------------------------------

/// A half-open `[start, end)` span of time.
pub type Span = (DateTime<Utc>, DateTime<Utc>);

/// Running segments of `[start, end)` and the time whose run state is
/// unknown (before the first recorded change). Between two changes the
/// earlier state holds, including while the daemon was down: the run state
/// is durable, so a restart does not change it.
pub fn running_segments(
    changes: &[RunChange],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> (Vec<Span>, Duration) {
    let mut segments = Vec::new();
    let mut unknown = Duration::zero();
    let mut cursor = start;
    let mut state = changes
        .iter()
        .rev()
        .find(|change| change.at <= start)
        .map(|change| change.running);
    for change in changes
        .iter()
        .filter(|change| change.at > start && change.at < end)
    {
        match state {
            Some(true) => segments.push((cursor, change.at)),
            None => unknown += change.at - cursor,
            Some(false) => {}
        }
        cursor = change.at;
        state = Some(change.running);
    }
    match state {
        Some(true) if cursor < end => segments.push((cursor, end)),
        None => unknown += end - cursor,
        _ => {}
    }
    (segments, unknown)
}

fn overlap(segments: &[Span], a: DateTime<Utc>, b: DateTime<Utc>) -> Duration {
    segments
        .iter()
        .map(|(start, end)| {
            let from = (*start).max(a);
            let to = (*end).min(b);
            if to > from {
                to - from
            } else {
                Duration::zero()
            }
        })
        .fold(Duration::zero(), |sum, part| sum + part)
}

/// Shots the schedule would have fired inside the running segments.
pub fn expected_shots(schedule: &ScheduleConfig, segments: &[Span]) -> u64 {
    segments
        .iter()
        .map(|(start, end)| {
            // `forecast` covers (from, from + horizon]; starting 1 s early
            // makes it [start, end] and the filter makes it [start, end), the
            // same half-open window the capture rows use.
            let from = *start - Duration::seconds(1);
            optic_scheduler::forecast(schedule, from, *end - from)
                .into_iter()
                .filter(|shot| shot.at.with_timezone(&Utc) < *end)
                .count() as u64
        })
        .sum()
}

/// The longest running time between consecutive successful frames, with
/// the pair of frames. Paused time inside a gap does not count.
pub fn longest_running_gap(
    frames: &[DateTime<Utc>],
    segments: &[Span],
) -> Option<(Duration, DateTime<Utc>, DateTime<Utc>)> {
    frames
        .windows(2)
        .map(|pair| (overlap(segments, pair[0], pair[1]), pair[0], pair[1]))
        .max_by_key(|(gap, _, _)| *gap)
}

// ---------------------------------------------------------------------------
// Building and rendering
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct CaptureRow {
    pub at: DateTime<Utc>,
    pub scheduled: bool,
    pub success: bool,
    pub bytes: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventRow {
    pub at: DateTime<Utc>,
    pub kind: String,
    /// For `boot`: the previous boot ended without an orderly shutdown.
    pub unexpected: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyncSnapshot {
    pub enabled: bool,
    pub paused: bool,
    pub queued_files: u64,
    pub queued_bytes: u64,
    pub connectivity: String,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HeartbeatSnapshot {
    /// `ok`, `withheld`, `failing`, `dry_run`.
    pub state: String,
    pub last_checkin: Option<DateTime<Utc>>,
}

/// Everything one digest needs, gathered by the actor (or built by tests).
pub struct DigestInput<'a> {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub tz: Tz,
    pub station_name: Option<&'a str>,
    pub schedule: &'a ScheduleConfig,
    /// Capture rows with `start <= at < end`, any order.
    pub rows: &'a [CaptureRow],
    pub state: &'a DigestState,
    pub sync: Option<SyncSnapshot>,
    pub capture_disk: Option<(u64, u64)>,
    pub state_disk: Option<(u64, u64)>,
    /// Titles of alert conditions currently firing or recovering.
    pub active_alerts: Vec<String>,
    pub events: &'a [EventRow],
    /// `None` when the heartbeat is disabled.
    pub heartbeat: Option<HeartbeatSnapshot>,
    pub host_uptime_secs: Option<u64>,
    pub daemon_started_at: DateTime<Utc>,
    pub version: &'a str,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Digest {
    pub title: String,
    pub message: String,
}

pub fn format_duration(duration: Duration) -> String {
    let total = duration.num_seconds().max(0);
    let (days, hours, minutes) = (total / 86_400, (total % 86_400) / 3600, (total % 3600) / 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else {
        format!("{:.1} MiB", bytes / MIB)
    }
}

pub fn build(input: &DigestInput<'_>) -> Digest {
    let tz = input.tz;
    let local =
        |at: DateTime<Utc>, pattern: &str| at.with_timezone(&tz).format(pattern).to_string();
    let window = input.end - input.start;
    let in_window = |at: DateTime<Utc>| at >= input.start && at < input.end;

    let (segments, unknown) = running_segments(&input.state.run_changes, input.start, input.end);
    let running = segments
        .iter()
        .fold(Duration::zero(), |sum, (a, b)| sum + (*b - *a));
    let paused = window - running - unknown;
    let expected = expected_shots(input.schedule, &segments);

    let mut frames: Vec<DateTime<Utc>> = input
        .rows
        .iter()
        .filter(|row| row.scheduled && row.success)
        .map(|row| row.at)
        .collect();
    frames.sort();
    let captured = frames.len() as u64;
    let mut failures: Vec<&CaptureRow> = input
        .rows
        .iter()
        .filter(|row| row.scheduled && !row.success)
        .collect();
    failures.sort_by_key(|row| row.at);
    let manual = input.rows.iter().filter(|row| !row.scheduled).count();
    let bytes_captured: u64 = input.rows.iter().map(|row| row.bytes).sum();

    let mut lines = vec![format!(
        "{} → {}",
        local(input.start, "%a %d %b %H:%M"),
        local(input.end, "%a %d %b %H:%M %Z")
    )];

    let frames_line = if expected == 0 && running.is_zero() && unknown.is_zero() {
        format!("Frames: {captured} (scheduler paused all day, 0 expected)")
    } else if expected == 0 {
        format!("Frames: {captured} of 0 expected")
    } else {
        let percent = captured as f64 * 100.0 / expected as f64;
        format!(
            "Frames: {captured} of {expected} expected ({percent:.0}%), {} missed",
            expected.saturating_sub(captured)
        )
    };
    lines.push(frames_line);

    let gap = match longest_running_gap(&frames, &segments) {
        Some((gap, from, to)) => format!(
            "{} ({} → {})",
            format_duration(gap),
            local(from, "%H:%M"),
            local(to, "%H:%M")
        ),
        None => "n/a".to_owned(),
    };
    let mut paused_line = format!("Paused: {} · Longest gap: {gap}", format_duration(paused));
    if !unknown.is_zero() {
        paused_line.push_str(&format!(
            " · run state unknown for {}",
            format_duration(unknown)
        ));
    }
    lines.push(paused_line);

    lines.push(match failures.last() {
        Some(last) => format!(
            "Failures: {} (last: {})",
            failures.len(),
            last.error.as_deref().unwrap_or("unknown error")
        ),
        None => "Failures: 0".to_owned(),
    });
    lines.push(format!(
        "Manual captures: {manual} · Captured {}",
        format_bytes(bytes_captured)
    ));

    let buckets: Vec<&Bucket> = input
        .state
        .buckets
        .iter()
        .filter(|bucket| bucket.hour + Duration::hours(1) > input.start && bucket.hour < input.end)
        .collect();
    let transferred_files: u64 = buckets.iter().map(|bucket| bucket.transferred_files).sum();
    let transferred_bytes: u64 = buckets.iter().map(|bucket| bucket.transferred_bytes).sum();
    let queue = match &input.sync {
        Some(sync) if !sync.enabled => "sync disabled".to_owned(),
        Some(sync) => {
            let mut text = format!(
                "Queue: {} files, {} ({}{})",
                sync.queued_files,
                format_bytes(sync.queued_bytes),
                sync.connectivity,
                if sync.paused { ", paused" } else { "" }
            );
            if let Some(error) = &sync.last_error {
                text.push_str(&format!(", last error: {error}"));
            }
            text
        }
        None => "Queue: n/a".to_owned(),
    };
    lines.push(format!(
        "Transferred: {transferred_files} files, {} · {queue}",
        format_bytes(transferred_bytes)
    ));

    let temp_min = buckets.iter().fold(None, |acc, b| min_opt(acc, b.temp_min));
    let temp_max = buckets.iter().fold(None, |acc, b| max_opt(acc, b.temp_max));
    lines.push(match (temp_min, temp_max) {
        (Some(low), Some(high)) => format!("CPU: {low:.1}–{high:.1} °C"),
        _ => "CPU: n/a".to_owned(),
    });

    let capture_free_min = buckets
        .iter()
        .fold(None, |acc, b| min_opt(acc, b.capture_free_min));
    let capture = match input.capture_disk {
        Some((total, available)) => {
            let mut text = format!(
                "capture {} / {}",
                format_bytes(available),
                format_bytes(total)
            );
            if let Some(low) = capture_free_min {
                text.push_str(&format!(" (min {})", format_bytes(low)));
            }
            text
        }
        None => "capture n/a".to_owned(),
    };
    let state_disk = input.state_disk.map_or_else(
        || "n/a".to_owned(),
        |(_, available)| format_bytes(available),
    );
    lines.push(format!("Free: {capture} · state {state_disk}"));

    let mut fired: Vec<(String, usize)> = Vec::new();
    for alert in input
        .state
        .alerts_fired
        .iter()
        .filter(|alert| in_window(alert.at))
    {
        match fired.iter_mut().find(|(name, _)| *name == alert.condition) {
            Some((_, count)) => *count += 1,
            None => fired.push((alert.condition.clone(), 1)),
        }
    }
    let fired_text = if fired.is_empty() {
        "none".to_owned()
    } else {
        fired
            .iter()
            .map(|(name, count)| format!("{name} ×{count}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let active_text = if input.active_alerts.is_empty() {
        "none".to_owned()
    } else {
        input.active_alerts.join(", ")
    };
    lines.push(format!(
        "Alerts fired: {fired_text} · active: {active_text}"
    ));

    let events: Vec<&EventRow> = input
        .events
        .iter()
        .filter(|event| in_window(event.at))
        .collect();
    let boots = events.iter().filter(|event| event.kind == "boot").count();
    let unexpected = events
        .iter()
        .filter(|event| event.kind == "boot" && event.unexpected)
        .count();
    let starts = events
        .iter()
        .filter(|event| event.kind == "daemon_start")
        .count();
    lines.push(format!(
        "System: {boots} boot{} ({unexpected} unexpected), {starts} daemon start{}",
        if boots == 1 { "" } else { "s" },
        if starts == 1 { "" } else { "s" }
    ));

    if let Some(heartbeat) = &input.heartbeat {
        let withheld = input
            .state
            .heartbeat_withheld
            .iter()
            .filter(|at| in_window(**at))
            .count();
        let last = heartbeat
            .last_checkin
            .map_or_else(|| "never".to_owned(), |at| local(at, "%H:%M"));
        lines.push(format!(
            "Heartbeat: {}, last check-in {last}, withheld {withheld}×",
            heartbeat.state
        ));
    }

    let host = input.host_uptime_secs.map_or_else(
        || "n/a".to_owned(),
        |secs| format_duration(Duration::seconds(secs as i64)),
    );
    lines.push(format!(
        "Uptime: Pi {host} · daemon {} · v{}",
        format_duration(input.now - input.daemon_started_at),
        input.version
    ));

    let covered = buckets.len() as i64;
    let window_hours = (window.num_minutes() + 59) / 60;
    if covered < window_hours {
        lines.push(format!(
            "(Temperature and transfer samples cover about {covered} of {window_hours} h.)"
        ));
    }

    let station = input
        .station_name
        .map(|name| format!(" {name}"))
        .unwrap_or_default();
    let summary = if running.is_zero() && unknown.is_zero() {
        "scheduler paused".to_owned()
    } else {
        format!("{captured}/{expected} frames")
    };
    Digest {
        title: format!("Optic{station}: daily digest {summary}"),
        message: lines.join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optic_scheduler::{Constraints, Rule, Trigger};

    fn utc(d: u32, h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, d, h, m, 0).unwrap()
    }

    fn vancouver() -> Tz {
        "America/Vancouver".parse().unwrap()
    }

    fn eight() -> NaiveTime {
        NaiveTime::from_hms_opt(8, 0, 0).unwrap()
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
            ..ScheduleConfig::default()
        }
    }

    #[test]
    fn send_at_parses_only_hh_mm() {
        assert_eq!(parse_send_at("08:00"), Some(eight()));
        assert_eq!(parse_send_at(" 23:59 "), NaiveTime::from_hms_opt(23, 59, 0));
        for bad in ["8:00", "24:00", "08:60", "08-00", "", "08:00:00"] {
            assert_eq!(parse_send_at(bad), None, "{bad}");
        }
    }

    #[test]
    fn due_is_the_most_recent_local_send_time() {
        // 08:00 PDT = 15:00 UTC.
        assert_eq!(due_at(utc(22, 15, 0), vancouver(), eight()), utc(22, 15, 0));
        assert_eq!(due_at(utc(22, 16, 0), vancouver(), eight()), utc(22, 15, 0));
        assert_eq!(
            due_at(utc(22, 14, 59), vancouver(), eight()),
            utc(21, 15, 0)
        );
        assert_eq!(
            due_at(utc(22, 14, 59), chrono_tz::UTC, eight()),
            utc(22, 8, 0)
        );
    }

    #[test]
    fn dst_changes_give_23_and_25_hour_windows() {
        // 2026-11-01: PDT → PST (fall back): window is 25 h.
        let due = due_at(
            Utc.with_ymd_and_hms(2026, 11, 1, 17, 0, 0).unwrap(),
            vancouver(),
            eight(),
        );
        assert_eq!(due, Utc.with_ymd_and_hms(2026, 11, 1, 16, 0, 0).unwrap());
        assert_eq!(
            due - previous_due(due, vancouver(), eight()),
            Duration::hours(25)
        );
        // 2026-03-08: PST → PDT (spring forward): 23 h.
        let due = due_at(
            Utc.with_ymd_and_hms(2026, 3, 8, 16, 0, 0).unwrap(),
            vancouver(),
            eight(),
        );
        assert_eq!(due, Utc.with_ymd_and_hms(2026, 3, 8, 15, 0, 0).unwrap());
        assert_eq!(
            due - previous_due(due, vancouver(), eight()),
            Duration::hours(23)
        );
        // A send time inside the spring-forward gap uses the first valid minute.
        let two_thirty = NaiveTime::from_hms_opt(2, 30, 0).unwrap();
        let gap_due = due_at(
            Utc.with_ymd_and_hms(2026, 3, 8, 12, 0, 0).unwrap(),
            vancouver(),
            two_thirty,
        );
        // 03:00 PDT = 10:00 UTC.
        assert_eq!(gap_due, Utc.with_ymd_and_hms(2026, 3, 8, 10, 0, 0).unwrap());
    }

    #[test]
    fn decide_sends_once_catches_up_within_12h_and_skips_later() {
        let tz = vancouver();
        let due = utc(22, 15, 0);
        assert_eq!(
            decide(utc(22, 16, 0), tz, eight(), None),
            Decision::Initialize { due }
        );
        assert_eq!(
            decide(utc(22, 15, 0), tz, eight(), Some(utc(21, 15, 0))),
            Decision::Send {
                start: utc(21, 15, 0),
                end: due
            }
        );
        assert_eq!(
            decide(utc(22, 15, 1), tz, eight(), Some(due)),
            Decision::Idle
        );
        assert_eq!(
            decide(utc(23, 3, 0), tz, eight(), Some(utc(21, 15, 0))),
            Decision::Send {
                start: utc(21, 15, 0),
                end: due
            }
        );
        assert_eq!(
            decide(utc(23, 3, 1), tz, eight(), Some(utc(21, 15, 0))),
            Decision::Skip { due }
        );
    }

    #[test]
    fn running_segments_use_the_state_before_the_window_and_mark_unknown_time() {
        let start = utc(21, 15, 0);
        let end = utc(22, 15, 0);
        let changes = vec![
            RunChange {
                at: utc(20, 0, 0),
                running: true,
            },
            RunChange {
                at: utc(21, 20, 0),
                running: false,
            },
            RunChange {
                at: utc(22, 0, 0),
                running: true,
            },
        ];
        let (segments, unknown) = running_segments(&changes, start, end);
        assert_eq!(
            segments,
            vec![(start, utc(21, 20, 0)), (utc(22, 0, 0), end)]
        );
        assert_eq!(unknown, Duration::zero());

        // Nothing known before the first change inside the window.
        let changes = vec![RunChange {
            at: utc(22, 3, 0),
            running: true,
        }];
        let (segments, unknown) = running_segments(&changes, start, end);
        assert_eq!(segments, vec![(utc(22, 3, 0), end)]);
        assert_eq!(unknown, utc(22, 3, 0) - start);

        // Paused all day.
        let changes = vec![RunChange {
            at: utc(1, 0, 0),
            running: false,
        }];
        assert_eq!(
            running_segments(&changes, start, end),
            (vec![], Duration::zero())
        );
    }

    #[test]
    fn expected_counts_only_running_time_and_downtime_between_equal_states_counts() {
        // Running since before the window, with no change recorded while the
        // daemon was down: the whole window is running.
        let changes = vec![RunChange {
            at: utc(20, 0, 0),
            running: true,
        }];
        let (segments, _) = running_segments(&changes, utc(21, 15, 0), utc(22, 15, 0));
        assert_eq!(expected_shots(&every_five_minutes(), &segments), 288);
        // Paused until 00:00 on the 22nd, then running.
        let (segments, _) = running_segments(
            &[
                RunChange {
                    at: utc(21, 0, 0),
                    running: false,
                },
                RunChange {
                    at: utc(22, 0, 0),
                    running: true,
                },
            ],
            utc(21, 15, 0),
            utc(22, 15, 0),
        );
        assert_eq!(segments, vec![(utc(22, 0, 0), utc(22, 15, 0))]);
        // 15 h at one shot per 5 min.
        assert_eq!(expected_shots(&every_five_minutes(), &segments), 180);
    }

    #[test]
    fn longest_gap_excludes_paused_time() {
        // Running 10:00–10:10 and 10:50–11:00 on the 21st.
        let segments = vec![
            (utc(21, 10, 0), utc(21, 10, 10)),
            (utc(21, 10, 50), utc(21, 11, 0)),
        ];
        let frames = vec![utc(21, 10, 5), utc(21, 10, 55), utc(21, 10, 58)];
        let (gap, from, to) = longest_running_gap(&frames, &segments).unwrap();
        assert_eq!(
            (gap, from, to),
            (Duration::minutes(10), utc(21, 10, 5), utc(21, 10, 55))
        );
        assert!(longest_running_gap(&frames[..1], &segments).is_none());
    }

    #[test]
    fn samples_fill_hourly_buckets_and_handle_counter_resets() {
        let mut state = DigestState::default();
        let sample = |temp: f32, free: u64, files: u64, bytes: u64| Sample {
            cpu_temp_celsius: Some(temp),
            capture_disk: Some((1000, free)),
            transferred: Some((files, bytes)),
        };
        assert!(state.record_sample(utc(21, 10, 10), sample(50.0, 900, 2, 200)));
        assert!(!state.record_sample(utc(21, 10, 20), sample(60.0, 800, 5, 500)));
        assert!(!state.record_sample(utc(21, 10, 30), Sample::default()));
        // Next hour, after a daemon restart: the counter starts again from zero.
        assert!(state.record_sample(utc(21, 11, 5), sample(40.0, 950, 1, 100)));
        assert_eq!(state.buckets.len(), 2);
        let first = &state.buckets[0];
        assert_eq!((first.temp_min, first.temp_max), (Some(50.0), Some(60.0)));
        assert_eq!(first.capture_free_min, Some(800));
        assert_eq!((first.transferred_files, first.transferred_bytes), (5, 500));
        assert_eq!(state.buckets[1].transferred_bytes, 100);
        // Bounded.
        for hour in 0..40 {
            state.record_sample(utc(23, 0, 0) + Duration::hours(hour), Sample::default());
        }
        assert_eq!(state.buckets.len(), MAX_BUCKETS);
    }

    #[test]
    fn run_state_changes_are_deduplicated_and_pruned_keeping_the_defining_change() {
        let mut state = DigestState::default();
        assert!(state.record_run_state(utc(18, 0, 0), true));
        assert!(!state.record_run_state(utc(18, 1, 0), true));
        assert!(state.record_run_state(utc(19, 0, 0), false));
        assert!(state.record_run_state(utc(22, 0, 0), true));
        state.record_alert(utc(18, 0, 0), "old");
        state.record_alert(utc(22, 0, 0), "new");
        state.record_withheld(utc(18, 0, 0));
        state.prune(utc(22, 12, 0));
        // 19:00 (paused) is the last change before the 48 h cutoff (20th 12:00).
        assert_eq!(state.run_changes.len(), 2);
        assert!(!state.run_changes[0].running);
        assert_eq!(state.alerts_fired.len(), 1);
        assert!(state.heartbeat_withheld.is_empty());
    }

    #[test]
    fn state_round_trips_and_corrupt_files_start_fresh() {
        let dir = std::env::temp_dir().join(format!("optic-digest-state-{}", std::process::id()));
        let path = dir.join("digest_state.json");
        let mut state = DigestState::default();
        state.record_run_state(utc(21, 0, 0), true);
        state.last_digest_due = Some(utc(21, 15, 0));
        state.save(&path).unwrap();
        let (loaded, warning) = DigestState::load(&path);
        assert_eq!(loaded, state);
        assert!(warning.is_none());
        std::fs::write(&path, "{not json").unwrap();
        let (loaded, warning) = DigestState::load(&path);
        assert_eq!(loaded, DigestState::default());
        assert!(warning.unwrap().contains("corrupt"));
        let (_, warning) = DigestState::load(&dir.join("missing.json"));
        assert!(warning.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn row(at: DateTime<Utc>, scheduled: bool, success: bool) -> CaptureRow {
        CaptureRow {
            at,
            scheduled,
            success,
            bytes: 10 * 1024 * 1024,
            error: (!success).then(|| "camera timeout".to_owned()),
        }
    }

    #[test]
    fn digest_renders_every_line_for_a_running_day() {
        let start = utc(21, 15, 0);
        let end = utc(22, 15, 0);
        let mut state = DigestState::default();
        state.record_run_state(utc(20, 0, 0), true);
        for hour in 0..24 {
            state.record_sample(
                start + Duration::hours(hour),
                Sample {
                    cpu_temp_celsius: Some(40.0 + hour as f32),
                    capture_disk: Some((256 << 20, (200 - hour as u64) << 20)),
                    transferred: Some((hour as u64 * 10, (hour as u64 * 100) << 20)),
                },
            );
        }
        state.record_alert(utc(22, 1, 0), "captures stalled");
        state.record_alert(utc(20, 1, 0), "outside window");
        state.record_withheld(utc(22, 1, 0));
        // Every scheduled shot captured except one hour 03:00–04:00 PDT.
        let schedule = every_five_minutes();
        let (segments, _) = running_segments(&state.run_changes, start, end);
        let from = start - Duration::seconds(1);
        let mut rows: Vec<CaptureRow> = optic_scheduler::forecast(&schedule, from, end - from)
            .into_iter()
            .map(|shot| shot.at.with_timezone(&Utc))
            .filter(|at| *at < end && !(*at >= utc(22, 10, 0) && *at < utc(22, 11, 0)))
            .map(|at| row(at, true, true))
            .collect();
        rows.push(row(utc(22, 10, 30), true, false));
        rows.push(row(utc(22, 2, 0), false, true));
        assert_eq!(expected_shots(&schedule, &segments), 288);
        let events = vec![
            EventRow {
                at: utc(22, 0, 0),
                kind: "boot".to_owned(),
                unexpected: true,
            },
            EventRow {
                at: utc(22, 0, 1),
                kind: "daemon_start".to_owned(),
                unexpected: false,
            },
            EventRow {
                at: utc(20, 0, 0),
                kind: "boot".to_owned(),
                unexpected: false,
            },
        ];
        let input = DigestInput {
            start,
            end,
            tz: vancouver(),
            station_name: Some("optic"),
            schedule: &schedule,
            rows: &rows,
            state: &state,
            sync: Some(SyncSnapshot {
                enabled: true,
                paused: false,
                queued_files: 0,
                queued_bytes: 0,
                connectivity: "online".to_owned(),
                last_error: None,
            }),
            capture_disk: Some((256 << 20, 230 << 20)),
            state_disk: Some((32 << 30, 20 << 30)),
            active_alerts: vec![],
            events: &events,
            heartbeat: Some(HeartbeatSnapshot {
                state: "ok".to_owned(),
                last_checkin: Some(utc(22, 14, 58)),
            }),
            host_uptime_secs: Some(3 * 86_400 + 4 * 3600),
            daemon_started_at: utc(21, 13, 0),
            version: "0.1.31",
            now: end,
        };
        let digest = build(&input);
        assert_eq!(digest.title, "Optic optic: daily digest 276/288 frames");
        let lines: Vec<&str> = digest.message.lines().collect();
        assert_eq!(lines[0], "Mon 21 Sep 08:00 → Tue 22 Sep 08:00 PDT");
        assert_eq!(lines[1], "Frames: 276 of 288 expected (96%), 12 missed");
        assert_eq!(lines[2], "Paused: 0m · Longest gap: 1h 5m (02:55 → 04:00)");
        assert_eq!(lines[3], "Failures: 1 (last: camera timeout)");
        assert_eq!(lines[4], "Manual captures: 1 · Captured 2.7 GiB");
        assert_eq!(
            lines[5],
            "Transferred: 230 files, 2.2 GiB · Queue: 0 files, 0.0 MiB (online)"
        );
        assert_eq!(lines[6], "CPU: 40.0–63.0 °C");
        assert_eq!(
            lines[7],
            "Free: capture 230.0 MiB / 256.0 MiB (min 177.0 MiB) · state 20.0 GiB"
        );
        assert_eq!(lines[8], "Alerts fired: captures stalled ×1 · active: none");
        assert_eq!(lines[9], "System: 1 boot (1 unexpected), 1 daemon start");
        assert_eq!(lines[10], "Heartbeat: ok, last check-in 07:58, withheld 1×");
        assert_eq!(lines[11], "Uptime: Pi 3d 4h · daemon 1d 2h · v0.1.31");
        assert_eq!(lines.len(), 12, "full sample coverage adds no note");
    }

    #[test]
    fn digest_for_a_paused_day_with_missing_signals() {
        let mut state = DigestState::default();
        state.record_run_state(utc(1, 0, 0), false);
        let schedule = every_five_minutes();
        let input = DigestInput {
            start: utc(21, 15, 0),
            end: utc(22, 15, 0),
            tz: chrono_tz::UTC,
            station_name: None,
            schedule: &schedule,
            rows: &[],
            state: &state,
            sync: None,
            capture_disk: None,
            state_disk: None,
            active_alerts: vec!["sync backlog".to_owned()],
            events: &[],
            heartbeat: None,
            host_uptime_secs: None,
            daemon_started_at: utc(22, 14, 0),
            version: "0.1.31",
            now: utc(22, 15, 0),
        };
        let digest = build(&input);
        assert_eq!(digest.title, "Optic: daily digest scheduler paused");
        let message = digest.message;
        assert!(
            message.contains("Frames: 0 (scheduler paused all day, 0 expected)"),
            "{message}"
        );
        assert!(
            message.contains("Paused: 1d 0h · Longest gap: n/a"),
            "{message}"
        );
        assert!(message.contains("Queue: n/a"));
        assert!(message.contains("CPU: n/a"));
        assert!(message.contains("Free: capture n/a · state n/a"));
        assert!(message.contains("active: sync backlog"));
        assert!(!message.contains("Heartbeat"));
        assert!(message.contains("samples cover about 0 of 24 h"));
    }
}
