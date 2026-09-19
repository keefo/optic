use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::{debug, warn};

use crate::{
    camera::{CameraError, CaptureRequest, CaptureResult, PreviewFrame, StreamRequest},
    native_camera::NativeCameraBackend,
};

const COMMAND_QUEUE_CAPACITY: usize = 16;

/// The single entry point for all camera operations.
///
/// The actor owns the native backend and executes commands in FIFO order.
#[derive(Clone)]
pub struct OpticCamera {
    inner: Arc<OpticCameraInner>,
}

struct OpticCameraInner {
    commands: mpsc::Sender<CameraCommand>,
    state: watch::Receiver<ActorState>,
    queued_commands: Arc<AtomicUsize>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraBackendKind {
    NativeLibcamera,
}

#[derive(Debug, Clone, Copy)]
struct ActorState {
    busy: bool,
    streaming: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct CameraRuntimeStatus {
    pub backend: CameraBackendKind,
    pub busy: bool,
    pub streaming: bool,
    pub queued_commands: usize,
}

enum CameraCommand {
    Probe {
        reply: oneshot::Sender<Option<String>>,
    },
    StartStream {
        request: StreamRequest,
        reply: oneshot::Sender<Result<(), CameraError>>,
    },
    ReconfigureStream {
        request: StreamRequest,
        reply: oneshot::Sender<Result<(), CameraError>>,
    },
    StopStream {
        reply: oneshot::Sender<bool>,
    },
    Subscribe {
        reply: oneshot::Sender<Result<broadcast::Receiver<PreviewFrame>, CameraError>>,
    },
    CaptureToStage {
        capture_dir: PathBuf,
        request: CaptureRequest,
        reply: oneshot::Sender<Result<CaptureResult, CameraError>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

impl OpticCamera {
    pub fn spawn_native() -> Self {
        let backend = NativeCameraBackend::spawn();
        let streaming = backend.subscribe_streaming();
        let (commands, receiver) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let (state_sender, state) = watch::channel(ActorState {
            busy: false,
            streaming: false,
        });
        let queued_commands = Arc::new(AtomicUsize::new(0));

        tokio::spawn(run_actor(
            backend,
            receiver,
            streaming,
            state_sender,
            queued_commands.clone(),
        ));

        Self {
            inner: Arc::new(OpticCameraInner {
                commands,
                state,
                queued_commands,
            }),
        }
    }

    pub fn status(&self) -> CameraRuntimeStatus {
        let state = *self.inner.state.borrow();
        CameraRuntimeStatus {
            backend: CameraBackendKind::NativeLibcamera,
            busy: state.busy,
            streaming: state.streaming,
            queued_commands: self.inner.queued_commands.load(Ordering::Acquire),
        }
    }

    pub async fn probe(&self) -> Option<String> {
        let (reply, response) = oneshot::channel();
        self.enqueue(CameraCommand::Probe { reply }).await.ok()?;
        response.await.ok().flatten()
    }

    pub async fn start_stream(&self, request: StreamRequest) -> Result<(), CameraError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(CameraCommand::StartStream { request, reply })
            .await?;
        receive(response).await?
    }

    pub async fn reconfigure_stream(&self, request: StreamRequest) -> Result<(), CameraError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(CameraCommand::ReconfigureStream { request, reply })
            .await?;
        receive(response).await?
    }

    pub async fn stop_stream(&self) -> Result<bool, CameraError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(CameraCommand::StopStream { reply }).await?;
        receive(response).await
    }

    pub async fn subscribe(&self) -> Result<broadcast::Receiver<PreviewFrame>, CameraError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(CameraCommand::Subscribe { reply }).await?;
        receive(response).await?
    }

    pub async fn capture_to_stage(
        &self,
        capture_dir: &Path,
        request: CaptureRequest,
    ) -> Result<CaptureResult, CameraError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(CameraCommand::CaptureToStage {
            capture_dir: capture_dir.to_path_buf(),
            request,
            reply,
        })
        .await?;
        receive(response).await?
    }

    pub async fn shutdown(&self) -> Result<(), CameraError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(CameraCommand::Shutdown { reply }).await?;
        receive(response).await
    }

    async fn enqueue(&self, command: CameraCommand) -> Result<(), CameraError> {
        let permit = self
            .inner
            .commands
            .reserve()
            .await
            .map_err(|_| CameraError::Unavailable)?;
        self.inner.queued_commands.fetch_add(1, Ordering::Release);
        permit.send(command);
        Ok(())
    }
}

async fn receive<T>(response: oneshot::Receiver<T>) -> Result<T, CameraError> {
    response.await.map_err(|_| CameraError::Unavailable)
}

async fn run_actor(
    backend: NativeCameraBackend,
    mut commands: mpsc::Receiver<CameraCommand>,
    mut streaming: watch::Receiver<bool>,
    state_sender: watch::Sender<ActorState>,
    queued_commands: Arc<AtomicUsize>,
) {
    debug!("optic_camera actor started with native libcamera backend");
    loop {
        let command = tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    break;
                };
                command
            }
            changed = streaming.changed() => {
                if changed.is_ok() {
                    publish_streaming(&state_sender, *streaming.borrow());
                }
                continue;
            }
        };
        queued_commands.fetch_sub(1, Ordering::AcqRel);
        publish_state(&state_sender, true, backend.is_streaming().await);

        let shutdown = match command {
            CameraCommand::Probe { reply } => {
                let _ = reply.send(backend.probe().await);
                false
            }
            CameraCommand::StartStream { request, reply } => {
                let _ = reply.send(backend.start_stream(request).await);
                false
            }
            CameraCommand::ReconfigureStream { request, reply } => {
                let _ = reply.send(backend.reconfigure_stream(request).await);
                false
            }
            CameraCommand::StopStream { reply } => {
                let _ = reply.send(backend.stop_stream().await);
                false
            }
            CameraCommand::Subscribe { reply } => {
                let _ = reply.send(backend.subscribe().await);
                false
            }
            CameraCommand::CaptureToStage {
                capture_dir,
                request,
                reply,
            } => {
                let _ = reply.send(backend.capture_to_stage(&capture_dir, request).await);
                false
            }
            CameraCommand::Shutdown { reply } => {
                backend.shutdown().await;
                let _ = reply.send(());
                true
            }
        };

        publish_state(&state_sender, false, backend.is_streaming().await);
        if shutdown {
            debug!("optic_camera actor shut down");
            return;
        }
    }

    let was_streaming = backend.is_streaming().await;
    backend.shutdown().await;
    if was_streaming {
        warn!("optic_camera clients dropped while preview was active; preview stopped");
    }
    publish_state(&state_sender, false, false);
}

fn publish_state(state: &watch::Sender<ActorState>, busy: bool, streaming: bool) {
    state.send_if_modified(|current| {
        let changed = current.busy != busy || current.streaming != streaming;
        current.busy = busy;
        current.streaming = streaming;
        changed
    });
}

fn publish_streaming(state: &watch::Sender<ActorState>, streaming: bool) {
    state.send_if_modified(|current| {
        let changed = current.streaming != streaming;
        current.streaming = streaming;
        changed
    });
}

#[cfg(all(test, not(target_os = "linux")))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn actor_exposes_backend_and_lifecycle_state() {
        let camera = OpticCamera::spawn_native();

        let status = camera.status();
        assert!(matches!(status.backend, CameraBackendKind::NativeLibcamera));
        assert!(!status.busy);
        assert!(!status.streaming);
        assert_eq!(status.queued_commands, 0);
        assert!(camera.probe().await.is_none());
        camera.shutdown().await.unwrap();
    }
}
