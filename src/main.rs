mod camera;
mod durable_state;
mod native_camera;
#[cfg(target_os = "linux")]
mod native_codec;
mod optic_camera;
mod optic_capture_log;
mod optic_scheduler;
mod optic_sync;
mod system_status;
mod web;

use std::{env, net::SocketAddr, path::PathBuf};

use optic_camera::OpticCamera;
use optic_capture_log::CaptureLog;
use optic_sync::{DataSyncManager, SyncConfig};
use system_status::{SystemStatusReader, WatchedPaths};
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use web::AppState;

const DEFAULT_BIND: &str = "0.0.0.0:8000";
const DEFAULT_CAPTURE_DIR: &str = "/mnt/capture";
const DEFAULT_SYNC_REMOTE_PORT: u16 = 2222;
const DEFAULT_SYNC_REMOTE_USER: &str = "admin";
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

    let sync_config = resolve_sync_config();
    if let Some(config) = &sync_config {
        info!(
            remote = format!(
                "{}@{}:{}",
                config.remote_user, config.remote_host, config.remote_port
            ),
            "optic_sync configured"
        );
    } else {
        info!("optic_sync disabled: set OPTIC_SYNC_REMOTE_HOST to enable capture transfer");
    }
    let sync = DataSyncManager::spawn(sync_config, capture_dir.clone());

    let capture_log_db_path = resolve_capture_log_db_path();
    let capture_log = match CaptureLog::open(&capture_log_db_path, capture_dir.clone()) {
        Ok(log) => {
            info!(path = %capture_log_db_path.display(), "capture history log ready");
            Some(log)
        }
        Err(error) => {
            warn!(
                %error,
                path = %capture_log_db_path.display(),
                "capture history log unavailable; captures will not be recorded to history"
            );
            None
        }
    };

    let state_dir = capture_log_db_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| capture_log_db_path.clone());
    let system_status = SystemStatusReader::new(WatchedPaths {
        capture_dir: capture_dir.clone(),
        state_dir: state_dir.clone(),
    });

    // Durable config lives on the real, persistent state dir (survives a
    // reboot), not the /mnt/capture tmpfs it used to — design doc §2.1.
    // A fast tmpfs cache mirrors it for every frequent/polled read.
    let cache_dir = resolve_scheduler_cache_dir();
    let config_path = state_dir.join("config.json");
    let config_cache_path = cache_dir.join("config.json");
    if let Err(error) = durable_state::hydrate_cache(&config_path, &config_cache_path).await {
        warn!(%error, path = %config_path.display(), "failed to hydrate the config cache from its durable copy at startup");
    }

    // Scheduler run state is a separate durable file from config.json on
    // purpose — see design doc §2.1's commit-clobbers-a-pause race note.
    let run_state_path = state_dir.join("schedule_run_state.json");
    let run_state_cache_path = cache_dir.join("schedule_run_state.json");
    if let Err(error) = durable_state::hydrate_cache(&run_state_path, &run_state_cache_path).await {
        warn!(%error, path = %run_state_path.display(), "failed to hydrate the scheduler run-state cache from its durable copy at startup");
    }
    let initial_run_state = match durable_state::read_cached(&run_state_cache_path).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => optic_scheduler::ScheduleRunState::default(),
    };
    info!(?initial_run_state, "scheduler starting");
    let scheduler = optic_scheduler::SchedulerHandle::spawn(
        camera.clone(),
        capture_log.clone(),
        capture_dir.clone(),
        config_cache_path.clone(),
        run_state_path,
        run_state_cache_path,
        initial_run_state,
    );

    let state = AppState::new(
        camera.clone(),
        sync.clone(),
        capture_log,
        system_status,
        scheduler.clone(),
        capture_dir,
        config_path,
        config_cache_path,
        asset_dir,
        sensor,
    );
    let listener = TcpListener::bind(bind).await?;
    info!(address = %bind, "optic_web listening");

    axum::serve(listener, web::router(state))
        .with_graceful_shutdown(shutdown_signal(camera, sync, scheduler))
        .await?;

    Ok(())
}

/// Resolves the remote host/credentials for `optic_sync`.
/// `OPTIC_SYNC_REMOTE_HOST` is the only required variable — its absence
/// leaves the manager permanently `disabled` (fine for local development,
/// where no capture-transfer receiver exists). `OPTIC_SYNC_ENABLED=false`
/// force-disables it even with a host configured, as an operator escape
/// hatch. Port/user/identity/known-hosts default to the values the existing
/// Phase 6 shell transfer already uses on the Pi
/// (`scripts/setup-phase-06-pi-ram-transfer.sh`), so cutover needs no new
/// provisioning.
fn resolve_sync_config() -> Option<SyncConfig> {
    if env::var("OPTIC_SYNC_ENABLED").ok().as_deref() == Some("false") {
        return None;
    }
    let remote_host = env::var("OPTIC_SYNC_REMOTE_HOST").ok()?;
    let remote_port = env::var("OPTIC_SYNC_REMOTE_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_SYNC_REMOTE_PORT);
    let remote_user =
        env::var("OPTIC_SYNC_REMOTE_USER").unwrap_or_else(|_| DEFAULT_SYNC_REMOTE_USER.to_owned());
    let identity_file = env::var("OPTIC_SYNC_IDENTITY_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_ssh_path("optic_capture_ed25519"));
    let known_hosts_file = env::var("OPTIC_SYNC_KNOWN_HOSTS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_ssh_path("optic_capture_known_hosts"));

    Some(SyncConfig {
        remote_host,
        remote_port,
        remote_user,
        identity_file,
        known_hosts_file,
    })
}

fn default_ssh_path(filename: &str) -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
    PathBuf::from(home).join(".ssh").join(filename)
}

/// Resolves the SQLite history database path for `optic_capture_log`.
/// `OPTIC_CAPTURE_LOG_DB` overrides it explicitly; otherwise it defaults to
/// `~/.local/state/optic-daemon/history.db` — deliberately outside
/// `~/.local/bin/`, which `scripts/setup-optic-daemon-phase-01.sh` wholesale
/// replaces on every deploy (see design doc §3.2).
fn resolve_capture_log_db_path() -> PathBuf {
    if let Ok(configured) = env::var("OPTIC_CAPTURE_LOG_DB") {
        return PathBuf::from(configured);
    }
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
    PathBuf::from(home)
        .join(".local/state/optic-daemon")
        .join("history.db")
}

/// Resolves the tmpfs directory used as a fast read-through cache mirror
/// of durably-stored config (design doc §2.1). `OPTIC_CACHE_DIR` overrides
/// it explicitly; otherwise defaults to `/dev/shm/optic-daemon` on Linux —
/// a separate tmpfs from the bounded 256 MiB `/mnt/capture` capture queue,
/// so this cache never competes with capture-file storage for space.
/// `/dev/shm` isn't a real, writable path on macOS, so local `cargo run`
/// dev sessions fall back to the system temp dir instead (same rationale
/// as `DEV_ASSET_DIR` above: this project's dev target is macOS, but
/// nothing here should require a Linux-only path to even start).
fn resolve_scheduler_cache_dir() -> PathBuf {
    if let Ok(configured) = env::var("OPTIC_CACHE_DIR") {
        return PathBuf::from(configured);
    }
    #[cfg(target_os = "linux")]
    {
        PathBuf::from("/dev/shm/optic-daemon")
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::temp_dir().join("optic-daemon-cache")
    }
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

async fn shutdown_signal(
    camera: OpticCamera,
    sync: DataSyncManager,
    scheduler: optic_scheduler::SchedulerHandle,
) {
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
    if sync.shutdown().await.is_err() {
        warn!("failed to shut down optic_sync cleanly");
    }
    if scheduler.shutdown().await.is_err() {
        warn!("failed to shut down optic_scheduler cleanly");
    }
}
