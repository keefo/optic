//! In-process replacement for the Phase 6 shell transfer
//! (`scripts/optic-capture-transfer.sh` + its systemd timer): drains capture
//! files out of `capture_dir` to a remote receiver over SSH, using the same
//! restricted-command wire protocol the existing receiver
//! (`scripts/optic-capture-receiver.sh`) already speaks, so the remote side
//! needs no changes.
//!
//! Follows the same bounded-actor shape as `optic_camera`: a single owning
//! task drains a command channel and periodically scans the capture
//! directory, publishing a status snapshot through a `watch` channel.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime},
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::{
    fs,
    io::AsyncReadExt,
    process::Command,
    sync::{mpsc, oneshot, watch},
    time::MissedTickBehavior,
};
use tracing::{error, info, warn};

const COMMAND_QUEUE_CAPACITY: usize = 16;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);
const BASE_BACKOFF_SECS: u64 = 30;
const MAX_BACKOFF_SECS: u64 = 900;

/// Remote host/credentials for the sync transport. Absence of this (no
/// `OPTIC_SYNC_REMOTE_HOST`) means the manager stays permanently `disabled`.
#[derive(Debug, Clone)]
pub struct SyncConfig {
    pub remote_host: String,
    pub remote_port: u16,
    pub remote_user: String,
    pub identity_file: PathBuf,
    pub known_hosts_file: PathBuf,
}

/// The single entry point for capture-file synchronization.
#[derive(Clone)]
pub struct DataSyncManager {
    commands: mpsc::Sender<SyncCommand>,
    state: watch::Receiver<ActorState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Connectivity {
    Disabled,
    Idle,
    Syncing,
    Backoff,
}

#[derive(Debug, Clone)]
struct ActorState {
    paused: bool,
    connectivity: Connectivity,
    queued_files: u64,
    queued_bytes: u64,
    transferred_files: u64,
    transferred_bytes: u64,
    last_error: Option<String>,
    backoff_secs: Option<u64>,
    backoff_until: Option<SystemTime>,
    /// When the capture directory was last scanned. `None` until the first
    /// scan happens (and stays `None` forever while disabled, since a
    /// disabled manager never scans).
    last_scan: Option<SystemTime>,
}

impl ActorState {
    fn disabled() -> Self {
        Self {
            paused: false,
            connectivity: Connectivity::Disabled,
            queued_files: 0,
            queued_bytes: 0,
            transferred_files: 0,
            transferred_bytes: 0,
            last_error: None,
            backoff_secs: None,
            backoff_until: None,
            last_scan: None,
        }
    }

    fn enabled() -> Self {
        Self {
            connectivity: Connectivity::Idle,
            ..Self::disabled()
        }
    }

    fn enter_backoff(&mut self, error: String) {
        let next = self
            .backoff_secs
            .map(|secs| (secs * 2).min(MAX_BACKOFF_SECS))
            .unwrap_or(BASE_BACKOFF_SECS);
        self.backoff_secs = Some(next);
        self.backoff_until = Some(SystemTime::now() + Duration::from_secs(next));
        self.connectivity = Connectivity::Backoff;
        self.last_error = Some(error);
    }

    fn exit_backoff(&mut self) {
        self.backoff_secs = None;
        self.backoff_until = None;
        if self.connectivity == Connectivity::Backoff {
            self.connectivity = Connectivity::Idle;
        }
    }

    fn backoff_remaining(&self) -> bool {
        self.backoff_until
            .is_some_and(|until| SystemTime::now() < until)
    }
}

/// Status snapshot served over `/api/status` and rendered in the dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct SyncStatus {
    pub enabled: bool,
    pub paused: bool,
    pub connectivity: Connectivity,
    pub queued_files: u64,
    pub queued_bytes: u64,
    pub transferred_files: u64,
    pub transferred_bytes: u64,
    pub last_error: Option<String>,
    pub backoff_secs: Option<u64>,
    pub next_retry_in_secs: Option<u64>,
    pub next_scan_in_secs: Option<u64>,
}

/// The command channel closed because the actor already shut down.
#[derive(Debug)]
pub struct SyncUnavailable;

enum SyncCommand {
    Pause { reply: oneshot::Sender<()> },
    Resume { reply: oneshot::Sender<()> },
    RetryNow { reply: oneshot::Sender<()> },
    Shutdown { reply: oneshot::Sender<()> },
}

impl DataSyncManager {
    pub fn spawn(config: Option<SyncConfig>, capture_dir: PathBuf) -> Self {
        let initial = if config.is_some() {
            ActorState::enabled()
        } else {
            ActorState::disabled()
        };
        let (commands, receiver) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let (state_sender, state) = watch::channel(initial);

        tokio::spawn(run_actor(config, capture_dir, receiver, state_sender));

        Self { commands, state }
    }

    pub fn status(&self) -> SyncStatus {
        let state = self.state.borrow().clone();
        let next_retry_in_secs = state.backoff_until.and_then(|until| {
            until
                .duration_since(SystemTime::now())
                .ok()
                .map(|remaining| remaining.as_secs())
        });
        // The next scan happens at (last scan + poll interval); this is an
        // estimate (a slow scan/drain pushes the real next tick later), not
        // a precise deadline.
        let next_scan_in_secs = state.last_scan.map(|last| {
            (last + POLL_INTERVAL)
                .duration_since(SystemTime::now())
                .map(|remaining| remaining.as_secs())
                .unwrap_or(0)
        });
        SyncStatus {
            enabled: state.connectivity != Connectivity::Disabled,
            paused: state.paused,
            connectivity: state.connectivity,
            queued_files: state.queued_files,
            queued_bytes: state.queued_bytes,
            transferred_files: state.transferred_files,
            transferred_bytes: state.transferred_bytes,
            last_error: state.last_error,
            backoff_secs: state.backoff_secs,
            next_retry_in_secs,
            next_scan_in_secs,
        }
    }

    pub async fn pause(&self) -> Result<(), SyncUnavailable> {
        self.send(|reply| SyncCommand::Pause { reply }).await
    }

    pub async fn resume(&self) -> Result<(), SyncUnavailable> {
        self.send(|reply| SyncCommand::Resume { reply }).await
    }

    pub async fn retry_now(&self) -> Result<(), SyncUnavailable> {
        self.send(|reply| SyncCommand::RetryNow { reply }).await
    }

    pub async fn shutdown(&self) -> Result<(), SyncUnavailable> {
        self.send(|reply| SyncCommand::Shutdown { reply }).await
    }

    async fn send(
        &self,
        make_command: impl FnOnce(oneshot::Sender<()>) -> SyncCommand,
    ) -> Result<(), SyncUnavailable> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(make_command(reply))
            .await
            .map_err(|_| SyncUnavailable)?;
        response.await.map_err(|_| SyncUnavailable)
    }
}

async fn run_actor(
    config: Option<SyncConfig>,
    capture_dir: PathBuf,
    mut commands: mpsc::Receiver<SyncCommand>,
    state_tx: watch::Sender<ActorState>,
) {
    let Some(config) = config else {
        info!("optic_sync disabled: OPTIC_SYNC_REMOTE_HOST is not set");
        while let Some(command) = commands.recv().await {
            if let SyncCommand::Shutdown { reply } = command {
                let _ = reply.send(());
                return;
            }
            // Pause/resume/retry-now are meaningless while disabled; reply
            // immediately so callers never hang waiting on a dead manager.
            match command {
                SyncCommand::Pause { reply }
                | SyncCommand::Resume { reply }
                | SyncCommand::RetryNow { reply } => {
                    let _ = reply.send(());
                }
                SyncCommand::Shutdown { .. } => unreachable!(),
            }
        }
        return;
    };

    info!(
        remote = format!(
            "{}@{}:{}",
            config.remote_user, config.remote_host, config.remote_port
        ),
        "optic_sync actor started"
    );

    let mut state = ActorState::enabled();
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break; };
                if apply_command(command, &mut state) {
                    let _ = state_tx.send(state.clone());
                    info!("optic_sync actor shut down");
                    return;
                }
                let _ = state_tx.send(state.clone());
                continue;
            }
            _ = poll.tick() => {}
        }

        state.last_scan = Some(SystemTime::now());
        update_queue_counts(&capture_dir, &mut state).await;

        if state.paused || state.backoff_remaining() {
            let _ = state_tx.send(state.clone());
            continue;
        }

        drain_cycle(&config, &capture_dir, &mut state).await;
        // Refresh queue counts so a successful drain is reflected
        // immediately in the published status, instead of lagging by up to
        // one poll interval.
        update_queue_counts(&capture_dir, &mut state).await;
        let _ = state_tx.send(state.clone());
    }
}

fn apply_command(command: SyncCommand, state: &mut ActorState) -> bool {
    match command {
        SyncCommand::Pause { reply } => {
            state.paused = true;
            let _ = reply.send(());
            false
        }
        SyncCommand::Resume { reply } => {
            state.paused = false;
            let _ = reply.send(());
            false
        }
        SyncCommand::RetryNow { reply } => {
            state.exit_backoff();
            let _ = reply.send(());
            false
        }
        SyncCommand::Shutdown { reply } => {
            let _ = reply.send(());
            true
        }
    }
}

struct QueuedFile {
    path: PathBuf,
    name: String,
    size: u64,
    modified: SystemTime,
}

/// True for exactly the filenames `native_camera.rs` writes captures as
/// (`testshot-<profile-slug>-<suffix>.{jpg,dng}`) plus the paired
/// `optic_capture_log` record (`<same-basename>.log.json`, or
/// `testshot-<profile-slug>-failed-<suffix>.log.json` for a capture that
/// produced no output file) — and nothing else. Deliberately narrower than
/// the shell script's "any non-dotfile" sweep, which could catch and delete
/// `config.json`/`preview_config.json`.
fn is_capture_filename(name: &str) -> bool {
    name.starts_with("testshot-")
        && (name.ends_with(".jpg") || name.ends_with(".dng") || name.ends_with(".log.json"))
}

async fn list_capture_files(capture_dir: &Path) -> Result<Vec<QueuedFile>, std::io::Error> {
    let mut entries = fs::read_dir(capture_dir).await?;
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_capture_filename(name) {
            continue;
        }
        let metadata = entry.metadata().await?;
        if !metadata.is_file() {
            continue;
        }
        files.push(QueuedFile {
            path: entry.path(),
            name: name.to_owned(),
            size: metadata.len(),
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    files.sort_by_key(|file| file.modified);
    Ok(files)
}

async fn update_queue_counts(capture_dir: &Path, state: &mut ActorState) {
    match list_capture_files(capture_dir).await {
        Ok(files) => {
            state.queued_files = files.len() as u64;
            state.queued_bytes = files.iter().map(|file| file.size).sum();
        }
        Err(error) => {
            error!(%error, path = %capture_dir.display(), "optic_sync failed to list capture directory");
            state.last_error = Some(format!("failed to list capture directory: {error}"));
        }
    }
}

async fn drain_cycle(config: &SyncConfig, capture_dir: &Path, state: &mut ActorState) {
    let files = match list_capture_files(capture_dir).await {
        Ok(files) => files,
        Err(error) => {
            error!(%error, "optic_sync failed to list capture directory for drain");
            state.last_error = Some(format!("failed to list capture directory: {error}"));
            return;
        }
    };
    if files.is_empty() {
        state.connectivity = Connectivity::Idle;
        return;
    }

    state.connectivity = Connectivity::Syncing;
    for file in files {
        match put_file(config, &file).await {
            Ok(()) => {
                if let Err(error) = remove_if_unchanged(&file).await {
                    warn!(
                        %error,
                        file = %file.name,
                        "optic_sync transferred a file but could not remove the local copy"
                    );
                    state.last_error = Some(format!(
                        "transferred {} but failed to remove local copy: {error}",
                        file.name
                    ));
                    continue;
                }
                state.transferred_files += 1;
                state.transferred_bytes += file.size;
                state.last_error = None;
            }
            Err(error) => {
                warn!(%error, file = %file.name, "optic_sync transfer failed");
                state.enter_backoff(format!("{}: {error}", file.name));
                return;
            }
        }
    }
    state.connectivity = Connectivity::Idle;
    state.exit_backoff();
}

/// Re-checks size and mtime immediately before deleting, guarding against a
/// file being rewritten while (or just after) it was uploaded. Capture files
/// are written via a hidden-temp-file-then-rename in `native_camera.rs`, so
/// this should never actually fire in practice; it's cheap insurance rather
/// than a load-bearing guarantee.
async fn remove_if_unchanged(file: &QueuedFile) -> Result<(), std::io::Error> {
    let metadata = fs::metadata(&file.path).await?;
    if metadata.len() != file.size || metadata.modified().ok() != Some(file.modified) {
        return Err(std::io::Error::other(
            "local file changed after transfer; not deleting",
        ));
    }
    fs::remove_file(&file.path).await
}

#[derive(Debug)]
enum TransferError {
    Io(std::io::Error),
    Spawn(std::io::Error),
    Timeout,
    Remote(String),
    UnexpectedResponse(String),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransferError::Io(error) => write!(f, "I/O error: {error}"),
            TransferError::Spawn(error) => write!(f, "failed to spawn ssh: {error}"),
            TransferError::Timeout => write!(f, "transfer timed out"),
            TransferError::Remote(message) => write!(f, "receiver rejected transfer: {message}"),
            TransferError::UnexpectedResponse(response) => {
                write!(f, "unexpected receiver response: {response}")
            }
        }
    }
}

async fn put_file(config: &SyncConfig, file: &QueuedFile) -> Result<(), TransferError> {
    let sha256_hex = sha256_file(&file.path).await.map_err(TransferError::Io)?;
    let encoded_name = base64_encode(file.name.as_bytes());
    let remote_target = format!("{}@{}", config.remote_user, config.remote_host);
    let remote_command = format!("put {encoded_name} {} {sha256_hex}", file.size);

    let mut child = Command::new("ssh")
        // Skip system/user ssh_config discovery entirely — every option we
        // need is passed explicitly below, and this file's real permissions
        // are fine (root:root 0644). This works around OpenSSH's ownership
        // check on /etc/ssh/ssh_config.d/*.conf rejecting the file as it
        // appears through this unit's ProtectSystem=strict mount namespace,
        // even though the same file passes the check from an interactive
        // shell on the same host.
        .arg("-F")
        .arg("/dev/null")
        .arg("-T")
        .arg("-p")
        .arg(config.remote_port.to_string())
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("IdentitiesOnly=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=yes")
        .arg("-o")
        .arg(format!(
            "UserKnownHostsFile={}",
            config.known_hosts_file.display()
        ))
        .arg("-o")
        .arg("ConnectTimeout=5")
        .arg("-o")
        .arg("ServerAliveInterval=5")
        .arg("-o")
        .arg("ServerAliveCountMax=2")
        .arg("-i")
        .arg(&config.identity_file)
        .arg(&remote_target)
        .arg(&remote_command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(TransferError::Spawn)?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let mut source = fs::File::open(&file.path)
        .await
        .map_err(TransferError::Io)?;
    let copy_result = tokio::io::copy(&mut source, &mut stdin).await;
    drop(stdin);

    // A copy failure (e.g. a broken pipe) almost always means the child
    // already exited — most likely because it rejected the request before
    // we finished streaming. Collect its exit status/stderr regardless, so
    // the real reason surfaces instead of a bare "broken pipe" I/O error.
    let output = tokio::time::timeout(TRANSFER_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| TransferError::Timeout)?
        .map_err(TransferError::Io)?;

    if let Err(copy_error) = copy_result {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(if detail.is_empty() {
            TransferError::Io(copy_error)
        } else {
            TransferError::Remote(detail.to_owned())
        });
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TransferError::Remote(stderr.trim().to_owned()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let response = stdout.trim();
    let expected_stored = format!("OK stored {} {sha256_hex}", file.size);
    let expected_existing = format!("OK existing {} {sha256_hex}", file.size);
    if response != expected_stored && response != expected_existing {
        return Err(TransferError::UnexpectedResponse(response.to_owned()));
    }
    Ok(())
}

async fn sha256_file(path: &Path) -> Result<String, std::io::Error> {
    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard (RFC 4648, padded) base64 — matches the encoding
/// `optic-capture-receiver.sh` decodes with `base64 -D`. Written inline
/// rather than pulling in a crate for one small, easily-tested operation.
fn base64_encode(input: &[u8]) -> String {
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let combined = (b0 << 16) | (b1 << 8) | b2;
        output.push(BASE64_ALPHABET[(combined >> 18 & 0x3F) as usize] as char);
        output.push(BASE64_ALPHABET[(combined >> 12 & 0x3F) as usize] as char);
        output.push(if chunk.len() > 1 {
            BASE64_ALPHABET[(combined >> 6 & 0x3F) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            BASE64_ALPHABET[(combined & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    output
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn unique_temp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "optic-sync-test-{label}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp capture dir");
        dir
    }

    #[test]
    fn base64_encode_matches_known_vectors() {
        // RFC 4648 test vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(
            base64_encode(b"any carnal pleasure."),
            "YW55IGNhcm5hbCBwbGVhc3VyZS4="
        );
    }

    #[test]
    fn is_capture_filename_allowlists_only_daemon_written_captures() {
        assert!(is_capture_filename("testshot-master_archive-123.jpg"));
        assert!(is_capture_filename("testshot-dci_4k-456.dng"));
        assert!(is_capture_filename(
            "testshot-master-archive-1789753359227.log.json"
        ));
        assert!(is_capture_filename(
            "testshot-2k-binning-failed-1789753359227.log.json"
        ));
        assert!(!is_capture_filename("config.json"));
        assert!(!is_capture_filename("preview_config.json"));
        assert!(!is_capture_filename(".hidden-testshot-123.jpg"));
        assert!(!is_capture_filename("testshot-123.png"));
        assert!(!is_capture_filename("random.jpg"));
        assert!(!is_capture_filename("random.log.json"));
    }

    #[test]
    fn backoff_doubles_from_base_and_caps_at_max() {
        let mut state = ActorState::enabled();
        assert_eq!(state.backoff_secs, None);

        state.enter_backoff("first failure".to_owned());
        assert_eq!(state.backoff_secs, Some(30));
        assert_eq!(state.connectivity, Connectivity::Backoff);
        assert_eq!(state.last_error.as_deref(), Some("first failure"));

        state.enter_backoff("second".to_owned());
        assert_eq!(state.backoff_secs, Some(60));
        state.enter_backoff("third".to_owned());
        assert_eq!(state.backoff_secs, Some(120));
        state.enter_backoff("fourth".to_owned());
        assert_eq!(state.backoff_secs, Some(240));
        state.enter_backoff("fifth".to_owned());
        assert_eq!(state.backoff_secs, Some(480));
        state.enter_backoff("sixth".to_owned());
        assert_eq!(state.backoff_secs, Some(900));
        state.enter_backoff("seventh".to_owned());
        assert_eq!(state.backoff_secs, Some(900), "must not exceed the 15m cap");

        state.exit_backoff();
        assert_eq!(state.backoff_secs, None);
        assert_eq!(state.backoff_until, None);
        assert_eq!(
            state.connectivity,
            Connectivity::Idle,
            "exiting backoff (e.g. via retry-now) should also clear the displayed connectivity state"
        );
    }

    #[tokio::test]
    async fn list_capture_files_only_returns_allowlisted_files_oldest_first() {
        let dir = unique_temp_dir("listing");
        std::fs::write(dir.join("config.json"), b"{}").unwrap();
        std::fs::write(dir.join("preview_config.json"), b"{}").unwrap();
        std::fs::write(dir.join(".hidden.jpg"), b"nope").unwrap();
        std::fs::write(
            dir.join("testshot-master_archive-older.jpg"),
            b"first-written",
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        std::fs::write(
            dir.join("testshot-master_archive-newer.jpg"),
            b"written-later",
        )
        .unwrap();

        let files = list_capture_files(&dir).await.unwrap();
        let names: Vec<&str> = files.iter().map(|file| file.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "testshot-master_archive-older.jpg",
                "testshot-master_archive-newer.jpg",
            ]
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn update_queue_counts_reflects_only_capture_files() {
        let dir = unique_temp_dir("counts");
        std::fs::write(dir.join("config.json"), b"ignored-content-here").unwrap();
        std::fs::write(dir.join("testshot-binning_2k-1.jpg"), b"12345").unwrap();
        std::fs::write(dir.join("testshot-binning_2k-2.dng"), b"1234567890").unwrap();

        let mut state = ActorState::enabled();
        update_queue_counts(&dir, &mut state).await;
        assert_eq!(state.queued_files, 2);
        assert_eq!(state.queued_bytes, 5 + 10);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn manager_reports_disabled_without_a_config() {
        let dir = unique_temp_dir("disabled");
        let manager = DataSyncManager::spawn(None, dir.clone());
        // Give the actor a tick to publish its initial state.
        tokio::time::sleep(Duration::from_millis(10)).await;

        let status = manager.status();
        assert!(!status.enabled);
        assert_eq!(status.connectivity, Connectivity::Disabled);

        manager.pause().await.unwrap();
        manager.resume().await.unwrap();
        manager.retry_now().await.unwrap();
        manager.shutdown().await.unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn manager_pause_and_resume_update_status() {
        let dir = unique_temp_dir("pause-resume");
        let config = SyncConfig {
            remote_host: "127.0.0.1".to_owned(),
            remote_port: 65535,
            remote_user: "nobody".to_owned(),
            identity_file: dir.join("missing-identity"),
            known_hosts_file: dir.join("missing-known-hosts"),
        };
        let manager = DataSyncManager::spawn(Some(config), dir.clone());
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(manager.status().enabled);
        assert!(!manager.status().paused);

        manager.pause().await.unwrap();
        assert!(manager.status().paused);

        manager.resume().await.unwrap();
        assert!(!manager.status().paused);

        manager.shutdown().await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
}
