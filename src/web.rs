use std::{convert::Infallible, path::PathBuf, sync::Arc, time::Instant};

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
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

const INDEX_HTML: &str = include_str!("web/index.html");
const APP_JS: &str = include_str!("web/app.js");
const STYLES_CSS: &str = include_str!("web/styles.css");
const SECURITY_POLICY: &str = "default-src 'self'; img-src 'self' blob: data:; script-src 'self'; style-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'";

#[derive(Clone)]
pub struct AppState {
    camera: OpticCamera,
    capture_dir: Arc<PathBuf>,
    sensor: Option<String>,
    started: Instant,
}

impl AppState {
    pub fn new(camera: OpticCamera, capture_dir: PathBuf, sensor: Option<String>) -> Self {
        Self {
            camera,
            capture_dir: Arc::new(capture_dir),
            sensor,
            started: Instant::now(),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(javascript))
        .route("/styles.css", get(stylesheet))
        .route("/healthz", get(health))
        .route("/api/status", get(status))
        .route("/api/stream/start", post(start_stream))
        .route("/api/stream/reconfigure", post(reconfigure_stream))
        .route("/api/stream/stop", post(stop_stream))
        .route("/api/stream/mjpeg", get(mjpeg_stream))
        .route("/api/test-shot", post(test_shot))
        .route("/api/capture", post(capture))
        .with_state(state)
}

async fn index() -> Response {
    static_response("text/html; charset=utf-8", INDEX_HTML)
}

async fn javascript() -> Response {
    static_response("text/javascript; charset=utf-8", APP_JS)
}

async fn stylesheet() -> Response {
    static_response("text/css; charset=utf-8", STYLES_CSS)
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
    }))
}

async fn start_stream(
    State(state): State<AppState>,
    Json(request): Json<StreamRequest>,
) -> Result<impl IntoResponse, AppError> {
    let accepted = request.clone();
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
    state.camera.reconfigure_stream(request).await?;
    Ok((
        StatusCode::OK,
        Json(StreamResponse::new("preview updated", accepted)),
    ))
}

async fn stop_stream(State(state): State<AppState>) -> impl IntoResponse {
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

fn static_response(content_type: &'static str, contents: &'static str) -> Response {
    let mut response = Html(contents).into_response();
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

fn add_no_store_headers(response: &mut Response) {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_assets_are_present() {
        assert!(INDEX_HTML.contains("Project Optic"));
        assert!(APP_JS.contains("/api/stream/start"));
        assert!(APP_JS.contains("/api/stream/reconfigure"));
        assert!(APP_JS.contains("controlRevision !== requestedRevision"));
        assert!(APP_JS.contains("recordRenderedFrame(headers, paintedAt)"));
        assert!(INDEX_HTML.contains("First visible"));
        assert!(INDEX_HTML.contains("3A stable"));
        assert!(APP_JS.contains("previewLabel(accepted.profile, accepted.settings)"));
        assert!(INDEX_HTML.contains("Capture profile"));
        assert!(APP_JS.contains("master_archive"));
        assert!(APP_JS.contains("${profile.previewFps} FPS"));
        assert!(APP_JS.contains("White balance ${optionLabel(\"awb\", values.awb)}"));
        assert!(APP_JS.contains("Metering ${optionLabel(\"metering\", values.metering)}"));
        assert!(APP_JS.contains("Exposure mode ${optionLabel(\"exposure\", values.exposure)}"));
        assert!(APP_JS.contains("Denoise ${optionLabel(\"denoise\", values.denoise)}"));
        assert!(APP_JS.contains("EV ${ev}"));
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
