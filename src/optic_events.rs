//! System event log: host boots, reboots and shutdowns, daemon starts and
//! stops, and the dashboard's Power/Restart requests. Shown on the Capture
//! History page (`GET /api/events`). See `docs/optic-daemon-system-events.md`
//! and `worklogs/2026-09-21-system-events-log.md`.
//!
//! Like the capture log, every write is best-effort: a failure is logged and
//! swallowed, never surfaced to the HTTP caller or allowed to hold up a
//! shutdown. It uses its own `events.db` rather than a table in
//! `history.db`, so its writes never contend with capture inserts (that
//! connection has no busy timeout).

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::process::Command;
use tracing::warn;

/// Oldest rows beyond this are pruned at startup. A few events a day for
/// years fits comfortably.
const MAX_EVENTS: i64 = 5000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemEventKind {
    Boot,
    DaemonStart,
    DaemonStop,
    Reboot,
    Shutdown,
    RebootRequested,
    ShutdownRequested,
    DaemonRestartRequested,
    RequestFailed,
}

impl SystemEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::DaemonStart => "daemon_start",
            Self::DaemonStop => "daemon_stop",
            Self::Reboot => "reboot",
            Self::Shutdown => "shutdown",
            Self::RebootRequested => "reboot_requested",
            Self::ShutdownRequested => "shutdown_requested",
            Self::DaemonRestartRequested => "daemon_restart_requested",
            Self::RequestFailed => "request_failed",
        }
    }
}

/// The current boot, from the kernel. `None` off Linux (development Macs),
/// where boot detection is skipped.
#[derive(Clone, Debug)]
pub struct BootInfo {
    pub boot_id: String,
    pub booted_at_unix_ms: i64,
}

impl BootInfo {
    pub fn read_host() -> Option<Self> {
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let btime: i64 = stat
            .lines()
            .find_map(|line| line.strip_prefix("btime "))?
            .trim()
            .parse()
            .ok()?;
        Some(Self {
            boot_id: boot_id.trim().to_owned(),
            booted_at_unix_ms: btime * 1000,
        })
    }
}

#[derive(Debug, Serialize)]
pub struct SystemEvent {
    pub id: i64,
    pub occurred_at_unix_ms: i64,
    pub kind: String,
    pub boot_id: String,
    pub detail: Value,
}

#[derive(Clone)]
pub struct SystemEventLog {
    db: Arc<Mutex<Connection>>,
    boot: Option<BootInfo>,
}

impl SystemEventLog {
    pub fn open(db_path: &Path, boot: Option<BootInfo>) -> rusqlite::Result<Self> {
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let db = Connection::open(db_path)?;
        // Same reason as history.db: survive an abrupt power loss.
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS system_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                occurred_at_ms INTEGER NOT NULL,
                kind TEXT NOT NULL,
                boot_id TEXT NOT NULL,
                detail_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_system_events_occurred_at
                ON system_events(occurred_at_ms);",
        )?;
        db.execute(
            "DELETE FROM system_events
             WHERE id <= (SELECT MAX(id) FROM system_events) - ?1",
            [MAX_EVENTS],
        )?;
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            boot,
        })
    }

    /// Called once at daemon start: records a `boot` event if this boot has
    /// not been seen before, then `daemon_start`.
    pub async fn record_startup(&self, version: &str) {
        let db = self.db.clone();
        let boot = self.boot.clone();
        let now = unix_ms_now();
        let version = version.to_owned();
        let outcome = tokio::task::spawn_blocking(move || {
            let connection = db.lock().expect("system event db mutex poisoned");
            record_startup_blocking(&connection, boot.as_ref(), now, &version)
        })
        .await;
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => warn!(%error, "failed to record startup system events"),
            Err(error) => warn!(%error, "startup system event task panicked"),
        }
    }

    pub async fn record(&self, kind: SystemEventKind, detail: Value) {
        let db = self.db.clone();
        let boot_id = self.boot_id().to_owned();
        let now = unix_ms_now();
        let outcome = tokio::task::spawn_blocking(move || {
            let connection = db.lock().expect("system event db mutex poisoned");
            insert(&connection, now, kind, &boot_id, &detail)
        })
        .await;
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => warn!(%error, kind = kind.as_str(), "failed to record system event"),
            Err(error) => warn!(%error, "system event insert task panicked"),
        }
    }

    /// Newest first. Best-effort: a query failure yields an empty list.
    pub async fn list(&self, limit: u32) -> Vec<SystemEvent> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let connection = db.lock().expect("system event db mutex poisoned");
            list_blocking(&connection, limit).unwrap_or_else(|error| {
                warn!(%error, "failed to list system events");
                Vec::new()
            })
        })
        .await
        .unwrap_or_default()
    }

    fn boot_id(&self) -> &str {
        self.boot.as_ref().map_or("", |boot| boot.boot_id.as_str())
    }
}

fn record_startup_blocking(
    connection: &Connection,
    boot: Option<&BootInfo>,
    now: i64,
    version: &str,
) -> rusqlite::Result<()> {
    let boot_id = boot.map_or("", |boot| boot.boot_id.as_str());
    if let Some(boot) = boot {
        let seen: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM system_events WHERE kind = 'boot' AND boot_id = ?1)",
                [&boot.boot_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if !seen {
            // The previous boot's last event says how it ended. "Last" is
            // by insertion order, not timestamp: the clock can be wrong
            // early in a boot, before NTP syncs. A boot ID of '' is a row
            // written without boot info; it is not a real boot.
            let previous = connection
                .query_row(
                    "SELECT boot_id, kind, occurred_at_ms FROM system_events
                     WHERE boot_id != ?1 AND boot_id != ''
                     ORDER BY id DESC LIMIT 1",
                    [&boot.boot_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()?;
            let previous_boot = previous.map(|(id, kind, at)| {
                json!({
                    "boot_id": id,
                    "ended": previous_boot_ending(&kind),
                    "last_event": kind,
                    "last_event_at_unix_ms": at,
                })
            });
            insert(
                connection,
                boot.booted_at_unix_ms,
                SystemEventKind::Boot,
                &boot.boot_id,
                &json!({ "previous_boot": previous_boot }),
            )?;
        }
    }
    insert(
        connection,
        now,
        SystemEventKind::DaemonStart,
        boot_id,
        &json!({ "version": version }),
    )
}

/// How a boot ended, judged by its last recorded event. A request is
/// followed by the stop event when the shutdown goes normally; if only the
/// request made it to disk, it is still the best evidence of what happened.
fn previous_boot_ending(last_kind: &str) -> &'static str {
    match last_kind {
        "reboot" | "reboot_requested" => "reboot",
        "shutdown" | "shutdown_requested" => "shutdown",
        "daemon_stop" => "daemon_stopped",
        _ => "unexpected",
    }
}

fn insert(
    connection: &Connection,
    occurred_at_ms: i64,
    kind: SystemEventKind,
    boot_id: &str,
    detail: &Value,
) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO system_events (occurred_at_ms, kind, boot_id, detail_json)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![occurred_at_ms, kind.as_str(), boot_id, detail.to_string()],
    )?;
    Ok(())
}

fn list_blocking(connection: &Connection, limit: u32) -> rusqlite::Result<Vec<SystemEvent>> {
    let mut statement = connection.prepare(
        "SELECT id, occurred_at_ms, kind, boot_id, detail_json FROM system_events
         ORDER BY occurred_at_ms DESC, id DESC LIMIT ?1",
    )?;
    let rows = statement.query_map([i64::from(limit)], |row| {
        let detail: String = row.get(4)?;
        Ok(SystemEvent {
            id: row.get(0)?,
            occurred_at_unix_ms: row.get(1)?,
            kind: row.get(2)?,
            boot_id: row.get(3)?,
            detail: serde_json::from_str(&detail).unwrap_or(Value::Null),
        })
    })?;
    rows.collect()
}

/// What the host is doing when the daemon gets SIGTERM, from the system
/// manager's job queue (read-only over D-Bus, no PolicyKit action needed).
/// `None` when no shutdown job is queued, i.e. only the daemon is stopping,
/// or when the query fails or takes too long.
pub async fn host_shutdown_action() -> Option<(SystemEventKind, &'static str)> {
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        Command::new("systemctl")
            .args(["list-jobs", "--no-legend", "--plain"])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    parse_host_action(&String::from_utf8_lossy(&output.stdout))
}

fn parse_host_action(list_jobs: &str) -> Option<(SystemEventKind, &'static str)> {
    for line in list_jobs.lines() {
        for unit in line.split_whitespace() {
            match unit {
                "reboot.target" => return Some((SystemEventKind::Reboot, "reboot.target")),
                "kexec.target" => return Some((SystemEventKind::Reboot, "kexec.target")),
                "poweroff.target" => return Some((SystemEventKind::Shutdown, "poweroff.target")),
                "halt.target" => return Some((SystemEventKind::Shutdown, "halt.target")),
                _ => {}
            }
        }
    }
    None
}

fn unix_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    fn temp_db(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "optic-events-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("events.db")
    }

    fn boot(id: &str, at: i64) -> Option<BootInfo> {
        Some(BootInfo {
            boot_id: id.to_owned(),
            booted_at_unix_ms: at,
        })
    }

    async fn kinds(log: &SystemEventLog) -> Vec<String> {
        log.list(200).await.into_iter().map(|e| e.kind).collect()
    }

    #[tokio::test]
    async fn first_start_records_boot_without_previous_then_daemon_start() {
        let path = temp_db("first");
        let log = SystemEventLog::open(&path, boot("a", 1_000)).unwrap();
        log.record_startup("0.1.31").await;

        let events = log.list(200).await;
        assert_eq!(kinds(&log).await, ["daemon_start", "boot"]);
        let boot_event = &events[1];
        assert_eq!(boot_event.occurred_at_unix_ms, 1_000);
        assert_eq!(boot_event.boot_id, "a");
        assert!(boot_event.detail["previous_boot"].is_null());
        assert_eq!(events[0].detail["version"], "0.1.31");
    }

    #[tokio::test]
    async fn daemon_restart_in_the_same_boot_adds_no_boot_event() {
        let path = temp_db("restart");
        SystemEventLog::open(&path, boot("a", 1_000))
            .unwrap()
            .record_startup("v")
            .await;
        let log = SystemEventLog::open(&path, boot("a", 1_000)).unwrap();
        log.record_startup("v").await;
        assert_eq!(kinds(&log).await, ["daemon_start", "daemon_start", "boot"]);
    }

    async fn previous_ending_after(last: Option<SystemEventKind>) -> Value {
        let path = temp_db("ending");
        let first = SystemEventLog::open(&path, boot("a", 1_000)).unwrap();
        first.record_startup("v").await;
        if let Some(kind) = last {
            first.record(kind, json!({})).await;
        }
        // A later boot: its kernel boot time is after everything above.
        let second = SystemEventLog::open(&path, boot("b", unix_ms_now() + 60_000)).unwrap();
        second.record_startup("v").await;
        let events = second.list(200).await;
        let boot_b = events
            .iter()
            .find(|e| e.kind == "boot" && e.boot_id == "b")
            .expect("boot event for the new boot");
        boot_b.detail["previous_boot"].clone()
    }

    #[tokio::test]
    async fn new_boot_reports_how_the_previous_boot_ended() {
        let cases = [
            (Some(SystemEventKind::Reboot), "reboot"),
            (Some(SystemEventKind::Shutdown), "shutdown"),
            (Some(SystemEventKind::RebootRequested), "reboot"),
            (Some(SystemEventKind::ShutdownRequested), "shutdown"),
            (Some(SystemEventKind::DaemonStop), "daemon_stopped"),
            (None, "unexpected"),
        ];
        for (last, expected) in cases {
            let previous = previous_ending_after(last).await;
            assert_eq!(previous["ended"], expected, "last event {last:?}");
            assert_eq!(previous["boot_id"], "a");
        }
    }

    #[tokio::test]
    async fn without_boot_info_only_daemon_events_are_recorded() {
        let path = temp_db("noboot");
        let log = SystemEventLog::open(&path, None).unwrap();
        log.record_startup("v").await;
        log.record(SystemEventKind::DaemonStop, json!({})).await;
        assert_eq!(kinds(&log).await, ["daemon_stop", "daemon_start"]);
        // Rows without boot info are never mistaken for a previous boot.
        let later = SystemEventLog::open(&path, boot("b", unix_ms_now() + 60_000)).unwrap();
        later.record_startup("v").await;
        let events = later.list(200).await;
        let boot_b = events.iter().find(|e| e.kind == "boot").unwrap();
        assert!(boot_b.detail["previous_boot"].is_null());
    }

    #[tokio::test]
    async fn list_is_newest_first_and_limited() {
        let path = temp_db("list");
        let log = SystemEventLog::open(&path, boot("a", 1_000)).unwrap();
        log.record_startup("v").await;
        log.record(SystemEventKind::RebootRequested, json!({}))
            .await;
        log.record(SystemEventKind::Reboot, json!({})).await;
        assert_eq!(
            kinds(&log).await,
            ["reboot", "reboot_requested", "daemon_start", "boot"]
        );
        assert_eq!(log.list(2).await.len(), 2);
    }

    #[test]
    fn prune_keeps_the_newest_rows() {
        let path = temp_db("prune");
        {
            let log = SystemEventLog::open(&path, None).unwrap();
            let connection = log.db.lock().unwrap();
            for i in 0..(MAX_EVENTS + 10) {
                insert(&connection, i, SystemEventKind::DaemonStart, "", &json!({})).unwrap();
            }
        }
        let log = SystemEventLog::open(&path, None).unwrap();
        let connection = log.db.lock().unwrap();
        let (count, oldest): (i64, i64) = connection
            .query_row(
                "SELECT COUNT(*), MIN(occurred_at_ms) FROM system_events",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, MAX_EVENTS);
        assert_eq!(oldest, 10);
    }

    #[test]
    fn host_action_comes_from_queued_shutdown_targets() {
        let reboot = "1234 reboot.target start waiting\n1240 optic-daemon.service stop running\n";
        assert_eq!(
            parse_host_action(reboot),
            Some((SystemEventKind::Reboot, "reboot.target"))
        );
        assert_eq!(
            parse_host_action("77 kexec.target start waiting\n"),
            Some((SystemEventKind::Reboot, "kexec.target"))
        );
        assert_eq!(
            parse_host_action("9 poweroff.target start waiting\n"),
            Some((SystemEventKind::Shutdown, "poweroff.target"))
        );
        assert_eq!(
            parse_host_action("9 halt.target start waiting\n"),
            Some((SystemEventKind::Shutdown, "halt.target"))
        );
        assert_eq!(parse_host_action(""), None);
        assert_eq!(
            parse_host_action("51 apt-daily.service start running\n"),
            None
        );
    }
}
