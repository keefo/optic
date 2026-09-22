use std::{
    convert::Infallible,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Instant, SystemTime},
};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path as PathParam, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::{Bytes, BytesMut};
use serde::{Deserialize, Serialize};

use crate::{
    camera::{
        CameraError, CameraSettings, CaptureProfile, CaptureRequest, CaptureResult, PreviewFrame,
        StreamRequest,
    },
    durable_state,
    optic_alerts::{
        AlertsHandle, AlertsStatus, NotificationsUpdate, NotificationsView, SaveError, SendError,
    },
    optic_camera::{CameraBackendKind, OpticCamera},
    optic_capture_log::{CaptureHealthStatus, CaptureLog, CaptureLogEntry, CaptureQueryFilter},
    optic_events::{SystemEvent, SystemEventKind, SystemEventLog},
    optic_scheduler::{
        self, CelestialTarget, LunarEvent, MilkyWayEvent, SchedulerHandle, SchedulerStatus,
        SchedulerUnavailable, SolarEvent, Station,
    },
    optic_sync::{DataSyncManager, SyncStatus, SyncUnavailable},
    system_status::{self, SystemStatus, SystemStatusReader},
};

use crate::camera::AppConfig;

const SECURITY_POLICY: &str = "default-src 'self'; img-src 'self' blob: data:; script-src 'self'; style-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'";

#[derive(Clone)]
pub struct AppState {
    camera: OpticCamera,
    sync: DataSyncManager,
    capture_log: Option<CaptureLog>,
    system_status: SystemStatusReader,
    scheduler: SchedulerHandle,
    capture_dir: Arc<PathBuf>,
    /// Durable source of truth for the committed config — real, persistent
    /// storage (`~/.local/state/optic-daemon/config.json`), *not* the
    /// `/mnt/capture` tmpfs. Written rarely (only on commit); never read on
    /// a hot path. See design doc §2.1.
    config_path: Arc<PathBuf>,
    /// Fast tmpfs mirror of `config_path`, hydrated at startup and updated
    /// write-through on every commit (`durable_state`). Every frequent read
    /// (status polling, discard) uses this, never `config_path` directly.
    config_cache_path: Arc<PathBuf>,
    preview_config_path: Arc<PathBuf>,
    /// Durable `preview_state.json` plus its tmpfs mirror, the same
    /// write-through pair as the scheduler's run state; see `PreviewState`.
    preview_state_path: Arc<PathBuf>,
    preview_state_cache_path: Arc<PathBuf>,
    /// Serializes read-modify-write updates of the preview state.
    preview_state_lock: Arc<tokio::sync::Mutex<()>>,
    asset_dir: Arc<PathBuf>,
    sensor: Option<String>,
    started: Instant,
    /// Records the Power menu and Restart daemon requests; `None` when the
    /// event log could not be opened.
    events: Option<SystemEventLog>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        camera: OpticCamera,
        sync: DataSyncManager,
        capture_log: Option<CaptureLog>,
        system_status: SystemStatusReader,
        scheduler: SchedulerHandle,
        capture_dir: PathBuf,
        config_path: PathBuf,
        config_cache_path: PathBuf,
        preview_state_path: PathBuf,
        preview_state_cache_path: PathBuf,
        asset_dir: PathBuf,
        sensor: Option<String>,
    ) -> Self {
        let preview_config_path = capture_dir.join("preview_config.json");
        Self {
            camera,
            sync,
            capture_log,
            system_status,
            scheduler,
            capture_dir: Arc::new(capture_dir),
            config_path: Arc::new(config_path),
            config_cache_path: Arc::new(config_cache_path),
            preview_config_path: Arc::new(preview_config_path),
            preview_state_path: Arc::new(preview_state_path),
            preview_state_cache_path: Arc::new(preview_state_cache_path),
            preview_state_lock: Arc::new(tokio::sync::Mutex::new(())),
            asset_dir: Arc::new(asset_dir),
            sensor,
            started: Instant::now(),
            events: None,
        }
    }

    pub fn with_events(mut self, events: Option<SystemEventLog>) -> Self {
        self.events = events;
        self
    }

    async fn record_event(&self, kind: SystemEventKind, detail: serde_json::Value) {
        if let Some(events) = &self.events {
            events.record(kind, detail).await;
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/{*asset}", get(asset))
        .route("/healthz", get(health))
        .route("/api/status", get(status))
        .route("/api/stream/start", post(start_stream))
        .route("/api/stream/reconfigure", post(reconfigure_stream))
        .route("/api/stream/stop", post(stop_stream))
        .route("/api/stream/mjpeg", get(mjpeg_stream))
        .route("/api/preview", post(update_preview_state))
        .route("/api/capture", post(capture))
        .route("/api/config/commit", post(commit_config))
        .route("/api/config/discard", post(discard_config))
        .route("/api/sync/pause", post(sync_pause))
        .route("/api/sync/resume", post(sync_resume))
        .route("/api/sync/retry-now", post(sync_retry_now))
        .route("/api/schedule/pause", post(schedule_pause))
        .route("/api/schedule/resume", post(schedule_resume))
        .route("/api/schedule/preview", post(schedule_preview))
        .route("/api/config/save-dng", post(stage_save_dng))
        .route("/api/schedule/forecast", get(schedule_forecast))
        .route("/api/captures", get(capture_history))
        .route("/api/system/status", get(system_status_handler))
        .route("/api/system/reboot", post(system_reboot))
        .route("/api/system/shutdown", post(system_shutdown))
        .route("/api/system/restart-daemon", post(system_restart_daemon))
        .route("/api/system/ntp-sync", post(system_ntp_sync))
        .route("/api/system/timezone", post(system_set_timezone))
        .route("/api/timezones", get(list_timezones))
        .route("/api/celestial-preview", get(celestial_preview))
        .with_state(state)
}

/// Read-only health-alert state (`docs/optic-daemon-alerts.md` §8). Its own
/// small router, merged in `main.rs`, so `AppState` doesn't change.
/// Read-only system event history (`docs/optic-daemon-system-events.md`).
/// Its own small router, like `alerts_router`, so it can be tested without
/// a camera-backed `AppState`.
pub fn events_router(events: Option<SystemEventLog>) -> Router {
    Router::new()
        .route("/api/events", get(list_events))
        .with_state(events)
}

#[derive(Debug, serde::Deserialize)]
struct EventsParams {
    limit: Option<u32>,
}

#[derive(Serialize)]
struct EventsResponse {
    events: Vec<SystemEvent>,
}

async fn list_events(
    State(events): State<Option<SystemEventLog>>,
    Query(params): Query<EventsParams>,
) -> Result<Json<EventsResponse>, AppError> {
    let Some(events) = events else {
        return Err(AppError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "the system event log is unavailable on this daemon".to_owned(),
        });
    };
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    Ok(Json(EventsResponse {
        events: events.list(limit).await,
    }))
}

pub fn alerts_router(alerts: AlertsHandle) -> Router {
    Router::new()
        .route("/api/alerts", get(alerts_status))
        .route(
            "/api/notifications",
            get(notifications_settings).put(save_notifications_settings),
        )
        .route("/api/notifications/test", post(send_test_notification))
        .route("/api/notifications/digest-now", post(send_digest_now))
        .with_state(alerts)
}

async fn alerts_status(State(alerts): State<AlertsHandle>) -> Json<AlertsStatus> {
    Json(alerts.status())
}

/// Notification settings for the Config page
/// (`docs/optic-daemon-digest-heartbeat.md` §6.2). Never returns the ntfy
/// topic or token.
async fn notifications_settings(State(alerts): State<AlertsHandle>) -> Json<NotificationsView> {
    Json(alerts.settings_view().await)
}

async fn save_notifications_settings(
    State(alerts): State<AlertsHandle>,
    Json(update): Json<NotificationsUpdate>,
) -> Result<Json<NotificationsView>, AppError> {
    alerts
        .save_settings(update)
        .await
        .map(Json)
        .map_err(|error| match error {
            SaveError::Invalid(message) => AppError {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                message,
            },
            SaveError::Io(message) => AppError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("could not save notification settings: {message}"),
            },
        })
}

fn manual_send_response(
    result: Result<(), SendError>,
    sent: &'static str,
) -> Result<Json<Message>, AppError> {
    match result {
        Ok(()) => Ok(Json(Message::new(sent))),
        Err(SendError::NotConfigured(reason)) => Err(AppError {
            status: StatusCode::CONFLICT,
            message: reason,
        }),
        Err(SendError::RateLimited) => Err(AppError {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "wait a few seconds before sending again".to_owned(),
        }),
        Err(SendError::Failed(error)) => Err(AppError {
            status: StatusCode::BAD_GATEWAY,
            message: format!("delivery failed: {error}"),
        }),
    }
}

async fn send_test_notification(
    State(alerts): State<AlertsHandle>,
) -> Result<Json<Message>, AppError> {
    manual_send_response(alerts.send_test().await, "test notification sent")
}

async fn send_digest_now(State(alerts): State<AlertsHandle>) -> Result<Json<Message>, AppError> {
    manual_send_response(alerts.send_digest_now().await, "digest sent")
}

async fn index(State(state): State<AppState>) -> Response {
    serve_asset(&state.asset_dir, "index.html").await
}

async fn asset(State(state): State<AppState>, PathParam(requested): PathParam<String>) -> Response {
    serve_asset(&state.asset_dir, &requested).await
}

/// Loads any file under `asset_dir` from disk on every request, so dropping a
/// new file in that directory (or editing an existing one) is visible on
/// browser refresh without a rebuild or a route change. `requested` is
/// untrusted client input; `safe_asset_path` restricts it to plain,
/// non-parent path segments, and the resolved file is additionally confirmed
/// (after symlinks are followed) to still live inside `asset_dir` before it
/// is read, so no request can escape the asset directory. Failures are
/// logged with the resolved path and degrade to a plain-text error response
/// rather than a panic or blank page.
async fn serve_asset(asset_dir: &Path, requested: &str) -> Response {
    let Some(candidate) = safe_asset_path(asset_dir, requested) else {
        tracing::warn!(
            requested,
            "rejected a web asset request with an unsafe path"
        );
        return bad_asset_path_response();
    };

    let root = match tokio::fs::canonicalize(asset_dir).await {
        Ok(root) => root,
        Err(error) => {
            tracing::error!(
                asset_dir = %asset_dir.display(),
                %error,
                "web asset directory is unavailable"
            );
            return asset_unavailable_response(requested);
        }
    };

    let resolved = match tokio::fs::canonicalize(&candidate).await {
        Ok(resolved) => resolved,
        Err(error) => {
            tracing::error!(
                requested,
                path = %candidate.display(),
                %error,
                "failed to load web asset from disk"
            );
            return asset_unavailable_response(requested);
        }
    };

    if !resolved.starts_with(&root) {
        tracing::warn!(
            requested,
            resolved = %resolved.display(),
            "rejected a web asset resolved outside the asset directory"
        );
        return bad_asset_path_response();
    }

    match tokio::fs::read(&resolved).await {
        Ok(contents) => static_response(content_type_for(&resolved), contents),
        Err(error) => {
            tracing::error!(
                requested,
                path = %resolved.display(),
                %error,
                "failed to load web asset from disk"
            );
            asset_unavailable_response(requested)
        }
    }
}

/// Joins `requested` onto `asset_dir` one path component at a time, accepting
/// only plain segments (`Component::Normal`). This rejects absolute paths,
/// `..` parent segments, and `.` segments structurally, before the
/// filesystem is ever touched — `../../etc/passwd` or a leading `/` cannot
/// produce a path outside `asset_dir`.
fn safe_asset_path(asset_dir: &Path, requested: &str) -> Option<PathBuf> {
    if requested.is_empty() {
        return None;
    }
    let mut resolved = asset_dir.to_path_buf();
    for component in Path::new(requested).components() {
        match component {
            Component::Normal(part) => resolved.push(part),
            _ => return None,
        }
    }
    Some(resolved)
}

fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("ico") => "image/x-icon",
        Some("webp") => "image/webp",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

#[derive(Serialize)]
struct StatusResponse {
    version: &'static str,
    uptime_seconds: u64,
    camera: CameraStatus,
    capture_stage: CaptureStageStatus,
    sync: SyncStatus,
    schedule: SchedulerStatus,
    config: AppConfig,
    preview: PreviewState,
    /// Whether `config` above is a staged-but-uncommitted preview rather
    /// than the committed config — lets a freshly loaded page (e.g. after
    /// a refresh mid-edit) tell the two apart and restore its own "unsaved
    /// changes" indicator (Save/Discard button state) accordingly, instead
    /// of always assuming a clean load.
    config_staged: bool,
}

#[derive(Serialize)]
struct CameraStatus {
    detected: bool,
    sensor: Option<String>,
    backend: CameraBackendKind,
    busy: bool,
    streaming: bool,
    queued_commands: usize,
}

#[derive(Serialize)]
struct CaptureStageStatus {
    path: String,
    queued_files: u64,
    queued_bytes: u64,
}

#[derive(Serialize)]
struct StreamResponse {
    message: &'static str,
    profile: CaptureProfile,
    settings: CameraSettings,
    control_revision: u64,
}

impl StreamResponse {
    fn new(message: &'static str, request: StreamRequest) -> Self {
        Self {
            message,
            profile: request.profile,
            settings: request.settings,
            control_revision: request.control_revision,
        }
    }
}

/// Whether there is a real, uncommitted edit pending: the preview file's
/// content differs from the committed cache's. NOT the same as "the
/// preview file exists" — `discard_config` resolves a staged edit by
/// overwriting `preview_config_path` with a copy of the committed cache
/// rather than deleting it (and `commit_config` never touches
/// `preview_config_path` at all), so the file exists on disk from the
/// first-ever staged edit onward for the rest of the daemon's life.
/// Content equality, not existence, is the only reliable signal.
async fn config_is_staged(preview_path: &Path, cache_path: &Path) -> bool {
    let Ok(preview) = tokio::fs::read_to_string(preview_path).await else {
        return false;
    };
    match durable_state::read_cached(cache_path).await {
        Ok(cache) => preview != cache,
        Err(_) => true,
    }
}

/// Reads the currently-in-effect config: the staged preview if one exists,
/// otherwise the committed config's fast tmpfs cache (never the durable
/// SD-card path directly — this is on hot paths like `status()`'s every-
/// few-seconds poll, design doc §2.1), otherwise defaults.
async fn current_app_config(state: &AppState) -> AppConfig {
    if state.preview_config_path.exists() {
        match tokio::fs::read_to_string(&*state.preview_config_path).await {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => AppConfig::default(),
        }
    } else {
        match durable_state::read_cached(&state.config_cache_path).await {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => AppConfig::default(),
        }
    }
}

async fn status(State(state): State<AppState>) -> Result<Json<StatusResponse>, AppError> {
    let (queued_files, queued_bytes) = queue_usage(&state.capture_dir).await?;
    let camera = state.camera.status();
    let config_staged =
        config_is_staged(&state.preview_config_path, &state.config_cache_path).await;
    let mut config = current_app_config(&state).await;
    // Every page polls this; the ntfy topic and token must never reach a
    // browser (docs/optic-daemon-digest-heartbeat.md §6.1).
    if let Some(notifications) = config.notifications.as_mut() {
        crate::optic_alerts::redact_notifications(notifications);
    }
    Ok(Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION"),
        uptime_seconds: state.started.elapsed().as_secs(),
        camera: CameraStatus {
            detected: state.sensor.is_some(),
            sensor: state.sensor.clone(),
            backend: camera.backend,
            busy: camera.busy,
            streaming: camera.streaming,
            queued_commands: camera.queued_commands,
        },
        capture_stage: CaptureStageStatus {
            path: state.capture_dir.display().to_string(),
            queued_files,
            queued_bytes,
        },
        sync: state.sync.status(),
        schedule: state.scheduler.status(),
        config,
        preview: read_preview_state(&state.preview_state_cache_path).await,
        config_staged,
    }))
}

/// Operator preview preferences that must outlive the page and the daemon.
/// The live preview itself is browser-driven; every tab consults this, and
/// `start_stream` refuses to start while `stopped`, so one tab's Stop isn't
/// undone by another tab's auto-start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
struct PreviewState {
    stopped: bool,
    downsample: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewStateUpdate {
    stopped: Option<bool>,
    downsample: Option<bool>,
}

impl PreviewState {
    fn apply(self, update: &PreviewStateUpdate) -> Self {
        Self {
            stopped: update.stopped.unwrap_or(self.stopped),
            downsample: update.downsample.unwrap_or(self.downsample),
        }
    }
}

/// Missing or unreadable state means defaults: preview running, full size.
async fn read_preview_state(cache_path: &Path) -> PreviewState {
    match durable_state::read_cached(cache_path).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => PreviewState::default(),
    }
}

async fn update_preview_state(
    State(state): State<AppState>,
    Json(update): Json<PreviewStateUpdate>,
) -> Result<Json<PreviewState>, AppError> {
    let _guard = state.preview_state_lock.lock().await;
    let next = read_preview_state(&state.preview_state_cache_path)
        .await
        .apply(&update);
    let content = serde_json::to_string_pretty(&next).map_err(std::io::Error::other)?;
    durable_state::write_through(
        &state.preview_state_path,
        &state.preview_state_cache_path,
        &content,
    )
    .await?;
    if next.stopped {
        state.camera.stop_stream().await?;
    }
    Ok(Json(next))
}

async fn sync_pause(State(state): State<AppState>) -> Result<Json<SyncStatus>, AppError> {
    state.sync.pause().await?;
    Ok(Json(state.sync.status()))
}

async fn sync_resume(State(state): State<AppState>) -> Result<Json<SyncStatus>, AppError> {
    state.sync.resume().await?;
    Ok(Json(state.sync.status()))
}

async fn sync_retry_now(State(state): State<AppState>) -> Result<Json<SyncStatus>, AppError> {
    state.sync.retry_now().await?;
    Ok(Json(state.sync.status()))
}

async fn schedule_pause(State(state): State<AppState>) -> Result<Json<SchedulerStatus>, AppError> {
    state.scheduler.pause().await?;
    Ok(Json(state.scheduler.status()))
}

async fn schedule_resume(State(state): State<AppState>) -> Result<Json<SchedulerStatus>, AppError> {
    state.scheduler.resume().await?;
    Ok(Json(state.scheduler.status()))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SchedulePreviewRequest {
    #[serde(default)]
    station: Option<optic_scheduler::Station>,
    #[serde(default)]
    rules: Vec<optic_scheduler::Rule>,
}

/// Stages rule edits into `preview_config.json`, the same file
/// `reconfigure_stream` stages camera settings into — but this never
/// touches the camera pipeline at all, unlike that handler. Rejects
/// invalid/duplicate slugs immediately (design doc §3.1's "immediate
/// inline feedback" half of the validation contract; `commit_config` is
/// the authoritative other half).
async fn schedule_preview(
    State(state): State<AppState>,
    Json(request): Json<SchedulePreviewRequest>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(error) = optic_scheduler::validate_rule_slugs(&request.rules) {
        return Err(AppError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: format!("invalid schedule rule slugs: {error:?}"),
        });
    }
    let mut config = current_app_config(&state).await;
    config.schedule.station = request.station;
    config.schedule.rules = request.rules;
    let serialized = serde_json::to_string_pretty(&config).map_err(|error| AppError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: error.to_string(),
    })?;
    let temp_path = state.preview_config_path.with_extension("json.tmp");
    tokio::fs::write(&temp_path, serialized).await?;
    tokio::fs::rename(temp_path, &*state.preview_config_path).await?;
    Ok((StatusCode::OK, Json(Message::new("schedule staged"))))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveDngRequest {
    save_dng: bool,
}

/// Stages the committed DNG preference (`AppConfig.save_dng`) into
/// `preview_config.json`, same file/pattern as `schedule_preview` —
/// deliberately its own tiny endpoint rather than piggybacking on
/// `StreamRequest`/`reconfigure_stream`: saving a companion DNG has
/// nothing to do with the live preview pipeline (the MJPEG stream never
/// produces a DNG), and `reconfigure_stream`'s staging only fires while
/// `livePreview` is true client-side (`schedulePreviewUpdate`'s guard),
/// which would make toggling this checkbox a no-op whenever the preview
/// isn't currently running. This path stages unconditionally, same as
/// `schedule_preview`.
async fn stage_save_dng(
    State(state): State<AppState>,
    Json(request): Json<SaveDngRequest>,
) -> Result<impl IntoResponse, AppError> {
    let mut config = current_app_config(&state).await;
    config.save_dng = request.save_dng;
    let serialized = serde_json::to_string_pretty(&config).map_err(|error| AppError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: error.to_string(),
    })?;
    let temp_path = state.preview_config_path.with_extension("json.tmp");
    tokio::fs::write(&temp_path, serialized).await?;
    tokio::fs::rename(temp_path, &*state.preview_config_path).await?;
    Ok((StatusCode::OK, Json(Message::new("DNG preference staged"))))
}

#[derive(Debug, serde::Deserialize)]
struct ForecastParams {
    hours: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ForecastShot {
    at: chrono::DateTime<chrono::Utc>,
    rule_slugs: Vec<String>,
}

/// `/mnt/capture`'s fixed tmpfs size (design doc §7/§8): 256 MiB.
const CAPTURE_TMPFS_CAPACITY_BYTES: u64 = 256 * 1024 * 1024;

/// Per-shot byte estimates for the Storage/Bandwidth Forecaster (design
/// doc §8). Real measurements, not guesses — captured live against this
/// same Pi/sensor on 2026-09-20 (`worklogs/2026-09-20-scheduler-phase2c-storage-forecaster.md`
/// has the exact capture responses).
///
/// **Known limitation, stated plainly rather than hidden behind false
/// precision**: the JPEG component of each figure is scene-dependent —
/// a dark/night scene compresses far more than a detailed daytime one.
/// The same Pi's `MasterArchive` JPEG measured 7.78 MB in an earlier
/// (daytime) session vs. 1.72 MB just now (nighttime) — a ~4.5x spread
/// on the *same profile, same sensor, same day*. The DNG component does
/// **not** vary this way (raw sensor data, size fixed by resolution) and
/// measured consistently across sessions. To keep the forecaster's
/// storage/fill-time warning conservative rather than falsely
/// reassuring, every constant here uses the **larger** of any measured
/// samples for that profile — these are deliberately worst-case-leaning
/// estimates, not averages.
fn estimated_bytes_per_shot(profile: CaptureProfile, save_dng: bool) -> u64 {
    match profile {
        // DNG was mandatory here (`validate_raw_policy`) until 2026-09-20,
        // when it became an optional default at the user's request — same
        // as `Dci4k`, `save_dng` is now genuinely consulted. Larger of two
        // measured sessions: JPEG 7,783,245 B (daytime) + DNG 24,661,360 B.
        CaptureProfile::MasterArchive if save_dng => 32_444_605,
        // No live no-DNG MasterArchive sample exists yet (this combination
        // was rejected outright before tonight) — uses the same measured
        // JPEG-only figure as the DNG case above (7,783,245 B), which is
        // already the larger/conservative daytime sample of just the JPEG
        // component, not a guess.
        CaptureProfile::MasterArchive => 7_783_245,
        // Never includes DNG (`validate_raw_policy`) — `save_dng` is
        // irrelevant here either. Larger of five same-session samples
        // (avg ~529.7 KB); tonight's separate measurement was a
        // dramatically smaller 51.8 KB, underscoring the scene-dependence
        // note above.
        CaptureProfile::Binning2k => 530_000,
        // The one profile where `save_dng` was already consulted before
        // tonight. Both figures measured live tonight (2026-09-20, ~03:15
        // UTC) — no prior daytime sample existed for this profile at all
        // (design doc §11/§8 flagged it "TBD/measure"); flagged in the
        // worklog as the weakest of the three estimates for exactly that
        // reason.
        CaptureProfile::Dci4k if save_dng => 17_891_671,
        CaptureProfile::Dci4k => 357_067,
    }
}

#[derive(Debug, Serialize)]
struct ForecastResponse {
    horizon_hours: u32,
    shots: Vec<ForecastShot>,
    /// `estimated_bytes_per_shot * shots.len()` — the physical
    /// (post-merge) shot count, per design doc §8's explicit note that
    /// this must count physical captures, not raw per-rule occurrences.
    estimated_total_bytes: u64,
    estimated_bytes_per_shot: u64,
    /// How full `/mnt/capture` already is right now, out of
    /// `CAPTURE_TMPFS_CAPACITY_BYTES` — the baseline the fill-time
    /// estimate below counts up from, not zero.
    capture_stage_queued_bytes: u64,
    capture_tmpfs_capacity_bytes: u64,
    /// Seconds until `/mnt/capture` would fill **if sync stalled
    /// entirely and captures kept firing at this forecast's average
    /// rate** (design doc §7/§8's "fills in ~40 min if sync stalls"
    /// framing) — `None` when the forecast has no shots at all, i.e. no
    /// rate to project. Not a claim about what will actually happen
    /// (sync is expected to keep draining the queue continuously); it's
    /// the proactive-warning half of §7's full-disk answer, the
    /// runtime skip+report behavior being the backstop.
    estimated_seconds_to_fill_if_sync_stalled: Option<u64>,
    /// Design doc §8's dead-rule advisory: enabled rules contributing to
    /// zero of `shots` — likely misconfigured (contradictory
    /// constraints), not intentionally idle.
    dead_rule_slugs: Vec<String>,
    /// Design doc §8's overlap advisory: enabled-rule pairs sustaining a
    /// combined rate higher than either alone over a shared span — the
    /// case §3.1's merge+tag mechanism alone doesn't surface.
    overlap_advisories: Vec<OverlapAdvisoryResponse>,
}

#[derive(Debug, Serialize)]
struct OverlapAdvisoryResponse {
    rule_slugs: [String; 2],
    window_start: chrono::DateTime<chrono::Utc>,
    window_end: chrono::DateTime<chrono::Utc>,
    combined_shots: usize,
}

/// Reads whatever is *currently staged* (preview if present, else the
/// committed config) — design doc §8: edits are visible in the forecast
/// immediately, before committing. Capped to a week regardless of what's
/// requested, so a typo'd `hours` query param can't trigger a pathological
/// computation.
async fn schedule_forecast(
    State(state): State<AppState>,
    Query(params): Query<ForecastParams>,
) -> Result<Json<ForecastResponse>, AppError> {
    let horizon_hours = params.hours.unwrap_or(48).clamp(1, 168);
    let config = current_app_config(&state).await;
    let raw_shots = optic_scheduler::forecast(
        &config.schedule,
        chrono::Utc::now(),
        chrono::Duration::hours(i64::from(horizon_hours)),
    );

    let dead_rule_slugs = optic_scheduler::dead_rule_slugs(&config.schedule, &raw_shots);
    let overlap_advisories = optic_scheduler::overlap_advisories(&config.schedule, &raw_shots)
        .into_iter()
        .map(|advisory| OverlapAdvisoryResponse {
            rule_slugs: advisory.rule_slugs,
            window_start: advisory.window_start.with_timezone(&chrono::Utc),
            window_end: advisory.window_end.with_timezone(&chrono::Utc),
            combined_shots: advisory.combined_shots,
        })
        .collect();

    let shots: Vec<ForecastShot> = raw_shots
        .into_iter()
        .map(|shot| ForecastShot {
            at: shot.at.with_timezone(&chrono::Utc),
            rule_slugs: shot.rule_slugs,
        })
        .collect();

    let bytes_per_shot = estimated_bytes_per_shot(config.profile, config.save_dng);
    let estimated_total_bytes = bytes_per_shot.saturating_mul(shots.len() as u64);
    let (_, capture_stage_queued_bytes) = queue_usage(&state.capture_dir).await?;
    let bytes_per_hour = estimated_total_bytes / u64::from(horizon_hours);
    let estimated_seconds_to_fill_if_sync_stalled = (bytes_per_hour > 0).then(|| {
        let remaining = CAPTURE_TMPFS_CAPACITY_BYTES.saturating_sub(capture_stage_queued_bytes);
        (remaining as f64 / bytes_per_hour as f64 * 3600.0) as u64
    });

    Ok(Json(ForecastResponse {
        horizon_hours,
        shots,
        estimated_total_bytes,
        estimated_bytes_per_shot: bytes_per_shot,
        capture_stage_queued_bytes,
        capture_tmpfs_capacity_bytes: CAPTURE_TMPFS_CAPACITY_BYTES,
        estimated_seconds_to_fill_if_sync_stalled,
        dead_rule_slugs,
        overlap_advisories,
    }))
}

#[derive(Debug, serde::Deserialize)]
struct CaptureHistoryParams {
    source: Option<String>,
    profile: Option<String>,
    success: Option<bool>,
    rule_slug: Option<String>,
    /// Unix milliseconds, inclusive — matches the rest of this API's
    /// timestamp convention (`*_unix_ms` fields elsewhere).
    since: Option<u64>,
    until: Option<u64>,
    limit: Option<u32>,
    offset: Option<u32>,
}

#[derive(Debug, Serialize)]
struct CaptureHistoryResponse {
    entries: Vec<CaptureLogEntry>,
    total: u64,
    limit: u32,
    offset: u32,
}

/// Dedicated capture-history page's backing query (design doc
/// `docs/optic-daemon-capture-log.md` §6's previously-deferred "dashboard
/// panel for querying this history"). `limit` is clamped to keep a typo'd
/// query param from triggering a pathological scan, same posture as
/// `schedule_forecast`'s `hours` clamp.
async fn capture_history(
    State(state): State<AppState>,
    Query(params): Query<CaptureHistoryParams>,
) -> Result<Json<CaptureHistoryResponse>, AppError> {
    let Some(capture_log) = &state.capture_log else {
        return Err(AppError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "capture history is unavailable on this daemon".to_owned(),
        });
    };

    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let offset = params.offset.unwrap_or(0);
    let page = capture_log
        .query(CaptureQueryFilter {
            source: params.source,
            profile: params.profile,
            success: params.success,
            rule_slug: params.rule_slug,
            since_unix_ms: params.since,
            until_unix_ms: params.until,
            limit,
            offset,
        })
        .await;

    Ok(Json(CaptureHistoryResponse {
        entries: page.entries,
        total: page.total,
        limit,
        offset,
    }))
}

/// Rollup window for the capture-health portion of the system panel. Fixed
/// rather than configurable for v1 — see
/// `worklogs/2026-09-18-system-control-panel.md`.
const CAPTURE_HEALTH_WINDOW_HOURS: u32 = 24;

#[derive(Serialize)]
struct SystemStatusResponse {
    system: SystemStatus,
    capture_health: CaptureHealthStatus,
}

async fn system_status_handler(State(state): State<AppState>) -> Json<SystemStatusResponse> {
    let system = state.system_status.snapshot().await;
    let capture_health = match &state.capture_log {
        Some(log) => log.recent_health(CAPTURE_HEALTH_WINDOW_HOURS).await,
        None => CaptureHealthStatus::empty(CAPTURE_HEALTH_WINDOW_HOURS),
    };
    Json(SystemStatusResponse {
        system,
        capture_health,
    })
}

// Each request is recorded before its command runs: a successful reboot or
// shutdown may stop this process before anything after it executes.
async fn system_reboot(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    state
        .record_event(
            SystemEventKind::RebootRequested,
            serde_json::json!({ "source": "dashboard" }),
        )
        .await;
    if let Err(error) = system_status::reboot_host().await {
        record_request_failure(&state, "reboot", &error).await;
        return Err(error.into());
    }
    Ok((StatusCode::OK, Json(Message::new("rebooting"))))
}

async fn system_shutdown(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    state
        .record_event(
            SystemEventKind::ShutdownRequested,
            serde_json::json!({ "source": "dashboard" }),
        )
        .await;
    if let Err(error) = system_status::power_off_host().await {
        record_request_failure(&state, "shutdown", &error).await;
        return Err(error.into());
    }
    Ok((StatusCode::OK, Json(Message::new("shutting down"))))
}

async fn system_restart_daemon(State(state): State<AppState>) -> impl IntoResponse {
    state
        .record_event(
            SystemEventKind::DaemonRestartRequested,
            serde_json::json!({ "source": "dashboard" }),
        )
        .await;
    system_status::restart_daemon_detached();
    (StatusCode::OK, Json(Message::new("restarting")))
}

async fn record_request_failure(state: &AppState, action: &str, error: &std::io::Error) {
    state
        .record_event(
            SystemEventKind::RequestFailed,
            serde_json::json!({ "action": action, "error": error.to_string() }),
        )
        .await;
}

async fn system_ntp_sync() -> Result<impl IntoResponse, AppError> {
    system_status::sync_ntp_now().await?;
    Ok((StatusCode::OK, Json(Message::new("NTP sync requested"))))
}

#[derive(Debug, serde::Deserialize)]
struct SetTimezoneRequest {
    timezone: String,
}

/// Sets the Pi's system timezone (`timedatectl set-timezone`), called
/// from the Config page's "Save station" flow — the user's explicit
/// choice to keep the Station's timezone and the system clock in sync
/// rather than treating them as independent settings. Validates against
/// the same `chrono_tz::TZ_VARIANTS` list `list_timezones` serves,
/// before ever invoking the OS command, so a malformed request can't
/// reach `timedatectl` at all (defense in depth — `Command::arg` already
/// passes this as a literal argv entry, never through a shell, so
/// there's no injection risk either way, but a real validation error
/// message is more useful than whatever `timedatectl` would print).
async fn system_set_timezone(
    Json(request): Json<SetTimezoneRequest>,
) -> Result<impl IntoResponse, AppError> {
    if !is_valid_timezone(&request.timezone) {
        return Err(AppError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: format!("not a recognized IANA timezone: {}", request.timezone),
        });
    }
    system_status::set_system_timezone(&request.timezone).await?;
    Ok((
        StatusCode::OK,
        Json(Message::new("system timezone updated")),
    ))
}

fn is_valid_timezone(timezone: &str) -> bool {
    chrono_tz::TZ_VARIANTS
        .iter()
        .any(|tz| tz.name() == timezone)
}

/// Every valid IANA timezone name, for the Config page's timezone
/// `<select>` (design doc's own `Station.timezone` doc comment already
/// calls this out as an IANA name; this endpoint is what actually
/// enforces it can only ever be one, rather than a free-text field that
/// silently falls back to UTC on a typo — see
/// `optic_scheduler.rs::Station::tz`).
async fn list_timezones() -> Json<Vec<&'static str>> {
    Json(chrono_tz::TZ_VARIANTS.iter().map(|tz| tz.name()).collect())
}

#[derive(Debug, serde::Deserialize)]
struct CelestialPreviewParams {
    latitude: f64,
    longitude: f64,
    #[serde(default)]
    elevation_m: f64,
}

#[derive(Debug, Serialize)]
struct CelestialPreviewItem {
    group: &'static str,
    label: &'static str,
    /// `None` if no occurrence was found within the search horizon —
    /// extremely rare for these events at real-world latitudes (would
    /// need e.g. polar day/night), but astronomically possible, so this
    /// is a real `Option`, not an infallible unwrap.
    at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize)]
struct CelestialPreviewResponse {
    items: Vec<CelestialPreviewItem>,
    moon_illumination_pct: f64,
    moon_waxing: bool,
}

/// Celestial-event preview for the Config page's Station card — lets an
/// operator sanity-check "does this location/timezone actually look
/// right?" against real astronomy immediately, without first saving
/// anything or navigating to the Scheduler's forecast. Deliberately a
/// GET with lat/long/elevation as query params, not reading the
/// committed/staged `Station` from `AppConfig` at all: previewing
/// whatever is *currently typed into the form* (including an unsaved
/// edit) is the whole point — `timezone` isn't a parameter here because
/// none of this math depends on it (ephemeris positions are computed in
/// absolute UTC/Julian-day terms; `Station.timezone` only matters for
/// the scheduler's local-wall-clock trigger types, e.g.
/// `RecurringTime`), the frontend applies the Station's timezone purely
/// for display formatting.
///
/// The curated list below (not every `SolarEvent`/`LunarEvent`/
/// `MilkyWayEvent` variant — `FixedElevation`/`CoreElevation`/
/// `Orientation` need an explicit degrees/azimuth parameter this preview
/// has no input for) covers what's actually useful for planning a
/// timelapse: the day/night/twilight boundaries, the two photography-
/// specific windows (golden/blue hour), Moon rise/set + current
/// illumination/phase trend, and the Milky Way core's daily window,
/// since this project's own Ephemeris trigger work this session was
/// largely in service of Milky Way timelapses.
async fn celestial_preview(
    Query(params): Query<CelestialPreviewParams>,
) -> Result<Json<CelestialPreviewResponse>, AppError> {
    if !(-90.0..=90.0).contains(&params.latitude) || !(-180.0..=180.0).contains(&params.longitude) {
        return Err(AppError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: "latitude must be -90..=90 and longitude -180..=180".to_owned(),
        });
    }
    let station = Station {
        latitude: params.latitude,
        longitude: params.longitude,
        elevation_m: params.elevation_m,
        // Irrelevant to every computation below (see doc comment) — a
        // placeholder, not a guess at the real value.
        timezone: "UTC".to_owned(),
    };
    let now = chrono::Utc::now();
    let short_horizon = now + chrono::Duration::hours(48);
    // Consecutive same-phase Moon events are ~29.5 days apart; 35 days
    // guarantees catching the next one.
    let long_horizon = now + chrono::Duration::days(35);

    let next = |target: CelestialTarget, until: chrono::DateTime<chrono::Utc>| {
        optic_scheduler::ephemeris_occurrences(&target, 0, &station, now, until)
            .into_iter()
            .next()
    };
    let solar = |event: SolarEvent| next(CelestialTarget::Solar(event), short_horizon);
    let lunar = |event: LunarEvent| next(CelestialTarget::Lunar(event), short_horizon);
    let milky_way = |event: MilkyWayEvent| next(CelestialTarget::MilkyWay(event), short_horizon);

    let items = vec![
        CelestialPreviewItem {
            group: "Sun",
            label: "Sunrise",
            at: solar(SolarEvent::Sunrise),
        },
        CelestialPreviewItem {
            group: "Sun",
            label: "Sunset",
            at: solar(SolarEvent::Sunset),
        },
        CelestialPreviewItem {
            group: "Sun",
            label: "Solar noon",
            at: solar(SolarEvent::SolarNoon),
        },
        CelestialPreviewItem {
            group: "Sun",
            label: "Civil dawn",
            at: solar(SolarEvent::CivilDawn),
        },
        CelestialPreviewItem {
            group: "Sun",
            label: "Civil dusk",
            at: solar(SolarEvent::CivilDusk),
        },
        CelestialPreviewItem {
            group: "Sun",
            label: "Astronomical dawn (true night ends)",
            at: solar(SolarEvent::AstronomicalDawn),
        },
        CelestialPreviewItem {
            group: "Sun",
            label: "Astronomical dusk (true night begins)",
            at: solar(SolarEvent::AstronomicalDusk),
        },
        CelestialPreviewItem {
            group: "Sun",
            label: "Golden hour start (morning)",
            at: solar(SolarEvent::GoldenHourMorningStart),
        },
        CelestialPreviewItem {
            group: "Sun",
            label: "Golden hour end (morning)",
            at: solar(SolarEvent::GoldenHourMorningEnd),
        },
        CelestialPreviewItem {
            group: "Sun",
            label: "Golden hour start (evening)",
            at: solar(SolarEvent::GoldenHourEveningStart),
        },
        CelestialPreviewItem {
            group: "Sun",
            label: "Golden hour end (evening)",
            at: solar(SolarEvent::GoldenHourEveningEnd),
        },
        CelestialPreviewItem {
            group: "Sun",
            label: "Blue hour start (morning)",
            at: solar(SolarEvent::BlueHourMorningStart),
        },
        CelestialPreviewItem {
            group: "Sun",
            label: "Blue hour end (morning)",
            at: solar(SolarEvent::BlueHourMorningEnd),
        },
        CelestialPreviewItem {
            group: "Sun",
            label: "Blue hour start (evening)",
            at: solar(SolarEvent::BlueHourEveningStart),
        },
        CelestialPreviewItem {
            group: "Sun",
            label: "Blue hour end (evening)",
            at: solar(SolarEvent::BlueHourEveningEnd),
        },
        CelestialPreviewItem {
            group: "Moon",
            label: "Moonrise",
            at: lunar(LunarEvent::Moonrise),
        },
        CelestialPreviewItem {
            group: "Moon",
            label: "Moonset",
            at: lunar(LunarEvent::Moonset),
        },
        CelestialPreviewItem {
            group: "Moon",
            label: "Lunar transit (highest point)",
            at: lunar(LunarEvent::LunarTransit),
        },
        CelestialPreviewItem {
            group: "Moon",
            label: "Next new moon",
            at: next(CelestialTarget::Lunar(LunarEvent::NewMoon), long_horizon),
        },
        CelestialPreviewItem {
            group: "Moon",
            label: "Next full moon",
            at: next(CelestialTarget::Lunar(LunarEvent::FullMoon), long_horizon),
        },
        CelestialPreviewItem {
            group: "Milky Way",
            label: "Core rise",
            at: milky_way(MilkyWayEvent::CoreRise),
        },
        CelestialPreviewItem {
            group: "Milky Way",
            label: "Core transit (highest point)",
            at: milky_way(MilkyWayEvent::CoreTransit),
        },
        CelestialPreviewItem {
            group: "Milky Way",
            label: "Core set",
            at: milky_way(MilkyWayEvent::CoreSet),
        },
    ];

    let jd_now = crate::ephemeris::julian_day(now);
    let illumination_now = crate::ephemeris::moon_illumination_pct(jd_now);
    let illumination_tomorrow = crate::ephemeris::moon_illumination_pct(jd_now + 1.0);

    Ok(Json(CelestialPreviewResponse {
        items,
        moon_illumination_pct: illumination_now,
        moon_waxing: illumination_tomorrow > illumination_now,
    }))
}

async fn start_stream(
    State(state): State<AppState>,
    Json(mut request): Json<StreamRequest>,
) -> Result<impl IntoResponse, AppError> {
    let preview = read_preview_state(&state.preview_state_cache_path).await;
    if preview.stopped {
        return Err(AppError {
            status: StatusCode::CONFLICT,
            message: "preview is stopped; resume it first".to_owned(),
        });
    }
    request.downsample = preview.downsample;
    let accepted = request.clone();

    // Reconfigure and Start streams exclusively write to the preview staging
    // config — but only profile/settings come from this request. Reading
    // the current config first and overriding just those two fields (rather
    // than constructing a bare-defaults AppConfig) preserves `save_dng`/
    // `schedule` across every live-preview tweak; blindly reconstructing
    // from scratch here would silently wipe scheduler rules on the next
    // slider drag.
    let mut config = current_app_config(&state).await;
    config.profile = request.profile;
    config.settings = request.settings.clone();
    if let Ok(serialized) = serde_json::to_string_pretty(&config) {
        let temp_path = state.preview_config_path.with_extension("json.tmp");
        if tokio::fs::write(&temp_path, serialized).await.is_ok() {
            let _ = tokio::fs::rename(temp_path, &*state.preview_config_path).await;
        }
    }

    state.camera.start_stream(request).await?;
    Ok((
        StatusCode::OK,
        Json(StreamResponse::new("preview started", accepted)),
    ))
}

async fn reconfigure_stream(
    State(state): State<AppState>,
    Json(mut request): Json<StreamRequest>,
) -> Result<impl IntoResponse, AppError> {
    request.downsample = read_preview_state(&state.preview_state_cache_path)
        .await
        .downsample;
    let accepted = request.clone();

    // See `start_stream`'s comment: preserve save_dng/schedule, only
    // override profile/settings from this request.
    let mut config = current_app_config(&state).await;
    config.profile = request.profile;
    config.settings = request.settings.clone();
    if let Ok(serialized) = serde_json::to_string_pretty(&config) {
        let temp_path = state.preview_config_path.with_extension("json.tmp");
        if tokio::fs::write(&temp_path, serialized).await.is_ok() {
            let _ = tokio::fs::rename(temp_path, &*state.preview_config_path).await;
        }
    }

    state.camera.reconfigure_stream(request).await?;
    Ok((
        StatusCode::OK,
        Json(StreamResponse::new("preview updated", accepted)),
    ))
}

async fn stop_stream(State(state): State<AppState>) -> impl IntoResponse {
    // Deliberately does NOT touch preview_config.json. It used to delete it
    // unconditionally here, back when that file was purely camera-preview
    // scratch space — but it's now the general staging file for the whole
    // AppConfig, including schedule rule edits (design doc §9: "reuses the
    // existing preview/commit/discard config flow"). `pagehide` (which
    // calls this via sendBeacon) fires on *any* navigation away from the
    // dashboard, including a plain refresh — deleting the preview here was
    // silently discarding legitimate unsaved edits (e.g. an unchecked rule
    // on the scheduler page) any time the camera preview merely stopped,
    // even though nothing else in this file resolves staged config except
    // an explicit `/api/config/commit` or `/api/config/discard`. Stopping
    // the live preview and having unsaved edits are orthogonal; only the
    // latter two endpoints should ever decide the fate of staged config.
    match state.camera.stop_stream().await {
        Ok(true) => (StatusCode::OK, Json(Message::new("preview stopped"))).into_response(),
        Ok(false) => (
            StatusCode::OK,
            Json(Message::new("preview was not running")),
        )
            .into_response(),
        Err(error) => AppError::from(error).into_response(),
    }
}

async fn mjpeg_stream(State(state): State<AppState>) -> Result<Response, AppError> {
    let mut receiver = state.camera.subscribe().await?;
    let stream = async_stream::stream! {
        loop {
            match receiver.recv().await {
                Ok(frame) => {
                    yield Ok::<Bytes, Infallible>(mjpeg_part(frame));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("multipart/x-mixed-replace; boundary=frame"),
    );
    add_no_store_headers(&mut response);
    Ok(response)
}

fn mjpeg_part(frame: PreviewFrame) -> Bytes {
    let mut part = BytesMut::with_capacity(frame.jpeg.len() + 256);
    part.extend_from_slice(
        format!(
            "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nX-Optic-Sequence: {}\r\nX-Optic-Control-Revision: {}\r\nX-Optic-AE-State: {}\r\nX-Optic-AWB-State: {}\r\nX-Optic-Exposure-Us: {}\r\nX-Optic-Analogue-Gain: {}\r\nX-Optic-Colour-Gains: {}\r\n\r\n",
            frame.jpeg.len(),
            frame.sequence,
            frame.control_revision,
            frame.ae_state.unwrap_or("unavailable"),
            frame.awb_state.unwrap_or("unavailable"),
            optional_header(frame.exposure_us),
            optional_header(frame.analogue_gain),
            frame
                .colour_gains
                .map(|gains| format!("{},{}", gains[0], gains[1]))
                .unwrap_or_else(|| "unavailable".to_owned()),
        )
        .as_bytes(),
    );
    part.extend_from_slice(&frame.jpeg);
    part.extend_from_slice(b"\r\n");
    part.freeze()
}

fn optional_header(value: Option<impl std::fmt::Display>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unavailable".to_owned())
}

async fn capture(
    State(state): State<AppState>,
    Json(request): Json<CaptureRequest>,
) -> Result<Json<CaptureResult>, AppError> {
    let started = Instant::now();
    let requested_at = SystemTime::now();
    let profile = request.profile;
    let settings = request.settings.clone();
    let save_dng = request.save_dng;
    let source = request.source.clone();

    let result = state
        .camera
        .capture_to_stage(&state.capture_dir, request)
        .await;
    let elapsed_ms = started.elapsed().as_millis();
    tracing::info!(stage = "http_handler_total", elapsed_ms, "capture perf");

    if let Some(capture_log) = &state.capture_log {
        let entry = CaptureLogEntry::new(
            requested_at,
            SystemTime::now(),
            elapsed_ms,
            profile,
            settings,
            save_dng,
            &source,
            &result,
        );
        capture_log.record(entry).await;
    }

    Ok(Json(result?))
}

async fn commit_config(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    if state.preview_config_path.exists() {
        let content = tokio::fs::read_to_string(&*state.preview_config_path).await?;
        // Authoritative slug-uniqueness check (design doc §3.1/§14):
        // rejects the whole commit if two rules collide, rather than
        // silently accepting one and losing the operator's intent. Only
        // enforced here if the preview actually parses as AppConfig —
        // malformed preview JSON isn't this check's job to report.
        if let Ok(config) = serde_json::from_str::<AppConfig>(&content)
            && let Err(error) = optic_scheduler::validate_rule_slugs(&config.schedule.rules)
        {
            return Err(AppError {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                message: format!("invalid schedule rule slugs: {error:?}"),
            });
        }
        // Promote the staged preview to the committed config: durable
        // SD-card path first, then its tmpfs cache mirror (design doc
        // §2.1) — never the other order, so a crash between the two never
        // leaves the cache ahead of a value that was never actually
        // durably committed.
        durable_state::write_through(&state.config_path, &state.config_cache_path, &content)
            .await?;
        // Wake the scheduler to re-read the new config immediately, rather
        // than letting it wait out a sleep already armed against the old
        // one (design doc §2.1's hot-reload requirement — a rule edit
        // could otherwise silently take hours to apply).
        state.scheduler.notify_config_changed().await;
    }
    Ok((
        StatusCode::OK,
        Json(Message::new("configuration committed")),
    ))
}

async fn discard_config(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    // Revert preview state by copying the committed config's tmpfs cache
    // back to preview_config.json (reads the cache, not the durable
    // SD-card path — design doc §2.1; the cache is always in sync with it).
    match durable_state::read_cached(&state.config_cache_path).await {
        Ok(content) => {
            let temp_path = state.preview_config_path.with_extension("json.tmp");
            tokio::fs::write(&temp_path, &content).await?;
            tokio::fs::rename(temp_path, &*state.preview_config_path).await?;
        }
        Err(_) => {
            let _ = tokio::fs::remove_file(&*state.preview_config_path).await;
        }
    }
    Ok((
        StatusCode::OK,
        Json(Message::new("configuration discarded")),
    ))
}

#[derive(Serialize)]
struct Message {
    message: &'static str,
}

impl Message {
    fn new(message: &'static str) -> Self {
        Self { message }
    }
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    message: String,
}

impl From<CameraError> for AppError {
    fn from(error: CameraError) -> Self {
        let status = match error {
            CameraError::Busy => StatusCode::CONFLICT,
            CameraError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            CameraError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            CameraError::NotStreaming => StatusCode::CONFLICT,
            CameraError::Io(_) | CameraError::Backend(_) | CameraError::Timeout => {
                StatusCode::BAD_GATEWAY
            }
        };
        Self {
            status,
            message: error.to_string(),
        }
    }
}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }
}

impl From<SyncUnavailable> for AppError {
    fn from(_: SyncUnavailable) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "optic_sync is unavailable".to_owned(),
        }
    }
}

impl From<SchedulerUnavailable> for AppError {
    fn from(_: SchedulerUnavailable) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "optic_scheduler is unavailable".to_owned(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorBody {
            error: String,
        }

        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

async fn queue_usage(path: &PathBuf) -> Result<(u64, u64), std::io::Error> {
    let mut entries = fs_read_dir(path).await?;
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let metadata = entry.metadata().await?;
        if metadata.is_file() {
            files += 1;
            bytes = bytes.saturating_add(metadata.len());
        }
    }
    Ok((files, bytes))
}

async fn fs_read_dir(path: &PathBuf) -> Result<tokio::fs::ReadDir, std::io::Error> {
    tokio::fs::read_dir(path).await
}

fn static_response(content_type: &'static str, contents: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(contents));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(content_type),
    );
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(SECURITY_POLICY),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    add_no_store_headers(&mut response);
    response
}

fn asset_unavailable_response(requested: &str) -> Response {
    plain_text_error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("optic-daemon: web asset unavailable: {requested}\n"),
    )
}

fn bad_asset_path_response() -> Response {
    plain_text_error_response(
        StatusCode::BAD_REQUEST,
        "optic-daemon: invalid web asset path\n".to_owned(),
    )
}

fn plain_text_error_response(status: StatusCode, body: String) -> Response {
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(SECURITY_POLICY),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    add_no_store_headers(&mut response);
    response
}

fn add_no_store_headers(response: &mut Response) {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    // Read straight from the source tree at compile time so this test still
    // catches accidental content regressions even though the daemon itself
    // now loads these files from disk at request time.
    const INDEX_HTML: &str = include_str!("web/index.html");
    const APP_JS: &str = include_str!("web/app.js");
    const STYLES_CSS: &str = include_str!("web/styles.css");
    const FOOTER_JS: &str = include_str!("web/footer.js");
    const CAPTURE_HISTORY_HTML: &str = include_str!("web/capture-history.html");
    const CAPTURE_HISTORY_JS: &str = include_str!("web/capture-history.js");
    const FOCUS_TOOLS_JS: &str = include_str!("web/focus-tools.js");
    const FOOTER_PAGES: [(&str, &str); 4] = [
        ("index.html", INDEX_HTML),
        ("scheduler.html", include_str!("web/scheduler.html")),
        ("capture-history.html", CAPTURE_HISTORY_HTML),
        ("config.html", include_str!("web/config.html")),
    ];

    fn unique_temp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "optic-web-test-{label}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp asset dir");
        dir
    }

    #[tokio::test]
    async fn preview_state_defaults_when_missing_or_corrupt() {
        let dir = unique_temp_dir("preview-state-defaults");
        let cache = dir.join("preview_state.json");
        assert_eq!(read_preview_state(&cache).await, PreviewState::default());
        tokio::fs::write(&cache, "{not json").await.unwrap();
        assert_eq!(read_preview_state(&cache).await, PreviewState::default());
    }

    #[test]
    fn preview_state_partial_update_keeps_other_fields() {
        let current = PreviewState {
            stopped: false,
            downsample: true,
        };
        let update: PreviewStateUpdate = serde_json::from_str(r#"{"stopped":true}"#).unwrap();
        assert_eq!(
            current.apply(&update),
            PreviewState {
                stopped: true,
                downsample: true,
            }
        );
        assert!(serde_json::from_str::<PreviewStateUpdate>(r#"{"paused":true}"#).is_err());
    }

    #[tokio::test]
    async fn preview_state_survives_a_simulated_reboot() {
        let durable_dir = unique_temp_dir("preview-state-durable");
        let durable = durable_dir.join("preview_state.json");
        let old_cache = unique_temp_dir("preview-state-cache-before").join("preview_state.json");
        let state = PreviewState {
            stopped: true,
            downsample: true,
        };
        durable_state::write_through(
            &durable,
            &old_cache,
            &serde_json::to_string(&state).unwrap(),
        )
        .await
        .unwrap();

        // A reboot wipes the tmpfs cache; startup re-hydrates a fresh one.
        let new_cache = unique_temp_dir("preview-state-cache-after").join("preview_state.json");
        durable_state::hydrate_cache(&durable, &new_cache)
            .await
            .unwrap();
        assert_eq!(read_preview_state(&new_cache).await, state);
    }

    async fn body_string(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        String::from_utf8(bytes.to_vec()).expect("utf8 response body")
    }

    #[test]
    fn focus_tools_are_wired_into_the_dashboard() {
        // Loaded before app.js (defer preserves order) and hooked per frame.
        let focus = INDEX_HTML
            .find("<script src=\"/focus-tools.js\" defer></script>")
            .expect("index.html loads focus-tools.js");
        let app = INDEX_HTML
            .find("<script src=\"/app.js\" defer></script>")
            .expect("index.html loads app.js");
        assert!(focus < app);
        assert!(APP_JS.contains("window.OpticFocus?.onFrame(elements.preview)"));
        assert!(APP_JS.contains("window.OpticFocus?.clear()"));
        // Every element focus-tools.js looks up must exist in index.html.
        let mut checked = 0;
        for chunk in FOCUS_TOOLS_JS.split("$(\"#").skip(1) {
            let id = chunk.split('"').next().expect("selector id");
            assert!(
                INDEX_HTML.contains(&format!("id=\"{id}\"")),
                "index.html is missing #{id} used by focus-tools.js"
            );
            checked += 1;
        }
        assert!(
            checked >= 15,
            "expected the focus-tools element lookups, found {checked}"
        );
    }

    #[test]
    fn embedded_assets_are_present() {
        assert!(INDEX_HTML.contains("Project Optic"));
        assert!(APP_JS.contains("/api/stream/start"));
        assert!(APP_JS.contains("/api/stream/reconfigure"));
        assert!(APP_JS.contains("controlRevision !== requestedRevision"));
        assert!(APP_JS.contains("recordRenderedFrame(headers, paintedAt)"));
        assert!(INDEX_HTML.contains("First visible"));
        assert!(APP_JS.contains("previewLabel(accepted.profile, accepted.settings)"));
        assert!(INDEX_HTML.contains("Capture profile"));
        assert!(APP_JS.contains("master_archive"));
        assert!(APP_JS.contains("${profile.previewFps} FPS"));
        assert!(APP_JS.contains("White balance ${optionLabel(\"awb\", values.awb)}"));
        assert!(APP_JS.contains("Denoise ${optionLabel(\"denoise\", values.denoise)}"));
        assert!(APP_JS.contains("Analogue gain ${gain}"));
        assert!(INDEX_HTML.contains("Live preview starts automatically"));
        assert!(!INDEX_HTML.contains("id=\"start-stream\""));
        assert!(!INDEX_HTML.contains("id=\"stop-stream\""));
        assert!(APP_JS.contains("navigator.sendBeacon(\"/api/stream/stop\")"));
        assert!(!INDEX_HTML.contains("JPEG quality"));
        assert!(!INDEX_HTML.contains("Preview ends automatically"));
        assert!(!APP_JS.contains("watchdog limit"));
        assert!(STYLES_CSS.contains(":root"));
    }

    #[test]
    fn every_page_footer_has_one_power_menu() {
        // The handlers themselves are not called here: on a Linux runner
        // they would really try to reboot or power the machine off.
        assert!(FOOTER_JS.contains("\"/api/system/reboot\""));
        assert!(FOOTER_JS.contains("\"/api/system/shutdown\""));
        assert!(FOOTER_JS.contains("#footer-power-menu"));
        for (name, html) in FOOTER_PAGES {
            assert_eq!(
                html.matches("id=\"footer-power\"").count(),
                1,
                "{name} needs exactly one Power button"
            );
            assert!(
                html.contains("aria-label=\"Power\""),
                "{name}: the icon-only Power button needs an accessible name"
            );
            let menu = html
                .find("id=\"footer-power-menu\"")
                .unwrap_or_else(|| panic!("{name} has no Power menu"));
            let reboot = html
                .find("id=\"footer-reboot\"")
                .unwrap_or_else(|| panic!("{name} has no Reboot item"));
            let shutdown = html
                .find("id=\"footer-shutdown\"")
                .unwrap_or_else(|| panic!("{name} has no Shut down item"));
            assert!(
                menu < reboot && reboot < shutdown,
                "{name}: the menu must hold Reboot, then Shut down"
            );
            assert!(!html.contains("Reboot Pi"), "{name}: old Reboot Pi label");
            assert!(!html.contains("Shut down Pi"), "{name}: old label");
        }
    }

    #[tokio::test]
    async fn events_endpoint_lists_newest_first_and_clamps_the_limit() {
        let dir = unique_temp_dir("events-endpoint");
        let log = SystemEventLog::open(&dir.join("events.db"), None).unwrap();
        log.record_startup("test").await;
        log.record(SystemEventKind::RebootRequested, serde_json::json!({}))
            .await;

        let Json(body) = list_events(
            State(Some(log.clone())),
            Query(EventsParams { limit: Some(9999) }),
        )
        .await
        .unwrap_or_else(|_| panic!("the event log is available"));
        let kinds: Vec<_> = body.events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["reboot_requested", "daemon_start"]);

        let Json(one) = list_events(State(Some(log)), Query(EventsParams { limit: Some(1) }))
            .await
            .unwrap_or_else(|_| panic!("the event log is available"));
        assert_eq!(one.events.len(), 1);
    }

    #[tokio::test]
    async fn events_endpoint_reports_a_missing_log() {
        let error = list_events(State(None), Query(EventsParams { limit: None }))
            .await
            .err()
            .expect("no log means an error");
        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn capture_history_page_shows_system_events() {
        assert!(CAPTURE_HISTORY_HTML.contains("id=\"events-body\""));
        assert!(CAPTURE_HISTORY_JS.contains("/api/events"));
    }

    #[test]
    fn security_policy_disallows_inline_scripts() {
        assert!(SECURITY_POLICY.contains("script-src 'self'"));
        assert!(!SECURITY_POLICY.contains("'unsafe-inline'"));
    }

    #[tokio::test]
    async fn serve_asset_reflects_current_file_contents_on_each_request() {
        let dir = unique_temp_dir("hot-reload");
        std::fs::write(dir.join("index.html"), "<html>v1</html>").unwrap();

        let first = serve_asset(&dir, "index.html").await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(
            first.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        assert_eq!(body_string(first).await, "<html>v1</html>");

        std::fs::write(dir.join("index.html"), "<html>v2</html>").unwrap();
        let second = serve_asset(&dir, "index.html").await;
        assert_eq!(body_string(second).await, "<html>v2</html>");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn serve_asset_serves_any_file_dropped_into_the_asset_directory() {
        // No route is registered anywhere for these filenames; the asset
        // route must be able to serve them purely because they exist on
        // disk under asset_dir, including in a subdirectory.
        let dir = unique_temp_dir("agnostic");
        std::fs::create_dir_all(dir.join("icons")).unwrap();
        std::fs::write(dir.join("icons").join("favicon.ico"), [0u8, 1, 2, 3]).unwrap();
        std::fs::write(dir.join("data.json"), br#"{"ok":true}"#).unwrap();

        let icon = serve_asset(&dir, "icons/favicon.ico").await;
        assert_eq!(icon.status(), StatusCode::OK);
        assert_eq!(
            icon.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/x-icon"
        );

        let json = serve_asset(&dir, "data.json").await;
        assert_eq!(json.status(), StatusCode::OK);
        assert_eq!(
            json.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(body_string(json).await, r#"{"ok":true}"#);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn serve_asset_reports_a_missing_file_as_a_friendly_error() {
        let dir = unique_temp_dir("missing");

        let response = serve_asset(&dir, "app.js").await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain; charset=utf-8"
        );
        assert!(body_string(response).await.contains("app.js"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn config_is_staged_is_false_when_no_preview_file_exists() {
        let dir = unique_temp_dir("staged-no-preview");
        let preview_path = dir.join("preview_config.json");
        let cache_path = dir.join("config_cache.json");
        std::fs::write(&cache_path, "committed").unwrap();

        assert!(!config_is_staged(&preview_path, &cache_path).await);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn config_is_staged_is_true_when_preview_content_differs_from_cache() {
        let dir = unique_temp_dir("staged-differs");
        let preview_path = dir.join("preview_config.json");
        let cache_path = dir.join("config_cache.json");
        std::fs::write(&cache_path, "committed").unwrap();
        std::fs::write(&preview_path, "edited").unwrap();

        assert!(config_is_staged(&preview_path, &cache_path).await);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn config_is_staged_is_false_once_preview_content_matches_cache_again() {
        // Mirrors what commit_config/discard_config actually do: neither
        // deletes preview_config.json, they just make its content match
        // the committed cache again (discard by overwriting it, commit by
        // leaving it as-is once the cache itself catches up to it). Mere
        // file existence can never be the signal — only content equality.
        let dir = unique_temp_dir("staged-resolved");
        let preview_path = dir.join("preview_config.json");
        let cache_path = dir.join("config_cache.json");
        std::fs::write(&cache_path, "same").unwrap();
        std::fs::write(&preview_path, "same").unwrap();

        assert!(!config_is_staged(&preview_path, &cache_path).await);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn config_is_staged_is_true_when_preview_exists_but_nothing_was_ever_committed() {
        let dir = unique_temp_dir("staged-no-cache-yet");
        let preview_path = dir.join("preview_config.json");
        let cache_path = dir.join("config_cache.json");
        std::fs::write(&preview_path, "first edit ever").unwrap();

        assert!(config_is_staged(&preview_path, &cache_path).await);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn estimated_bytes_per_shot_ignores_save_dng_only_for_the_profile_that_forbids_it() {
        assert_eq!(
            estimated_bytes_per_shot(CaptureProfile::Binning2k, false),
            estimated_bytes_per_shot(CaptureProfile::Binning2k, true),
            "Binning2k never includes a DNG regardless of save_dng"
        );
    }

    #[test]
    fn estimated_bytes_per_shot_depends_on_save_dng_for_profiles_where_dng_is_optional() {
        for profile in [CaptureProfile::MasterArchive, CaptureProfile::Dci4k] {
            let with_dng = estimated_bytes_per_shot(profile, true);
            let without_dng = estimated_bytes_per_shot(profile, false);
            assert!(
                with_dng > without_dng,
                "a DNG file only adds bytes, never removes them ({profile:?})"
            );
        }
    }

    #[test]
    fn estimated_bytes_per_shot_ranks_profiles_by_their_known_relative_sizes() {
        // MasterArchive (full-resolution, DNG on by default) must be the
        // largest, Binning2k (JPEG-only, quarter resolution) the
        // smallest — a regression here (e.g. transposed constants) would
        // silently produce a wildly wrong storage-fill warning.
        let master_archive = estimated_bytes_per_shot(CaptureProfile::MasterArchive, true);
        let dci4k_with_dng = estimated_bytes_per_shot(CaptureProfile::Dci4k, true);
        let binning2k = estimated_bytes_per_shot(CaptureProfile::Binning2k, false);
        assert!(master_archive > dci4k_with_dng);
        assert!(dci4k_with_dng > binning2k);
    }

    #[tokio::test]
    async fn list_timezones_includes_known_real_iana_zones() {
        let Json(zones) = list_timezones().await;
        assert!(zones.contains(&"America/Vancouver"));
        assert!(zones.contains(&"UTC"));
        // Sanity bound, not a precise count — the real IANA database has
        // several hundred zones; a regression that returned e.g. an empty
        // or truncated list would still pass a bare non-empty check.
        assert!(
            zones.len() > 300,
            "expected the full IANA zone list, got {}",
            zones.len()
        );
    }

    #[test]
    fn is_valid_timezone_accepts_real_zones_and_rejects_garbage() {
        assert!(is_valid_timezone("America/Vancouver"));
        assert!(is_valid_timezone("UTC"));
        assert!(!is_valid_timezone("Not/A/Zone"));
        assert!(!is_valid_timezone(""));
    }

    #[tokio::test]
    async fn celestial_preview_finds_real_sun_and_milky_way_events_for_vancouver() {
        let params = Query(CelestialPreviewParams {
            latitude: 49.2827,
            longitude: -123.1207,
            elevation_m: 70.0,
        });
        let Json(response) = celestial_preview(params).await.unwrap();

        let find = |label: &str| response.items.iter().find(|item| item.label == label);
        let sunrise = find("Sunrise").unwrap().at;
        let sunset = find("Sunset").unwrap().at;
        assert!(sunrise.is_some(), "expected a sunrise within 48h");
        assert!(sunset.is_some(), "expected a sunset within 48h");

        // Milky Way core rise/transit/set should all be found too — daily
        // cadence events, same as the Sun, well within the 48h horizon.
        assert!(find("Core rise").unwrap().at.is_some());
        assert!(find("Core transit (highest point)").unwrap().at.is_some());
        assert!(find("Core set").unwrap().at.is_some());

        assert!((0.0..=100.0).contains(&response.moon_illumination_pct));
    }

    #[tokio::test]
    async fn celestial_preview_rejects_out_of_range_coordinates() {
        let params = Query(CelestialPreviewParams {
            latitude: 200.0,
            longitude: 0.0,
            elevation_m: 0.0,
        });
        let error = celestial_preview(params).await.unwrap_err();
        assert_eq!(error.status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn serve_asset_preserves_security_headers_on_success_and_failure() {
        let dir = unique_temp_dir("headers");
        std::fs::write(dir.join("styles.css"), ":root{}").unwrap();

        let ok = serve_asset(&dir, "styles.css").await;
        assert_eq!(
            ok.headers().get(header::CONTENT_SECURITY_POLICY).unwrap(),
            SECURITY_POLICY
        );
        assert_eq!(
            ok.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        assert_eq!(ok.headers().get(header::CACHE_CONTROL).unwrap(), "no-store");

        let missing = serve_asset(&dir, "app.js").await;
        assert_eq!(
            missing
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .unwrap(),
            SECURITY_POLICY
        );
        assert_eq!(
            missing.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn safe_asset_path_rejects_traversal_and_absolute_paths() {
        let root = PathBuf::from("/srv/optic/web");
        assert!(safe_asset_path(&root, "").is_none());
        assert!(safe_asset_path(&root, "..").is_none());
        assert!(safe_asset_path(&root, "../secret.txt").is_none());
        assert!(safe_asset_path(&root, "../../etc/passwd").is_none());
        assert!(safe_asset_path(&root, "a/../../etc/passwd").is_none());
        assert!(safe_asset_path(&root, "/etc/passwd").is_none());
        assert!(safe_asset_path(&root, "./index.html").is_none());
    }

    #[test]
    fn safe_asset_path_accepts_plain_relative_segments() {
        let root = PathBuf::from("/srv/optic/web");
        assert_eq!(safe_asset_path(&root, "app.js"), Some(root.join("app.js")));
        assert_eq!(
            safe_asset_path(&root, "icons/favicon.ico"),
            Some(root.join("icons").join("favicon.ico"))
        );
    }

    #[tokio::test]
    async fn serve_asset_rejects_path_traversal_attempts() {
        let dir = unique_temp_dir("traversal");
        let secret_dir = unique_temp_dir("traversal-secret");
        std::fs::write(secret_dir.join("secret.txt"), "top secret").unwrap();

        for attempt in [
            "../secret.txt",
            "../../etc/passwd",
            "a/../../etc/passwd",
            "/etc/passwd",
        ] {
            let response = serve_asset(&dir, attempt).await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "attempt should be rejected: {attempt}"
            );
        }

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&secret_dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn serve_asset_rejects_symlinks_that_escape_the_asset_directory() {
        let dir = unique_temp_dir("symlink-escape");
        let outside = unique_temp_dir("symlink-secret");
        std::fs::write(outside.join("secret.txt"), "top secret").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("leak.txt")).unwrap();

        let response = serve_asset(&dir, "leak.txt").await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn mjpeg_part_carries_measurement_metadata() {
        let part = mjpeg_part(PreviewFrame {
            jpeg: Bytes::from_static(b"jpeg"),
            sequence: 42,
            control_revision: 7,
            ae_state: Some("searching"),
            awb_state: Some("converged"),
            exposure_us: Some(12_500),
            analogue_gain: Some(2.5),
            colour_gains: Some([1.25, 1.5]),
        });
        let text = String::from_utf8(part.to_vec()).unwrap();

        assert!(text.contains("Content-Length: 4\r\n"));
        assert!(text.contains("X-Optic-Sequence: 42\r\n"));
        assert!(text.contains("X-Optic-Control-Revision: 7\r\n"));
        assert!(text.contains("X-Optic-Exposure-Us: 12500\r\n"));
        assert!(text.ends_with("\r\n\r\njpeg\r\n"));
    }
}
