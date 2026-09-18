use std::{
    convert::Infallible,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path as PathParam, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::{Bytes, BytesMut};
use serde::Serialize;

use crate::{
    camera::{
        CameraError, CameraSettings, CaptureProfile, CaptureRequest, CaptureResult, PreviewFrame,
        StreamRequest, TestShotRequest,
    },
    optic_camera::{CameraBackendKind, OpticCamera},
};

use crate::camera::AppConfig;

const SECURITY_POLICY: &str = "default-src 'self'; img-src 'self' blob: data:; script-src 'self'; style-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'";

#[derive(Clone)]
pub struct AppState {
    camera: OpticCamera,
    capture_dir: Arc<PathBuf>,
    config_path: Arc<PathBuf>,
    preview_config_path: Arc<PathBuf>,
    asset_dir: Arc<PathBuf>,
    sensor: Option<String>,
    started: Instant,
}

impl AppState {
    pub fn new(
        camera: OpticCamera,
        capture_dir: PathBuf,
        asset_dir: PathBuf,
        sensor: Option<String>,
    ) -> Self {
        let config_path = capture_dir.join("config.json");
        let preview_config_path = capture_dir.join("preview_config.json");
        Self {
            camera,
            capture_dir: Arc::new(capture_dir),
            config_path: Arc::new(config_path),
            preview_config_path: Arc::new(preview_config_path),
            asset_dir: Arc::new(asset_dir),
            sensor,
            started: Instant::now(),
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
        .route("/api/test-shot", post(test_shot))
        .route("/api/capture", post(capture))
        .route("/api/config/commit", post(commit_config))
        .route("/api/config/discard", post(discard_config))
        .with_state(state)
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
    config: AppConfig,
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

async fn status(State(state): State<AppState>) -> Result<Json<StatusResponse>, AppError> {
    let (queued_files, queued_bytes) = queue_usage(&state.capture_dir).await?;
    let camera = state.camera.status();

    // Read from preview_config.json if present; otherwise, fall back to config.json or default
    let config = if state.preview_config_path.exists() {
        match tokio::fs::read_to_string(&*state.preview_config_path).await {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => AppConfig::default(),
        }
    } else {
        match tokio::fs::read_to_string(&*state.config_path).await {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => AppConfig::default(),
        }
    };
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
        config,
    }))
}

async fn start_stream(
    State(state): State<AppState>,
    Json(request): Json<StreamRequest>,
) -> Result<impl IntoResponse, AppError> {
    let accepted = request.clone();

    // Reconfigure and Start streams exclusively write to the preview staging config
    let config = AppConfig {
        profile: request.profile,
        settings: request.settings.clone(),
    };
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
    Json(request): Json<StreamRequest>,
) -> Result<impl IntoResponse, AppError> {
    let accepted = request.clone();

    // Reconfigure and Start streams exclusively write to the preview staging config
    let config = AppConfig {
        profile: request.profile,
        settings: request.settings.clone(),
    };
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
    // Delete the preview stage temporary configuration file when the preview stream closes/stops
    let _ = tokio::fs::remove_file(&*state.preview_config_path).await;

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

async fn test_shot(
    State(state): State<AppState>,
    Json(request): Json<TestShotRequest>,
) -> Result<Response, AppError> {
    let jpeg = state.camera.capture_test_shot(request).await?;
    let mut response = Response::new(Body::from(jpeg));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("image/jpeg"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_static("inline; filename=optic-test-shot.jpg"),
    );
    add_no_store_headers(&mut response);
    Ok(response)
}

async fn capture(
    State(state): State<AppState>,
    Json(request): Json<CaptureRequest>,
) -> Result<Json<CaptureResult>, AppError> {
    Ok(Json(
        state
            .camera
            .capture_to_stage(&state.capture_dir, request)
            .await?,
    ))
}

async fn commit_config(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    if state.preview_config_path.exists() {
        // Overwrite the persistent operational production config under config.json atomically
        let content = tokio::fs::read_to_string(&*state.preview_config_path).await?;
        let temp_path = state.config_path.with_extension("json.tmp");
        tokio::fs::write(&temp_path, &content).await?;
        tokio::fs::rename(temp_path, &*state.config_path).await?;
    }
    Ok((
        StatusCode::OK,
        Json(Message::new("configuration committed")),
    ))
}

async fn discard_config(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    // Revert preview state by copying config.json back to preview_config.json
    if state.config_path.exists() {
        let content = tokio::fs::read_to_string(&*state.config_path).await?;
        let temp_path = state.preview_config_path.with_extension("json.tmp");
        tokio::fs::write(&temp_path, &content).await?;
        tokio::fs::rename(temp_path, &*state.preview_config_path).await?;
    } else {
        let _ = tokio::fs::remove_file(&*state.preview_config_path).await;
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

    async fn body_string(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        String::from_utf8(bytes.to_vec()).expect("utf8 response body")
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
