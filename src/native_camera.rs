#[cfg(target_os = "linux")]
mod imp {
    use std::{
        collections::VecDeque,
        fs, io,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
            mpsc as std_mpsc,
        },
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use bytes::Bytes;
    use libcamera::{
        camera::{
            ActiveCamera, CameraConfiguration, CameraConfigurationStatus, Orientation,
            SensorConfiguration,
        },
        camera_manager::CameraManager,
        control::ControlList,
        controls,
        framebuffer::AsFrameBuffer,
        framebuffer_allocator::{FrameBuffer, FrameBufferAllocator},
        framebuffer_map::MemoryMappedFrameBuffer,
        geometry::Size,
        pixel_format::PixelFormat,
        properties,
        request::{Request, RequestStatus, ReuseFlag},
        stream::{Stream, StreamRole},
    };
    use tokio::sync::{broadcast, oneshot, watch};
    use tracing::{debug, info, warn};

    use crate::{
        camera::{
            CameraError, CameraSettings, CaptureExposure, CaptureFile, CaptureProfile,
            CaptureRequest, CaptureResult, CaptureSource, PreviewFrame, StreamRequest,
            format_rule_tags, still_frame_duration_limits_us,
        },
        exif::{ExifMetadata, exif_app1, insert_exif},
        exposure_ramp::meter_yuv420,
        native_codec::{DngMetadata, decode_pisp_comp1, encode_bayer16_dng, encode_yuv420_jpeg},
    };

    const FRAME_CHANNEL_CAPACITY: usize = 4;
    const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(5);
    const CAPTURE_WARMUP_FRAMES: usize = 8;
    static LAST_CAPTURE_SUFFIX: AtomicU64 = AtomicU64::new(0);

    pub(crate) struct NativeCameraBackend {
        commands: std_mpsc::Sender<NativeCommand>,
        frames: broadcast::Sender<PreviewFrame>,
        streaming: watch::Sender<bool>,
        worker: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
    }

    enum NativeCommand {
        Probe(oneshot::Sender<Option<String>>),
        StartStream(StreamRequest, oneshot::Sender<Result<(), CameraError>>),
        ReconfigureStream(StreamRequest, oneshot::Sender<Result<(), CameraError>>),
        StopStream(oneshot::Sender<bool>),
        Capture(
            PathBuf,
            CaptureRequest,
            oneshot::Sender<Result<CaptureResult, CameraError>>,
        ),
        Shutdown(oneshot::Sender<()>),
    }

    #[derive(Clone)]
    struct StreamInfo {
        width: u32,
        height: u32,
        stride: u32,
        format: String,
    }

    struct Pipeline {
        profile: CaptureProfile,
        settings: CameraSettings,
        streaming: bool,
        downsample: bool,
        streams: Vec<Stream>,
        preview_index: Option<usize>,
        still_index: usize,
        raw_index: Option<usize>,
        preview_info: Option<StreamInfo>,
        still_info: StreamInfo,
        raw_info: Option<StreamInfo>,
        completed: std_mpsc::Receiver<Request>,
        control_revision: u64,
        request_revisions: Vec<u64>,
        request_count: usize,
        frame_count: usize,
    }

    struct CapturedFrame {
        yuv: Vec<u8>,
        raw: Option<Vec<u8>>,
        metadata: DngMetadata,
        exposure: CaptureExposure,
        still: StreamInfo,
        raw_info: Option<StreamInfo>,
    }

    impl NativeCameraBackend {
        pub(crate) fn spawn() -> Self {
            let (commands, receiver) = std_mpsc::channel();
            let (frames, _) = broadcast::channel(FRAME_CHANNEL_CAPACITY);
            let (streaming, _) = watch::channel(false);
            let worker_frames = frames.clone();
            let worker_streaming = streaming.clone();
            let worker = thread::Builder::new()
                .name("optic-libcamera".to_owned())
                .spawn(move || run_worker(receiver, worker_frames, worker_streaming))
                .expect("failed to spawn native camera worker");
            Self {
                commands,
                frames,
                streaming,
                worker: Arc::new(Mutex::new(Some(worker))),
            }
        }

        pub(crate) fn subscribe_streaming(&self) -> watch::Receiver<bool> {
            self.streaming.subscribe()
        }

        pub(crate) async fn is_streaming(&self) -> bool {
            *self.streaming.borrow()
        }

        pub(crate) async fn probe(&self) -> Option<String> {
            let (reply, response) = oneshot::channel();
            self.commands.send(NativeCommand::Probe(reply)).ok()?;
            response.await.ok().flatten()
        }

        pub(crate) async fn start_stream(&self, request: StreamRequest) -> Result<(), CameraError> {
            request.validate()?;
            let (reply, response) = oneshot::channel();
            self.send(NativeCommand::StartStream(request, reply))?;
            response.await.map_err(|_| CameraError::Unavailable)?
        }

        pub(crate) async fn reconfigure_stream(
            &self,
            request: StreamRequest,
        ) -> Result<(), CameraError> {
            request.validate()?;
            let (reply, response) = oneshot::channel();
            self.send(NativeCommand::ReconfigureStream(request, reply))?;
            response.await.map_err(|_| CameraError::Unavailable)?
        }

        pub(crate) async fn stop_stream(&self) -> bool {
            let (reply, response) = oneshot::channel();
            if self.send(NativeCommand::StopStream(reply)).is_err() {
                return false;
            }
            response.await.unwrap_or(false)
        }

        pub(crate) async fn subscribe(
            &self,
        ) -> Result<broadcast::Receiver<PreviewFrame>, CameraError> {
            if !*self.streaming.borrow() {
                return Err(CameraError::NotStreaming);
            }
            Ok(self.frames.subscribe())
        }

        pub(crate) async fn capture_to_stage(
            &self,
            capture_dir: &Path,
            request: CaptureRequest,
        ) -> Result<CaptureResult, CameraError> {
            request.validate()?;
            let (reply, response) = oneshot::channel();
            self.send(NativeCommand::Capture(
                capture_dir.to_path_buf(),
                request,
                reply,
            ))?;
            response.await.map_err(|_| CameraError::Unavailable)?
        }

        pub(crate) async fn shutdown(&self) {
            let (reply, response) = oneshot::channel();
            if self.send(NativeCommand::Shutdown(reply)).is_ok() {
                let _ = response.await;
            }
            if let Some(worker) = self.worker.lock().unwrap().take() {
                let _ = worker.join();
            }
        }

        fn send(&self, command: NativeCommand) -> Result<(), CameraError> {
            self.commands
                .send(command)
                .map_err(|_| CameraError::Unavailable)
        }
    }

    fn backend_error(error: impl std::fmt::Display) -> CameraError {
        CameraError::Backend(error.to_string())
    }

    fn run_worker(
        commands: std_mpsc::Receiver<NativeCommand>,
        frames: broadcast::Sender<PreviewFrame>,
        streaming_state: watch::Sender<bool>,
    ) {
        if let Err(error) = run_camera(commands, frames, &streaming_state) {
            warn!(%error, "native camera worker stopped");
        }
        streaming_state.send_replace(false);
    }

    fn run_camera(
        commands: std_mpsc::Receiver<NativeCommand>,
        frames: broadcast::Sender<PreviewFrame>,
        streaming_state: &watch::Sender<bool>,
    ) -> Result<(), CameraError> {
        let manager = CameraManager::new()?;
        let cameras = manager.cameras();
        let camera = cameras
            .iter()
            .find(|camera| {
                camera
                    .properties()
                    .get::<properties::Model>()
                    .map(|model| model.0.to_ascii_lowercase().contains("imx477"))
                    .unwrap_or(false)
            })
            .ok_or(CameraError::Unavailable)?;
        let model = camera
            .properties()
            .get::<properties::Model>()
            .map_err(backend_error)?
            .0;
        let sensor = format!("{} : {} ({})", camera.id(), model, camera.id());
        let mut camera = camera.acquire()?;
        let mut pipeline: Option<Pipeline> = None;
        debug!(camera = %sensor, "native libcamera worker ready");

        loop {
            let mut pipeline_failed = false;
            if let Some(active) = &mut pipeline
                && let Ok(request) = active.completed.try_recv()
                && let Err(error) =
                    process_completed_request(&camera, active, request, &frames, &model)
            {
                warn!(%error, "native camera frame processing failed");
                pipeline_failed = true;
            }
            if pipeline_failed {
                stop_pipeline(&mut camera, &mut pipeline, streaming_state)?;
            }

            let command = if pipeline.is_some() {
                match commands.recv_timeout(COMMAND_POLL_INTERVAL) {
                    Ok(command) => Some(command),
                    Err(std_mpsc::RecvTimeoutError::Timeout) => None,
                    Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match commands.recv() {
                    Ok(command) => Some(command),
                    Err(_) => break,
                }
            };

            let Some(command) = command else { continue };
            if handle_command(
                command,
                &mut camera,
                &mut pipeline,
                &frames,
                streaming_state,
                &sensor,
                &model,
            )? {
                break;
            }
        }

        stop_pipeline(&mut camera, &mut pipeline, streaming_state)?;
        Ok(())
    }

    fn handle_command(
        command: NativeCommand,
        camera: &mut ActiveCamera<'_>,
        pipeline: &mut Option<Pipeline>,
        _frames: &broadcast::Sender<PreviewFrame>,
        streaming_state: &watch::Sender<bool>,
        sensor: &str,
        model: &str,
    ) -> Result<bool, CameraError> {
        match command {
            NativeCommand::Probe(reply) => {
                let _ = reply.send(Some(sensor.to_owned()));
            }
            NativeCommand::StartStream(request, reply) => {
                let result = if pipeline.is_some() {
                    Err(CameraError::Busy)
                } else {
                    start_pipeline(
                        camera,
                        request.profile,
                        request.effective_settings(),
                        request.control_revision,
                        true,
                        false,
                        request.downsample,
                    )
                    .map(|active| {
                        *pipeline = Some(active);
                        streaming_state.send_replace(true);
                    })
                };
                let _ = reply.send(result);
            }
            NativeCommand::ReconfigureStream(request, reply) => {
                let is_streaming = pipeline.as_ref().is_some_and(|active| active.streaming);
                let controls_only = pipeline.as_ref().is_some_and(|active| {
                    active.downsample == request.downsample
                        && !stream_configuration_changed(
                            active.profile,
                            &active.settings,
                            request.profile,
                            &request.effective_settings(),
                        )
                });
                let result = if is_streaming && controls_only {
                    let active = pipeline.as_mut().expect("streaming pipeline disappeared");
                    active.settings = request.effective_settings();
                    active.control_revision = request.control_revision;
                    Ok(())
                } else if is_streaming {
                    stop_pipeline(camera, pipeline, streaming_state).and_then(|_| {
                        start_pipeline(
                            camera,
                            request.profile,
                            request.effective_settings(),
                            request.control_revision,
                            true,
                            false,
                            request.downsample,
                        )
                        .map(|active| {
                            *pipeline = Some(active);
                            streaming_state.send_replace(true);
                        })
                    })
                } else {
                    Err(CameraError::NotStreaming)
                };
                let _ = reply.send(result);
            }
            NativeCommand::StopStream(reply) => {
                let was_streaming = pipeline.as_ref().is_some_and(|active| active.streaming);
                if let Err(error) = stop_pipeline(camera, pipeline, streaming_state) {
                    warn!(%error, "failed to stop native preview");
                }
                let _ = reply.send(was_streaming);
            }
            NativeCommand::Capture(capture_dir, request, reply) => {
                let profile = request.profile;
                let settings = request.settings.clone();
                let save_dng = request.save_dng;
                let total_started = Instant::now();

                let stop_started = Instant::now();
                let stop_result = stop_pipeline(camera, pipeline, streaming_state);
                info!(
                    stage = "stop_existing_preview",
                    elapsed_ms = stop_started.elapsed().as_millis(),
                    "capture perf"
                );

                let result = stop_result.and_then(|_| {
                    let frame_started = Instant::now();
                    let frame_result = capture_frame(camera, profile, settings, save_dng, model);
                    info!(
                        stage = "capture_frame_total",
                        elapsed_ms = frame_started.elapsed().as_millis(),
                        "capture perf"
                    );
                    frame_result.and_then(|mut frame| {
                        let meter_started = Instant::now();
                        frame.exposure.meter = meter_yuv420(
                            &frame.yuv,
                            frame.still.width,
                            frame.still.height,
                            frame.still.stride,
                        );
                        info!(
                            stage = "meter",
                            luminance = frame.exposure.meter.map(|meter| meter.luminance),
                            elapsed_ms = meter_started.elapsed().as_millis(),
                            "capture perf"
                        );
                        let publish_started = Instant::now();
                        let publish_result = publish_capture(&capture_dir, request, frame);
                        info!(
                            stage = "publish_capture",
                            elapsed_ms = publish_started.elapsed().as_millis(),
                            "capture perf"
                        );
                        publish_result
                    })
                });
                info!(
                    ?profile,
                    save_dng,
                    stage = "total",
                    elapsed_ms = total_started.elapsed().as_millis(),
                    "capture perf"
                );
                let _ = reply.send(result);
            }
            NativeCommand::Shutdown(reply) => {
                let _ = stop_pipeline(camera, pipeline, streaming_state);
                let _ = reply.send(());
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn stream_configuration_changed(
        current_profile: CaptureProfile,
        current: &CameraSettings,
        requested_profile: CaptureProfile,
        requested: &CameraSettings,
    ) -> bool {
        current_profile != requested_profile
            || current.rotation != requested.rotation
            || current.horizontal_flip != requested.horizontal_flip
            || current.vertical_flip != requested.vertical_flip
    }

    fn start_pipeline(
        camera: &mut ActiveCamera<'_>,
        profile: CaptureProfile,
        settings: CameraSettings,
        control_revision: u64,
        streaming: bool,
        save_dng: bool,
        downsample: bool,
    ) -> Result<Pipeline, CameraError> {
        let roles: &[StreamRole] = if streaming {
            &[StreamRole::ViewFinder]
        } else if save_dng {
            &[StreamRole::StillCapture, StreamRole::Raw]
        } else {
            &[StreamRole::StillCapture]
        };
        let mut config = camera
            .generate_configuration(roles)
            .ok_or_else(|| backend_error("camera rejected requested stream roles"))?;
        config.set_orientation(orientation(&settings));
        if let Some((width, height)) = streaming
            .then(|| profile.preview_spec(downsample).sensor_mode)
            .flatten()
        {
            let mut sensor = SensorConfiguration::new();
            sensor.set_bit_depth(12);
            sensor.set_output_size(width, height);
            config.set_sensor_configuration(sensor);
        }

        let output = if streaming {
            let spec = profile.preview_spec(downsample);
            (spec.width, spec.height, 4)
        } else {
            let spec = profile.spec();
            (spec.width, spec.height, 3)
        };
        configure_stream(
            &mut config,
            0,
            pixel_format("YUV420")?,
            output.0,
            output.1,
            output.2,
        )?;
        if save_dng {
            let spec = profile.spec();
            configure_stream(
                &mut config,
                1,
                pixel_format(profile.raw_format())?,
                spec.width,
                spec.height,
                3,
            )?;
        }

        let validation = config.validate();
        if matches!(validation, CameraConfigurationStatus::Invalid) {
            return Err(backend_error("camera configuration is invalid"));
        }
        let still_info = stream_info(&config, 0)?;
        if still_info.format != "YUV420"
            || still_info.width != output.0
            || still_info.height != output.1
        {
            return Err(backend_error(format!(
                "camera adjusted processed stream to {} {}x{}",
                still_info.format, still_info.width, still_info.height
            )));
        }
        let raw_info = if save_dng {
            let info = stream_info(&config, 1)?;
            if !info.format.ends_with("_PISP_COMP1")
                || info.width != output.0
                || info.height != output.1
            {
                return Err(backend_error(format!(
                    "camera adjusted raw stream to {} {}x{}",
                    info.format, info.width, info.height
                )));
            }
            Some(info)
        } else {
            None
        };

        camera.configure(&mut config)?;
        let streams = (0..config.len())
            .map(|index| {
                config
                    .get(index)
                    .and_then(|stream| stream.stream())
                    .ok_or_else(|| {
                        backend_error(format!("configured stream {index} is unavailable"))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut allocator = FrameBufferAllocator::new(camera);
        let mut buffers = Vec::with_capacity(streams.len());
        for stream in &streams {
            buffers.push(
                allocator
                    .alloc(stream)?
                    .into_iter()
                    .map(MemoryMappedFrameBuffer::new)
                    .collect::<Result<VecDeque<_>, _>>()
                    .map_err(backend_error)?,
            );
        }
        let request_count = buffers.iter().map(VecDeque::len).min().unwrap_or(0);
        if request_count == 0 {
            return Err(backend_error("camera allocated no buffers"));
        }

        let mut requests = Vec::with_capacity(request_count);
        for cookie in 0..request_count {
            let mut request = camera
                .create_request(Some(cookie as u64))
                .ok_or_else(|| backend_error("failed to create request"))?;
            for (stream, stream_buffers) in streams.iter().zip(&mut buffers) {
                request
                    .add_buffer(
                        stream,
                        stream_buffers
                            .pop_front()
                            .ok_or_else(|| backend_error("buffer allocation became unbalanced"))?,
                    )
                    .map_err(backend_error)?;
            }
            apply_controls(
                &mut request,
                &settings,
                streaming.then_some(profile.preview_spec(downsample).fps),
            )?;
            requests.push(request);
        }

        let (completed_tx, completed) = std_mpsc::channel();
        camera.on_request_completed(move |request| {
            let _ = completed_tx.send(request);
        });
        camera.start(None)?;
        for request in requests {
            if let Err((_, error)) = camera.queue_request(request) {
                let _ = camera.stop();
                return Err(error.into());
            }
        }

        // The configured sensor mode bounds the achievable frame rate; logged
        // per role to compare StillCapture against ViewFinder timing.
        let frame_duration_range = camera
            .controls()
            .find(controls::ControlId::FrameDurationLimits as u32)
            .ok()
            .map(|info| format!("{:?}..{:?}", info.min(), info.max()));
        info!(
            ?profile,
            streaming,
            processed = %format!("{}x{}", still_info.width, still_info.height),
            raw = raw_info.as_ref().map(|value| value.format.as_str()),
            frame_duration_range_us = frame_duration_range.as_deref(),
            "native camera pipeline started"
        );
        Ok(Pipeline {
            profile,
            settings,
            streaming,
            downsample,
            streams,
            preview_index: streaming.then_some(0),
            still_index: 0,
            raw_index: raw_info.as_ref().map(|_| 1),
            preview_info: streaming.then(|| StreamInfo {
                width: still_info.width,
                height: still_info.height,
                stride: still_info.stride,
                format: still_info.format.clone(),
            }),
            still_info,
            raw_info,
            completed,
            control_revision,
            request_revisions: vec![control_revision; request_count],
            request_count,
            frame_count: 0,
        })
    }

    fn pixel_format(name: &str) -> Result<PixelFormat, CameraError> {
        PixelFormat::parse(name)
            .ok_or_else(|| backend_error(format!("unknown pixel format {name}")))
    }

    fn orientation(settings: &CameraSettings) -> Orientation {
        match (
            settings.rotation,
            settings.horizontal_flip,
            settings.vertical_flip,
        ) {
            (0, false, false) | (180, true, true) => Orientation::Rotate0,
            (0, true, false) | (180, false, true) => Orientation::Rotate0Mirror,
            (0, false, true) | (180, true, false) => Orientation::Rotate180Mirror,
            (0, true, true) | (180, false, false) => Orientation::Rotate180,
            _ => unreachable!("camera settings are validated before configuration"),
        }
    }

    fn configure_stream(
        config: &mut CameraConfiguration,
        index: usize,
        format: PixelFormat,
        width: u32,
        height: u32,
        buffers: u32,
    ) -> Result<(), CameraError> {
        let mut stream = config
            .get_mut(index)
            .ok_or_else(|| backend_error(format!("configuration has no stream {index}")))?;
        stream.set_pixel_format(format);
        stream.set_size(Size::new(width, height));
        stream.set_buffer_count(buffers);
        Ok(())
    }

    fn stream_info(config: &CameraConfiguration, index: usize) -> Result<StreamInfo, CameraError> {
        let stream = config
            .get(index)
            .ok_or_else(|| backend_error(format!("configuration has no stream {index}")))?;
        let size = stream.get_size();
        Ok(StreamInfo {
            width: size.width,
            height: size.height,
            stride: stream.get_stride(),
            format: stream.get_pixel_format().to_string(),
        })
    }

    fn apply_controls(
        request: &mut Request,
        settings: &CameraSettings,
        fps: Option<u8>,
    ) -> Result<(), CameraError> {
        let controls = request.controls_mut();

        // Permanent AEC Bypass: Pure Manual open-loop
        controls
            .set(controls::AeEnable(false))
            .map_err(backend_error)?;
        // Manual colour gains (exposure ramping's eased white balance)
        // replace per-frame AWB; otherwise AWB runs in the chosen mode.
        if let Some(gains) = settings.colour_gains {
            controls
                .set(controls::AwbEnable(false))
                .map_err(backend_error)?;
            controls
                .set(controls::ColourGains(gains))
                .map_err(backend_error)?;
        } else {
            controls
                .set(controls::AwbEnable(true))
                .map_err(backend_error)?;
            controls
                .set(controls::AwbMode::from_setting(&settings.awb))
                .map_err(backend_error)?;
        }
        controls
            .set(noise_reduction_mode(&settings.denoise, fps.is_some()))
            .map_err(backend_error)?;

        // Force Shutter Speed manually
        let exposure = i32::try_from(settings.shutter_us)
            .map_err(|_| CameraError::Invalid("shutter exceeds libcamera range"))?;
        controls
            .set(controls::ExposureTimeMode::Manual)
            .map_err(backend_error)?;
        controls
            .set(controls::ExposureTime(exposure))
            .map_err(backend_error)?;

        // Force Analogue Gain manually
        controls
            .set(controls::AnalogueGainMode::Manual)
            .map_err(backend_error)?;
        controls
            .set(controls::AnalogueGain(settings.gain))
            .map_err(backend_error)?;

        if let Some(fps) = fps {
            let frame_us = (1_000_000_i64 / i64::from(fps)).max(
                i64::try_from(settings.shutter_us)
                    .map_err(|_| CameraError::Invalid("shutter exceeds libcamera range"))?,
            );
            controls
                .set(controls::FrameDurationLimits([frame_us, frame_us]))
                .map_err(backend_error)?;
        } else {
            // Still capture: never leave the frame duration to libcamera's
            // default or a previous preview's limit (~500 ms/frame observed).
            controls
                .set(controls::FrameDurationLimits(
                    still_frame_duration_limits_us(settings.shutter_us),
                ))
                .map_err(backend_error)?;
        }
        Ok(())
    }

    trait FromSetting {
        fn from_setting(value: &str) -> Self;
    }

    impl FromSetting for controls::AwbMode {
        fn from_setting(value: &str) -> Self {
            match value {
                "auto" => Self::AwbAuto,
                "daylight" => Self::AwbDaylight,
                "cloudy" => Self::AwbCloudy,
                "tungsten" => Self::AwbTungsten,
                "fluorescent" => Self::AwbFluorescent,
                "indoor" => Self::AwbIndoor,
                "incandescent" => Self::AwbIncandescent,
                _ => unreachable!("camera settings are validated before control mapping"),
            }
        }
    }

    fn noise_reduction_mode(value: &str, streaming: bool) -> controls::NoiseReductionMode {
        match value {
            "off" => controls::NoiseReductionMode::Off,
            "cdn_off" => controls::NoiseReductionMode::Minimal,
            "cdn_fast" => controls::NoiseReductionMode::Fast,
            "cdn_hq" => controls::NoiseReductionMode::HighQuality,
            "auto" if streaming => controls::NoiseReductionMode::Fast,
            "auto" => controls::NoiseReductionMode::HighQuality,
            _ => unreachable!("camera settings are validated before control mapping"),
        }
    }

    fn capture_frame(
        camera: &mut ActiveCamera<'_>,
        profile: CaptureProfile,
        settings: CameraSettings,
        save_dng: bool,
        model: &str,
    ) -> Result<CapturedFrame, CameraError> {
        let pipeline_started = Instant::now();
        let mut pipeline = start_pipeline(camera, profile, settings, 0, false, save_dng, false)?;
        info!(
            stage = "pipeline_start",
            elapsed_ms = pipeline_started.elapsed().as_millis(),
            "capture perf"
        );

        let warmup_started = Instant::now();
        let mut last_frame_at = warmup_started;
        let result = (|| {
            let target = pipeline.request_count + CAPTURE_WARMUP_FRAMES;
            let mut captured = None;
            while pipeline.frame_count < target {
                let mut request = pipeline
                    .completed
                    .recv_timeout(Duration::from_secs(10))
                    .map_err(|_| CameraError::Timeout)?;
                if request.status() != RequestStatus::Complete {
                    return Err(backend_error(format!(
                        "capture request completed as {:?}",
                        request.status()
                    )));
                }
                pipeline.frame_count += 1;

                // Distinguishes "libcamera/sensor is slow to deliver frames"
                // (large since_previous_ms, small exposure_us) from "the
                // requested exposure itself is long" (exposure_us tracks
                // since_previous_ms). exposure_us is read back from the
                // completed request's own metadata — the value libcamera
                // actually used, not just what we asked for.
                let now = Instant::now();
                let since_previous_ms = now.duration_since(last_frame_at).as_millis();
                last_frame_at = now;
                let exposure_us = request
                    .metadata()
                    .get::<controls::ExposureTime>()
                    .ok()
                    .map(|value| value.0);
                let frame_duration_us = request
                    .metadata()
                    .get::<controls::FrameDuration>()
                    .ok()
                    .map(|value| value.0);
                // AWB and (with auto shutter) exposure still converge during
                // warmup; these show how many warmup frames they need.
                let colour_gains = request
                    .metadata()
                    .get::<controls::ColourGains>()
                    .ok()
                    .map(|value| value.0);
                let analogue_gain = request
                    .metadata()
                    .get::<controls::AnalogueGain>()
                    .ok()
                    .map(|value| value.0);
                info!(
                    stage = "warmup_frame",
                    frame_index = pipeline.frame_count,
                    exposure_us,
                    analogue_gain,
                    frame_duration_us,
                    ?colour_gains,
                    since_previous_ms,
                    "capture perf"
                );

                if pipeline.frame_count == target {
                    let yuv = copy_buffer(
                        request
                            .buffer(&pipeline.streams[pipeline.still_index])
                            .ok_or_else(|| {
                                backend_error("capture request lost processed buffer")
                            })?,
                    )?;
                    let raw = match pipeline.raw_index {
                        Some(index) => Some(copy_buffer(
                            request
                                .buffer(&pipeline.streams[index])
                                .ok_or_else(|| backend_error("capture request lost raw buffer"))?,
                        )?),
                        None => None,
                    };
                    captured = Some(CapturedFrame {
                        yuv,
                        raw,
                        metadata: dng_metadata(request.metadata(), &pipeline.raw_info, model),
                        exposure: CaptureExposure {
                            exposure_us,
                            analogue_gain,
                            colour_gains,
                            meter: None,
                        },
                        still: pipeline.still_info.clone(),
                        raw_info: pipeline.raw_info.clone(),
                    });
                } else {
                    request.reuse(ReuseFlag::REUSE_BUFFERS);
                    apply_controls(&mut request, &pipeline.settings, None)?;
                    camera.queue_request(request).map_err(|(_, error)| error)?;
                }
            }
            captured.ok_or_else(|| backend_error("capture completed without an output frame"))
        })();
        info!(
            stage = "warmup_and_capture",
            frames = pipeline.frame_count,
            elapsed_ms = warmup_started.elapsed().as_millis(),
            "capture perf"
        );

        let stop_started = Instant::now();
        let stop_result = camera.stop().map_err(CameraError::from);
        info!(
            stage = "camera_stop",
            elapsed_ms = stop_started.elapsed().as_millis(),
            "capture perf"
        );
        match (result, stop_result) {
            (Ok(frame), Ok(())) => Ok(frame),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(stop_error)) => {
                warn!(%stop_error, "failed to stop native camera after capture error");
                Err(error)
            }
        }
    }

    fn process_completed_request(
        camera: &ActiveCamera<'_>,
        pipeline: &mut Pipeline,
        mut request: Request,
        frames: &broadcast::Sender<PreviewFrame>,
        _model: &str,
    ) -> Result<(), CameraError> {
        if request.status() != RequestStatus::Complete {
            return Err(backend_error(format!(
                "preview request completed as {:?}",
                request.status()
            )));
        }
        pipeline.frame_count += 1;
        let request_index = usize::try_from(request.cookie())
            .map_err(|_| backend_error("preview request cookie exceeds platform limits"))?;
        let control_revision = *pipeline
            .request_revisions
            .get(request_index)
            .ok_or_else(|| backend_error("preview request cookie is out of range"))?;
        if let (Some(index), Some(info)) = (pipeline.preview_index, &pipeline.preview_info) {
            let yuv = copy_buffer(
                request
                    .buffer(&pipeline.streams[index])
                    .ok_or_else(|| backend_error("preview request lost its buffer"))?,
            )?;
            let jpeg = encode_yuv420_jpeg(&yuv, info.width, info.height, info.stride, 95)?;
            let meter = meter_yuv420(&yuv, info.width, info.height, info.stride);
            let _ = frames.send(PreviewFrame {
                jpeg: Bytes::from(jpeg),
                sequence: request.sequence(),
                control_revision,
                ae_state: ae_state(request.metadata()),
                awb_state: awb_state(request.metadata()),
                exposure_us: request
                    .metadata()
                    .get::<controls::ExposureTime>()
                    .ok()
                    .map(|value| value.0),
                analogue_gain: request
                    .metadata()
                    .get::<controls::AnalogueGain>()
                    .ok()
                    .map(|value| value.0),
                colour_gains: request
                    .metadata()
                    .get::<controls::ColourGains>()
                    .ok()
                    .map(|value| value.0),
                meter,
            });
        }
        request.reuse(ReuseFlag::REUSE_BUFFERS);
        apply_controls(
            &mut request,
            &pipeline.settings,
            pipeline
                .streaming
                .then_some(pipeline.profile.preview_spec(pipeline.downsample).fps),
        )?;
        pipeline.request_revisions[request_index] = pipeline.control_revision;
        camera.queue_request(request).map_err(|(_, error)| error)?;
        Ok(())
    }

    fn ae_state(metadata: &ControlList) -> Option<&'static str> {
        metadata
            .get::<controls::AeState>()
            .ok()
            .map(|state| match state {
                controls::AeState::Idle => "idle",
                controls::AeState::Searching => "searching",
                controls::AeState::Converged => "converged",
            })
    }

    fn awb_state(metadata: &ControlList) -> Option<&'static str> {
        metadata
            .get::<controls::AwbState>()
            .ok()
            .map(|state| match state {
                controls::AwbState::Inactive => "inactive",
                controls::AwbState::Searching => "searching",
                controls::AwbState::AwbConverged => "converged",
                controls::AwbState::AwbLocked => "locked",
            })
    }

    fn stop_pipeline(
        camera: &mut ActiveCamera<'_>,
        pipeline: &mut Option<Pipeline>,
        streaming_state: &watch::Sender<bool>,
    ) -> Result<(), CameraError> {
        let Some(active) = pipeline.as_ref() else {
            streaming_state.send_replace(false);
            return Ok(());
        };
        camera.stop()?;
        let profile = active.profile;
        drop(pipeline.take());
        streaming_state.send_replace(false);
        info!(?profile, "native camera pipeline stopped");
        Ok(())
    }

    fn dng_metadata(
        metadata: &ControlList,
        raw_info: &Option<StreamInfo>,
        model: &str,
    ) -> DngMetadata {
        let mut output = DngMetadata {
            model: model.to_owned(),
            ..DngMetadata::default()
        };
        if let Some(info) = raw_info {
            output.cfa_pattern = cfa_pattern(&info.format).unwrap_or(output.cfa_pattern);
        }
        if let Ok(value) = metadata.get::<controls::SensorBlackLevels>() {
            output.black_levels = value.0;
        }
        if let Ok(value) = metadata.get::<controls::ExposureTime>() {
            output.exposure_us = value.0;
        }
        if let Ok(value) = metadata.get::<controls::AnalogueGain>() {
            output.analogue_gain = value.0;
        }
        if let Ok(value) = metadata.get::<controls::ColourGains>() {
            output.colour_gains = value.0;
        }
        if let Ok(value) = metadata.get::<controls::ColourCorrectionMatrix>() {
            output.colour_correction_matrix = value.0;
        }
        output
    }

    fn cfa_pattern(format: &str) -> Option<[u8; 4]> {
        if format.starts_with("RGGB") {
            Some([0, 1, 1, 2])
        } else if format.starts_with("GRBG") {
            Some([1, 0, 2, 1])
        } else if format.starts_with("GBRG") {
            Some([1, 2, 0, 1])
        } else if format.starts_with("BGGR") {
            Some([2, 1, 1, 0])
        } else {
            None
        }
    }

    fn set_capture_permissions(path: &Path) -> io::Result<()> {
        fs::set_permissions(path, fs::Permissions::from_mode(0o640))
    }

    fn unique_suffix() -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let mut previous = LAST_CAPTURE_SUFFIX.load(Ordering::Relaxed);
        loop {
            let next = now.max(previous.saturating_add(1));
            match LAST_CAPTURE_SUFFIX.compare_exchange_weak(
                previous,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return next,
                Err(current) => previous = current,
            }
        }
    }

    fn copy_buffer(buffer: &MemoryMappedFrameBuffer<FrameBuffer>) -> Result<Vec<u8>, CameraError> {
        let metadata = buffer
            .metadata()
            .ok_or_else(|| backend_error("completed buffer has no metadata"))?;
        let planes = buffer.data();
        let metadata_planes = metadata.planes();
        if planes.len() != metadata_planes.len() {
            return Err(backend_error(
                "buffer plane metadata count does not match mapping",
            ));
        }
        let capacity = (&metadata_planes)
            .into_iter()
            .map(|plane| plane.bytes_used as usize)
            .sum();
        let mut output = Vec::with_capacity(capacity);
        for (plane, bytes) in planes.into_iter().zip(&metadata_planes) {
            let used = bytes.bytes_used as usize;
            if used > plane.len() {
                return Err(backend_error("buffer metadata exceeds mapped plane"));
            }
            output.extend_from_slice(&plane[..used]);
        }
        Ok(output)
    }

    /// Encodes the frame and gives it EXIF metadata describing what the
    /// sensor actually did (`docs/optic-daemon-capture-log.md` §3.2). A
    /// failure to build the block is logged and skipped rather than failing
    /// the capture: a JPEG without metadata still beats a lost frame.
    fn encode_jpeg(
        frame: &CapturedFrame,
        quality: u8,
        model: &str,
        manual_white_balance: bool,
    ) -> Result<Bytes, CameraError> {
        let jpeg = encode_yuv420_jpeg(
            &frame.yuv,
            frame.still.width,
            frame.still.height,
            frame.still.stride,
            quality,
        )?;
        let metadata = ExifMetadata {
            model: model.to_owned(),
            width: frame.still.width,
            height: frame.still.height,
            date_time: exif_date_time(SystemTime::now()),
            exposure_us: frame.exposure.exposure_us,
            analogue_gain: frame.exposure.analogue_gain,
            colour_gains: frame.exposure.colour_gains,
            manual_white_balance,
        };
        match exif_app1(&metadata) {
            Ok(segment) => Ok(Bytes::from(insert_exif(jpeg, &segment))),
            Err(error) => {
                warn!(%error, "failed to build EXIF metadata; writing JPEG without it");
                Ok(Bytes::from(jpeg))
            }
        }
    }

    /// EXIF wants local civil time as `YYYY:MM:DD HH:MM:SS`. The daemon has
    /// no timezone database dependency here, so this formats UTC, which is
    /// unambiguous and matches the sidecar's unix milliseconds.
    fn exif_date_time(at: SystemTime) -> String {
        let seconds = i64::try_from(at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
            .unwrap_or_default();
        chrono::DateTime::from_timestamp(seconds, 0)
            .map(|stamp| stamp.format("%Y:%m:%d %H:%M:%S").to_string())
            .unwrap_or_default()
    }

    fn publish_capture(
        capture_dir: &Path,
        request: CaptureRequest,
        frame: CapturedFrame,
    ) -> Result<CaptureResult, CameraError> {
        let spec = request.profile.spec();

        let jpeg_started = Instant::now();
        // `colour_gains` set means the operator fixed white balance for this
        // capture; otherwise AWB chose it.
        let jpeg = encode_jpeg(
            &frame,
            spec.jpeg_quality,
            &frame.metadata.model,
            request.settings.colour_gains.is_some() || request.settings.awb != "auto",
        )?;
        info!(
            stage = "jpeg_encode",
            bytes = jpeg.len(),
            elapsed_ms = jpeg_started.elapsed().as_millis(),
            "capture perf"
        );

        let dng = if request.save_dng {
            let dng_started = Instant::now();
            let raw = frame
                .raw
                .as_ref()
                .ok_or_else(|| backend_error("DNG capture has no raw buffer"))?;
            let raw_info = frame
                .raw_info
                .as_ref()
                .ok_or_else(|| backend_error("DNG capture has no raw stream information"))?;
            let pixels = decode_pisp_comp1(raw, raw_info.width, raw_info.height, raw_info.stride)?;
            let encoded =
                encode_bayer16_dng(&pixels, raw_info.width, raw_info.height, &frame.metadata)?;
            info!(
                stage = "dng_decode_and_encode",
                bytes = encoded.len(),
                elapsed_ms = dng_started.elapsed().as_millis(),
                "capture perf"
            );
            Some(encoded)
        } else {
            None
        };

        let basename = match &request.source {
            CaptureSource::WebUi => format!("testshot-{}-{}", spec.slug, unique_suffix()),
            CaptureSource::Scheduler { rule_slugs } => match format_rule_tags(rule_slugs) {
                Some(tags) => format!("scheduler-{}-{tags}-{}", spec.slug, unique_suffix()),
                None => {
                    // Unreachable in practice: a scheduler-fired shot
                    // always has >=1 contributing rule slug. Handled
                    // rather than unwrapped so this can't panic if that
                    // invariant is ever violated.
                    format!("scheduler-{}-{}", spec.slug, unique_suffix())
                }
            },
        };
        let jpeg_filename = format!("{basename}.jpg");
        let jpeg_part = capture_dir.join(format!(".{jpeg_filename}.part"));
        let jpeg_final = capture_dir.join(&jpeg_filename);
        let dng_filename = format!("{basename}.dng");
        let dng_part = capture_dir.join(format!(".{dng_filename}.part"));
        let dng_final = capture_dir.join(&dng_filename);

        let disk_started = Instant::now();
        let publication = (|| -> io::Result<()> {
            fs::write(&jpeg_part, &jpeg)?;
            set_capture_permissions(&jpeg_part)?;
            if let Some(contents) = &dng {
                fs::write(&dng_part, contents)?;
                set_capture_permissions(&dng_part)?;
                fs::rename(&dng_part, &dng_final)?;
            }
            fs::rename(&jpeg_part, &jpeg_final)?;
            Ok(())
        })();
        info!(
            stage = "disk_write",
            elapsed_ms = disk_started.elapsed().as_millis(),
            "capture perf"
        );
        if let Err(error) = publication {
            let _ = fs::remove_file(&jpeg_part);
            let _ = fs::remove_file(&dng_part);
            let _ = fs::remove_file(&jpeg_final);
            let _ = fs::remove_file(&dng_final);
            return Err(error.into());
        }

        let mut files = vec![CaptureFile {
            filename: jpeg_filename,
            bytes: jpeg.len() as u64,
        }];
        if let Some(contents) = dng {
            files.push(CaptureFile {
                filename: dng_filename,
                bytes: contents.len() as u64,
            });
        }
        let bytes = files.iter().map(|file| file.bytes).sum();
        info!(?request.profile, files = files.len(), bytes, "native capture published to RAM stage");
        Ok(CaptureResult {
            profile: request.profile,
            files,
            bytes,
            width: spec.width,
            height: spec.height,
            exposure: Some(frame.exposure),
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn orientation_composes_rotation_and_flips() {
            let settings = CameraSettings::default();
            assert_eq!(orientation(&settings), Orientation::Rotate0);

            let settings = CameraSettings {
                horizontal_flip: true,
                ..CameraSettings::default()
            };
            assert_eq!(orientation(&settings), Orientation::Rotate0Mirror);

            let settings = CameraSettings {
                rotation: 180,
                horizontal_flip: true,
                ..CameraSettings::default()
            };
            assert_eq!(orientation(&settings), Orientation::Rotate180Mirror);
        }

        #[test]
        fn isp_controls_do_not_require_stream_reconfiguration() {
            let current = CameraSettings::default();
            let requested = CameraSettings {
                awb: "tungsten".to_owned(),
                gain: 4.0,
                shutter_us: 20_000,
                ..current.clone()
            };

            assert!(!stream_configuration_changed(
                CaptureProfile::Binning2k,
                &current,
                CaptureProfile::Binning2k,
                &requested,
            ));
        }

        #[test]
        fn profile_and_transform_changes_require_stream_reconfiguration() {
            let current = CameraSettings::default();
            let rotated = CameraSettings {
                rotation: 180,
                ..current.clone()
            };

            assert!(stream_configuration_changed(
                CaptureProfile::Binning2k,
                &current,
                CaptureProfile::Dci4k,
                &current,
            ));
            assert!(stream_configuration_changed(
                CaptureProfile::Binning2k,
                &current,
                CaptureProfile::Binning2k,
                &rotated,
            ));
        }

        #[test]
        fn raw_format_maps_to_dng_cfa_order() {
            assert_eq!(cfa_pattern("RGGB_PISP_COMP1"), Some([0, 1, 1, 2]));
            assert_eq!(cfa_pattern("BGGR_PISP_COMP1"), Some([2, 1, 1, 0]));
            assert_eq!(cfa_pattern("YUV420"), None);
        }

        #[test]
        fn capture_suffixes_are_monotonic() {
            assert!(unique_suffix() < unique_suffix());
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use imp::NativeCameraBackend;

#[cfg(not(target_os = "linux"))]
mod imp_stub {
    use std::path::Path;

    use tokio::sync::{broadcast, watch};

    use crate::camera::{CameraError, CaptureRequest, CaptureResult, PreviewFrame, StreamRequest};

    pub(crate) struct NativeCameraBackend {
        frames: broadcast::Sender<PreviewFrame>,
        streaming: watch::Sender<bool>,
    }

    impl NativeCameraBackend {
        pub(crate) fn spawn() -> Self {
            let (frames, _) = broadcast::channel(1);
            let (streaming, _) = watch::channel(false);
            Self { frames, streaming }
        }

        pub(crate) fn subscribe_streaming(&self) -> watch::Receiver<bool> {
            self.streaming.subscribe()
        }

        pub(crate) async fn is_streaming(&self) -> bool {
            false
        }

        pub(crate) async fn probe(&self) -> Option<String> {
            None
        }

        pub(crate) async fn start_stream(&self, request: StreamRequest) -> Result<(), CameraError> {
            request.validate()?;
            Err(CameraError::Unavailable)
        }

        pub(crate) async fn reconfigure_stream(
            &self,
            request: StreamRequest,
        ) -> Result<(), CameraError> {
            request.validate()?;
            Err(CameraError::Unavailable)
        }

        pub(crate) async fn stop_stream(&self) -> bool {
            false
        }

        pub(crate) async fn subscribe(
            &self,
        ) -> Result<broadcast::Receiver<PreviewFrame>, CameraError> {
            let _ = &self.frames;
            Err(CameraError::NotStreaming)
        }

        pub(crate) async fn capture_to_stage(
            &self,
            _capture_dir: &Path,
            request: CaptureRequest,
        ) -> Result<CaptureResult, CameraError> {
            request.validate()?;
            Err(CameraError::Unavailable)
        }

        pub(crate) async fn shutdown(&self) {}
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) use imp_stub::NativeCameraBackend;
