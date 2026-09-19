//! Passive observer of every capture request `optic_web` (and, later,
//! `optic_scheduler`) issues to `optic_camera`: durably records what was
//! requested, whether it succeeded, and basic output metadata, so capture
//! history survives past a single `tracing` session. See
//! `docs/optic-daemon-capture-log.md` for the full design and
//! `worklogs/2026-09-18-capture-history-module.md` for the decisions behind
//! this scope.
//!
//! Two best-effort writes per capture — neither can affect whether the
//! capture itself succeeded, and a failure in either is logged and
//! swallowed rather than surfaced to the HTTP caller:
//! - a `<capture_id>.log.json` file dropped into the capture directory,
//!   picked up and transferred by `optic_sync` exactly like the JPEG/DNG
//!   files it's paired with;
//! - a row in a local SQLite database, for fast Pi-side dashboard queries
//!   that per-file scanning can't answer efficiently (date-range counts,
//!   duration trends).

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::camera::{CameraError, CameraSettings, CaptureFile, CaptureProfile, CaptureResult};

static LAST_FAILURE_SUFFIX: AtomicU64 = AtomicU64::new(0);

/// One capture's worth of what was requested and what happened, persisted
/// verbatim as both the `.log.json` file body and the SQLite row's
/// `detail_json` column.
#[derive(Debug, Serialize, Deserialize)]
pub struct CaptureLogEntry {
    pub capture_id: String,
    pub source: String,
    pub requested_at_unix_ms: u128,
    pub completed_at_unix_ms: u128,
    pub duration_ms: u128,
    pub profile: CaptureProfile,
    pub settings: CameraSettings,
    pub save_dng: bool,
    pub success: bool,
    pub error: Option<String>,
    pub files: Vec<CaptureFile>,
    pub bytes: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl CaptureLogEntry {
    /// Builds an entry from the same request fields and
    /// `Result<CaptureResult, CameraError>` the HTTP handler already has,
    /// without consuming it — `web.rs::capture()` still needs `outcome` to
    /// build its own response afterward.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        requested_at: SystemTime,
        completed_at: SystemTime,
        duration_ms: u128,
        profile: CaptureProfile,
        settings: CameraSettings,
        save_dng: bool,
        outcome: &Result<CaptureResult, CameraError>,
    ) -> Self {
        let capture_id = derive_capture_id(profile, outcome);
        let (success, error, files, bytes, width, height) = match outcome {
            Ok(result) => (
                true,
                None,
                result
                    .files
                    .iter()
                    .map(|file| CaptureFile {
                        filename: file.filename.clone(),
                        bytes: file.bytes,
                    })
                    .collect(),
                result.bytes,
                Some(result.width),
                Some(result.height),
            ),
            Err(error) => (false, Some(error.to_string()), Vec::new(), 0, None, None),
        };
        Self {
            capture_id,
            source: "web_ui".to_owned(),
            requested_at_unix_ms: unix_ms(requested_at),
            completed_at_unix_ms: unix_ms(completed_at),
            duration_ms,
            profile,
            settings,
            save_dng,
            success,
            error,
            files,
            bytes,
            width,
            height,
        }
    }
}

fn unix_ms(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// On success, shares the JPEG's own basename (matching design doc §3.1's
/// shared-basename convention) without needing to thread an ID down through
/// the actor into `native_camera.rs`. On failure there's no output file to
/// borrow a basename from, so a synthetic id is minted instead — a case the
/// original design doc example didn't illustrate; see the worklog's
/// "Decisions Made" §3 for why this is scoped this way.
fn derive_capture_id(
    profile: CaptureProfile,
    outcome: &Result<CaptureResult, CameraError>,
) -> String {
    match outcome {
        Ok(result) => result
            .files
            .first()
            .and_then(|file| file.filename.rsplit_once('.'))
            .map(|(basename, _extension)| basename.to_owned())
            .unwrap_or_else(|| failure_capture_id(profile)),
        Err(_) => failure_capture_id(profile),
    }
}

fn failure_capture_id(profile: CaptureProfile) -> String {
    let now = unix_ms(SystemTime::now()) as u64;
    let mut previous = LAST_FAILURE_SUFFIX.load(Ordering::Relaxed);
    let suffix = loop {
        let next = now.max(previous.saturating_add(1));
        match LAST_FAILURE_SUFFIX.compare_exchange_weak(
            previous,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break next,
            Err(current) => previous = current,
        }
    };
    format!("testshot-{}-failed-{suffix}", profile_slug(profile))
}

fn profile_slug(profile: CaptureProfile) -> &'static str {
    profile.spec().slug
}

/// Matches the `profile` values already used across the dashboard/API
/// (`master_archive`/`dci_4k`/`binning_2k`), kept separate from the
/// filename slug (`master-archive`/`4k-dci`/`2k-binning`) used for
/// `capture_id`/filenames.
fn profile_key(profile: CaptureProfile) -> &'static str {
    match profile {
        CaptureProfile::MasterArchive => "master_archive",
        CaptureProfile::Dci4k => "dci_4k",
        CaptureProfile::Binning2k => "binning_2k",
    }
}

/// The single entry point for capture history recording.
#[derive(Clone)]
pub struct CaptureLog {
    inner: Arc<Inner>,
}

struct Inner {
    capture_dir: PathBuf,
    db: Mutex<Connection>,
}

impl CaptureLog {
    /// Opens (creating if needed) the SQLite history database at `db_path`
    /// and prepares its schema. `capture_dir` is where paired `.log.json`
    /// files are written, alongside the JPEG/DNG files `optic_sync` already
    /// drains from there.
    pub fn open(db_path: &Path, capture_dir: PathBuf) -> rusqlite::Result<Self> {
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let db = Connection::open(db_path)?;
        // Crash-safety, not (only) a wear optimization: unattended
        // multi-month operation makes an abrupt power loss a realistic
        // event, and WAL substantially reduces the chance of a corrupted
        // database file from a write interrupted mid-transaction.
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS captures (
                capture_id TEXT PRIMARY KEY,
                captured_at INTEGER NOT NULL,
                source TEXT NOT NULL,
                profile TEXT NOT NULL,
                save_dng INTEGER NOT NULL,
                success INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                bytes_total INTEGER NOT NULL,
                error TEXT,
                detail_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_captures_captured_at ON captures(captured_at);
            CREATE INDEX IF NOT EXISTS idx_captures_success ON captures(success);",
        )?;
        Ok(Self {
            inner: Arc::new(Inner {
                capture_dir,
                db: Mutex::new(db),
            }),
        })
    }

    /// Records one completed capture. Both writes are best-effort; a
    /// failure here is logged and swallowed, never surfaced to the HTTP
    /// caller (see module docs).
    pub async fn record(&self, entry: CaptureLogEntry) {
        let json = match serde_json::to_string_pretty(&entry) {
            Ok(json) => json,
            Err(error) => {
                warn!(%error, capture_id = %entry.capture_id, "failed to serialize capture log entry");
                return;
            }
        };

        let log_path = self
            .inner
            .capture_dir
            .join(format!("{}.log.json", entry.capture_id));
        if let Err(error) = tokio::fs::write(&log_path, &json).await {
            warn!(%error, path = %log_path.display(), "failed to write capture log file");
        }

        let inner = self.inner.clone();
        let capture_id = entry.capture_id.clone();
        let captured_at = (entry.completed_at_unix_ms / 1000) as i64;
        let source = entry.source.clone();
        let profile = profile_key(entry.profile);
        let save_dng = entry.save_dng;
        let success = entry.success;
        let duration_ms = entry.duration_ms as i64;
        let bytes_total = entry.bytes as i64;
        let error_text = entry.error.clone();
        let detail_json = json;

        let outcome = tokio::task::spawn_blocking(move || {
            let connection = inner.db.lock().expect("capture log db mutex poisoned");
            connection.execute(
                "INSERT OR REPLACE INTO captures
                 (capture_id, captured_at, source, profile, save_dng, success, duration_ms, bytes_total, error, detail_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    capture_id,
                    captured_at,
                    source,
                    profile,
                    save_dng,
                    success,
                    duration_ms,
                    bytes_total,
                    error_text,
                    detail_json,
                ],
            )
        })
        .await;

        match outcome {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => warn!(%error, "failed to record capture history row"),
            Err(error) => warn!(%error, "capture history insert task panicked"),
        }
    }

    /// Rolls up capture history over the last `window_hours` for the
    /// dashboard's system-health panel — the first consumer of this
    /// module's SQLite data (see `docs/optic-daemon-capture-log.md` §6,
    /// which explicitly deferred dashboard UI). Best-effort: any query
    /// failure yields an all-empty/zero result rather than propagating an
    /// error, matching this module's existing "never affect the caller"
    /// posture.
    pub async fn recent_health(&self, window_hours: u32) -> CaptureHealthStatus {
        let inner = self.inner.clone();
        let cutoff = unix_ms(SystemTime::now()) as i64 / 1000 - i64::from(window_hours) * 3600;

        tokio::task::spawn_blocking(move || {
            let connection = inner.db.lock().expect("capture log db mutex poisoned");
            let (total, successful, average_duration_ms) = connection
                .query_row(
                    "SELECT COUNT(*), COALESCE(SUM(success), 0), AVG(duration_ms)
                     FROM captures WHERE captured_at >= ?1",
                    rusqlite::params![cutoff],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Option<f64>>(2)?,
                        ))
                    },
                )
                .unwrap_or((0, 0, None));

            let last = connection
                .query_row(
                    "SELECT captured_at, success FROM captures ORDER BY captured_at DESC LIMIT 1",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)),
                )
                .optional()
                .unwrap_or(None);

            CaptureHealthStatus {
                window_hours,
                total: total.max(0) as u64,
                successful: successful.max(0) as u64,
                last_capture_at_unix_ms: last.map(|(captured_at, _)| captured_at as u128 * 1000),
                last_capture_success: last.map(|(_, success)| success),
                average_duration_ms,
            }
        })
        .await
        .unwrap_or_else(|_| CaptureHealthStatus::empty(window_hours))
    }
}

/// Rollup of recent capture outcomes, for the system-health dashboard
/// panel. See `CaptureLog::recent_health`.
#[derive(Debug, Serialize)]
pub struct CaptureHealthStatus {
    pub window_hours: u32,
    pub total: u64,
    pub successful: u64,
    pub last_capture_at_unix_ms: Option<u128>,
    pub last_capture_success: Option<bool>,
    pub average_duration_ms: Option<f64>,
}

impl CaptureHealthStatus {
    pub fn empty(window_hours: u32) -> Self {
        Self {
            window_hours,
            total: 0,
            successful: 0,
            last_capture_at_unix_ms: None,
            last_capture_success: None,
            average_duration_ms: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64 as TestAtomicU64, Ordering as TestOrdering};

    use super::*;

    fn unique_temp_dir(label: &str) -> PathBuf {
        static COUNTER: TestAtomicU64 = TestAtomicU64::new(0);
        let id = COUNTER.fetch_add(1, TestOrdering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "optic-capture-log-test-{label}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn success_outcome(filename: &str) -> Result<CaptureResult, CameraError> {
        Ok(CaptureResult {
            profile: CaptureProfile::MasterArchive,
            files: vec![CaptureFile {
                filename: filename.to_owned(),
                bytes: 1234,
            }],
            bytes: 1234,
            width: 4056,
            height: 3040,
        })
    }

    #[test]
    fn capture_log_entry_round_trips_through_json() {
        let entry = CaptureLogEntry::new(
            SystemTime::now(),
            SystemTime::now(),
            250,
            CaptureProfile::Dci4k,
            CameraSettings::default(),
            false,
            &success_outcome("testshot-4k-dci-1000.jpg"),
        );
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: CaptureLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.capture_id, "testshot-4k-dci-1000");
        assert!(parsed.success);
        assert_eq!(parsed.duration_ms, 250);
        assert_eq!(parsed.bytes, 1234);
    }

    #[test]
    fn derive_capture_id_shares_the_jpeg_basename_on_success() {
        let id = derive_capture_id(
            CaptureProfile::MasterArchive,
            &success_outcome("testshot-master-archive-1789753359227.jpg"),
        );
        assert_eq!(id, "testshot-master-archive-1789753359227");
    }

    #[test]
    fn derive_capture_id_mints_a_synthetic_id_on_failure() {
        let id = derive_capture_id(CaptureProfile::Binning2k, &Err(CameraError::Timeout));
        assert!(id.starts_with("testshot-2k-binning-failed-"));
    }

    #[tokio::test]
    async fn record_writes_a_log_file_and_a_queryable_row_on_success() {
        let capture_dir = unique_temp_dir("success-capture-dir");
        let db_path = unique_temp_dir("success-db").join("history.db");
        let log = CaptureLog::open(&db_path, capture_dir.clone()).unwrap();

        let entry = CaptureLogEntry::new(
            SystemTime::now(),
            SystemTime::now(),
            842,
            CaptureProfile::MasterArchive,
            CameraSettings::default(),
            true,
            &success_outcome("testshot-master-archive-42.jpg"),
        );
        log.record(entry).await;

        let log_file = capture_dir.join("testshot-master-archive-42.log.json");
        assert!(log_file.exists());
        let contents = std::fs::read_to_string(&log_file).unwrap();
        assert!(contents.contains("\"success\": true"));

        let connection = Connection::open(&db_path).unwrap();
        let (profile, success, duration_ms): (String, bool, i64) = connection
            .query_row(
                "SELECT profile, success, duration_ms FROM captures WHERE capture_id = ?1",
                rusqlite::params!["testshot-master-archive-42"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(profile, "master_archive");
        assert!(success);
        assert_eq!(duration_ms, 842);

        std::fs::remove_dir_all(&capture_dir).ok();
    }

    #[tokio::test]
    async fn record_writes_a_history_row_for_a_failed_capture_with_no_output_files() {
        let capture_dir = unique_temp_dir("failure-capture-dir");
        let db_path = unique_temp_dir("failure-db").join("history.db");
        let log = CaptureLog::open(&db_path, capture_dir.clone()).unwrap();

        let entry = CaptureLogEntry::new(
            SystemTime::now(),
            SystemTime::now(),
            15,
            CaptureProfile::Dci4k,
            CameraSettings::default(),
            false,
            &Err(CameraError::Busy),
        );
        let capture_id = entry.capture_id.clone();
        log.record(entry).await;

        let log_file = capture_dir.join(format!("{capture_id}.log.json"));
        assert!(log_file.exists());
        let contents = std::fs::read_to_string(&log_file).unwrap();
        assert!(contents.contains("\"success\": false"));
        assert!(contents.contains("camera is busy"));

        let connection = Connection::open(&db_path).unwrap();
        let (success, error, bytes_total): (bool, Option<String>, i64) = connection
            .query_row(
                "SELECT success, error, bytes_total FROM captures WHERE capture_id = ?1",
                rusqlite::params![capture_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(!success);
        assert_eq!(error.as_deref(), Some("camera is busy"));
        assert_eq!(bytes_total, 0);

        std::fs::remove_dir_all(&capture_dir).ok();
    }

    #[test]
    fn date_range_and_average_duration_queries_work_against_the_schema() {
        let db_path = unique_temp_dir("query-db").join("history.db");
        let capture_dir = unique_temp_dir("query-capture-dir");
        let connection = CaptureLog::open(&db_path, capture_dir).unwrap();
        let connection = &connection.inner.db;
        let connection = connection.lock().unwrap();

        // Two captures on the same day, one on a different day.
        connection
            .execute(
                "INSERT INTO captures (capture_id, captured_at, source, profile, save_dng, success, duration_ms, bytes_total, error, detail_json)
                 VALUES ('a', 1758153600, 'web_ui', 'master_archive', 1, 1, 1000, 5000, NULL, '{}')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO captures (capture_id, captured_at, source, profile, save_dng, success, duration_ms, bytes_total, error, detail_json)
                 VALUES ('b', 1758160800, 'web_ui', 'master_archive', 1, 1, 2000, 5000, NULL, '{}')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO captures (capture_id, captured_at, source, profile, save_dng, success, duration_ms, bytes_total, error, detail_json)
                 VALUES ('c', 1758240000, 'web_ui', 'master_archive', 1, 1, 3000, 5000, NULL, '{}')",
                [],
            )
            .unwrap();

        let count_on_day: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM captures WHERE captured_at >= 1758153600 AND captured_at < 1758240000",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count_on_day, 2);

        let average_duration: f64 = connection
            .query_row("SELECT AVG(duration_ms) FROM captures", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!((average_duration - 2000.0).abs() < 0.001);
    }

    #[tokio::test]
    async fn recent_health_excludes_rows_outside_the_window_and_computes_a_rollup() {
        let db_path = unique_temp_dir("health-db").join("history.db");
        let capture_dir = unique_temp_dir("health-capture-dir");
        let log = CaptureLog::open(&db_path, capture_dir).unwrap();

        let now = (unix_ms(SystemTime::now()) / 1000) as i64;
        let within_window = now - 3600; // 1h ago
        let outside_window = now - 30 * 3600; // 30h ago, outside a 24h window

        {
            let connection = log.inner.db.lock().unwrap();
            connection.execute(
                "INSERT INTO captures (capture_id, captured_at, source, profile, save_dng, success, duration_ms, bytes_total, error, detail_json)
                 VALUES ('recent-ok', ?1, 'web_ui', 'master_archive', 1, 1, 1000, 5000, NULL, '{}')",
                rusqlite::params![within_window],
            ).unwrap();
            connection.execute(
                "INSERT INTO captures (capture_id, captured_at, source, profile, save_dng, success, duration_ms, bytes_total, error, detail_json)
                 VALUES ('recent-failed', ?1, 'web_ui', 'master_archive', 1, 0, 2000, 0, 'boom', '{}')",
                rusqlite::params![within_window + 60],
            ).unwrap();
            connection.execute(
                "INSERT INTO captures (capture_id, captured_at, source, profile, save_dng, success, duration_ms, bytes_total, error, detail_json)
                 VALUES ('too-old', ?1, 'web_ui', 'master_archive', 1, 1, 9999, 5000, NULL, '{}')",
                rusqlite::params![outside_window],
            ).unwrap();
        }

        let health = log.recent_health(24).await;
        assert_eq!(health.window_hours, 24);
        assert_eq!(health.total, 2, "the 30h-old row must be excluded");
        assert_eq!(health.successful, 1);
        assert_eq!(health.average_duration_ms, Some(1500.0));
        assert_eq!(
            health.last_capture_at_unix_ms,
            Some((within_window + 60) as u128 * 1000)
        );
        assert_eq!(health.last_capture_success, Some(false));
    }

    #[tokio::test]
    async fn recent_health_is_all_empty_when_the_database_has_no_rows() {
        let db_path = unique_temp_dir("empty-health-db").join("history.db");
        let capture_dir = unique_temp_dir("empty-health-capture-dir");
        let log = CaptureLog::open(&db_path, capture_dir).unwrap();

        let health = log.recent_health(24).await;
        assert_eq!(health.total, 0);
        assert_eq!(health.successful, 0);
        assert_eq!(health.last_capture_at_unix_ms, None);
        assert_eq!(health.average_duration_ms, None);
    }
}
