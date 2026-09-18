mod camera;
mod native_camera;
#[cfg(target_os = "linux")]
mod native_codec;
mod optic_camera;
mod web;

use std::{env, net::SocketAddr, path::PathBuf};

use optic_camera::OpticCamera;
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use web::AppState;

const DEFAULT_BIND: &str = "0.0.0.0:8000";
const DEFAULT_CAPTURE_DIR: &str = "/mnt/capture";
// Falls back to the workspace source tree so `cargo run`/`cargo test` hot-reload
// UI edits during local development, where no sibling `web/` directory exists
// next to the debug binary.
const DEV_ASSET_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/web");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .init();

    let bind: SocketAddr = env::var("OPTIC_BIND_ADDR")
        .unwrap_or_else(|_| DEFAULT_BIND.to_owned())
        .parse()?;
    let capture_dir = PathBuf::from(
        env::var("OPTIC_CAPTURE_DIR").unwrap_or_else(|_| DEFAULT_CAPTURE_DIR.to_owned()),
    );
    validate_capture_dir(&capture_dir).await?;

    let asset_dir = resolve_web_asset_dir();
    if asset_dir.is_dir() {
        info!(asset_dir = %asset_dir.display(), "serving web assets from disk");
    } else {
        warn!(
            asset_dir = %asset_dir.display(),
            "web asset directory does not exist; the dashboard will return errors until it is present"
        );
    }

    let camera = OpticCamera::spawn_native();
    let sensor = camera.probe().await;
    if let Some(sensor) = &sensor {
        info!(sensor, "camera detected");
    } else {
        warn!("no IMX477 camera detected; the dashboard will remain available");
    }

    let state = AppState::new(camera.clone(), capture_dir, asset_dir, sensor);
    let listener = TcpListener::bind(bind).await?;
    info!(address = %bind, "optic_web listening");

    axum::serve(listener, web::router(state))
        .with_graceful_shutdown(shutdown_signal(camera))
        .await?;

    Ok(())
}

/// Resolves the directory `index.html`, `app.js`, and `styles.css` are read
/// from on every request. `OPTIC_WEB_ASSETS_DIR` overrides it explicitly;
/// otherwise the daemon looks for a `web` directory installed as a sibling of
/// its own executable (`std::env::current_exe()`), matching the deployment
/// layout that places assets next to `~/.local/bin/optic-daemon`. If neither
/// resolves to a real directory, it falls back to the workspace source tree.
fn resolve_web_asset_dir() -> PathBuf {
    if let Ok(configured) = env::var("OPTIC_WEB_ASSETS_DIR") {
        return PathBuf::from(configured);
    }

    let exe_sibling = env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("web")));
    match exe_sibling {
        Some(dir) if dir.is_dir() => dir,
        _ => PathBuf::from(DEV_ASSET_DIR),
    }
}

async fn validate_capture_dir(path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = tokio::fs::metadata(path).await?;
    if !metadata.is_dir() {
        return Err(format!("capture path is not a directory: {}", path.display()).into());
    }

    let probe = path.join(format!(".optic-web-write-test-{}", std::process::id()));
    tokio::fs::write(&probe, b"").await?;
    tokio::fs::remove_file(&probe).await?;
    Ok(())
}

async fn shutdown_signal(camera: OpticCamera) {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    warn!(%error, "failed to listen for Ctrl-C");
                }
            }
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        warn!(%error, "failed to listen for Ctrl-C");
    }

    info!("shutdown requested");
    if let Err(error) = camera.shutdown().await {
        warn!(%error, "failed to shut down optic_camera cleanly");
    }
}
