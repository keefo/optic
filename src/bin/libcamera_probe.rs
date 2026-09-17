#[cfg(target_os = "linux")]
#[path = "../native_codec.rs"]
mod native_codec;

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("libcamera-probe is only available on Linux");
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        collections::VecDeque, env, error::Error, fs, io, path::PathBuf, sync::mpsc, time::Duration,
    };

    use crate::native_codec::{
        DngMetadata, decode_pisp_comp1, encode_bayer16_dng, encode_yuv420_jpeg,
    };
    use libcamera::{
        camera::{ActiveCamera, CameraConfigurationStatus},
        camera_manager::CameraManager,
        controls,
        framebuffer::AsFrameBuffer,
        framebuffer_allocator::{FrameBuffer, FrameBufferAllocator},
        framebuffer_map::MemoryMappedFrameBuffer,
        geometry::Size,
        pixel_format::PixelFormat,
        properties,
        request::{RequestStatus, ReuseFlag},
        stream::{Stream, StreamRole},
    };

    type Result<T> = std::result::Result<T, Box<dyn Error>>;

    #[derive(Clone, Copy)]
    struct Profile {
        name: &'static str,
        preview: Size,
        still: Size,
        raw: Size,
        raw_format: &'static str,
    }

    const PROFILES: [Profile; 3] = [
        Profile {
            name: "master_archive",
            preview: Size::new(4056, 3040),
            still: Size::new(4056, 3040),
            raw: Size::new(4056, 3040),
            raw_format: "SBGGR12_CSI2P",
        },
        Profile {
            name: "dci_4k",
            preview: Size::new(1352, 720),
            still: Size::new(4056, 2160),
            raw: Size::new(4056, 2160),
            raw_format: "SBGGR12_CSI2P",
        },
        Profile {
            name: "binning_2k",
            preview: Size::new(1014, 760),
            still: Size::new(2028, 1520),
            raw: Size::new(2028, 1520),
            raw_format: "SBGGR10_CSI2P",
        },
    ];

    fn other(message: impl Into<String>) -> io::Error {
        io::Error::other(message.into())
    }

    fn format(name: &str) -> Result<PixelFormat> {
        PixelFormat::parse(name).ok_or_else(|| other(format!("unknown pixel format {name}")).into())
    }

    fn configure_stream(
        config: &mut libcamera::camera::CameraConfiguration,
        index: usize,
        pixel_format: PixelFormat,
        size: Size,
        buffers: u32,
    ) -> Result<()> {
        let mut stream = config
            .get_mut(index)
            .ok_or_else(|| other(format!("configuration has no stream {index}")))?;
        stream.set_pixel_format(pixel_format);
        stream.set_size(size);
        stream.set_buffer_count(buffers);
        Ok(())
    }

    fn describe_configuration(
        camera: &ActiveCamera<'_>,
        profile: Profile,
        fixed_preview: bool,
        include_raw: bool,
    ) -> Result<()> {
        let roles: &[StreamRole] = if include_raw {
            &[
                StreamRole::ViewFinder,
                StreamRole::StillCapture,
                StreamRole::Raw,
            ]
        } else {
            &[StreamRole::ViewFinder, StreamRole::StillCapture]
        };
        let mut config = camera
            .generate_configuration(roles)
            .ok_or_else(|| other("camera rejected role combination"))?;
        let preview = if fixed_preview {
            Size::new(1280, 960)
        } else {
            profile.preview
        };
        configure_stream(&mut config, 0, format("YUV420")?, preview, 4)?;
        configure_stream(&mut config, 1, format("YUV420")?, profile.still, 2)?;
        if include_raw {
            configure_stream(&mut config, 2, format(profile.raw_format)?, profile.raw, 2)?;
        }

        let status = config.validate();
        println!(
            "CONFIG profile={} preview={} raw={} status={status:?}",
            profile.name,
            if fixed_preview { "fixed" } else { "profile" },
            include_raw
        );
        for index in 0..config.len() {
            let stream = config
                .get(index)
                .ok_or_else(|| other("missing validated stream"))?;
            let size = stream.get_size();
            println!(
                "  stream={index} format={} size={}x{} stride={} frame_size={} buffers={}",
                stream.get_pixel_format(),
                size.width,
                size.height,
                stream.get_stride(),
                stream.get_frame_size(),
                stream.get_buffer_count()
            );
        }
        if let Some(sensor) = config.sensor_configuration() {
            let size = sensor.output_size();
            let (bin_x, bin_y) = sensor.binning();
            println!(
                "  sensor size={}x{} depth={} binning={}x{} crop={:?}",
                size.width,
                size.height,
                sensor.bit_depth(),
                bin_x,
                bin_y,
                sensor.analog_crop()
            );
        }
        if status.is_invalid() {
            return Err(other(format!("{} configuration is invalid", profile.name)).into());
        }
        Ok(())
    }

    fn capture_reusable_requests(camera: &mut ActiveCamera<'_>, profile: Profile) -> Result<()> {
        let mut config = camera
            .generate_configuration(&[StreamRole::ViewFinder, StreamRole::StillCapture])
            .ok_or_else(|| other("camera rejected dual processed streams"))?;
        configure_stream(&mut config, 0, format("YUV420")?, profile.preview, 4)?;
        configure_stream(&mut config, 1, format("YUV420")?, profile.still, 4)?;
        let validation = config.validate();
        if matches!(validation, CameraConfigurationStatus::Invalid) {
            return Err(other("capture configuration is invalid").into());
        }
        camera.configure(&mut config)?;

        let streams = (0..config.len())
            .map(|index| {
                config
                    .get(index)
                    .and_then(|stream| stream.stream())
                    .ok_or_else(|| other(format!("configured stream {index} is unavailable")))
            })
            .collect::<std::result::Result<Vec<Stream>, io::Error>>()?;
        let mut allocator = FrameBufferAllocator::new(camera);
        let mut buffers = Vec::with_capacity(streams.len());
        for stream in &streams {
            let mapped = allocator
                .alloc(stream)?
                .into_iter()
                .map(MemoryMappedFrameBuffer::new)
                .collect::<std::result::Result<VecDeque<_>, _>>()?;
            buffers.push(mapped);
        }
        let request_count = buffers.iter().map(VecDeque::len).min().unwrap_or(0);
        if request_count == 0 {
            return Err(other("no buffers were allocated").into());
        }

        let mut requests = Vec::with_capacity(request_count);
        for cookie in 0..request_count {
            let mut request = camera
                .create_request(Some(cookie as u64))
                .ok_or_else(|| other("failed to create request"))?;
            for (stream, stream_buffers) in streams.iter().zip(&mut buffers) {
                request.add_buffer(
                    stream,
                    stream_buffers
                        .pop_front()
                        .ok_or_else(|| other("buffer set became unbalanced"))?,
                )?;
            }
            request
                .controls_mut()
                .set(controls::FrameDurationLimits([125_000, 125_000]))?;
            request.controls_mut().set(controls::ExposureValue(0.0))?;
            requests.push(request);
        }

        let (completed_tx, completed_rx) = mpsc::channel();
        camera.on_request_completed(move |request| {
            let _ = completed_tx.send(request);
        });
        camera.start(None)?;
        for request in requests {
            camera.queue_request(request).map_err(|(_, error)| error)?;
        }

        let target_frames = request_count + 8;
        let mut completed = 0usize;
        while completed < target_frames {
            let mut request = completed_rx.recv_timeout(Duration::from_secs(10))?;
            if request.status() != RequestStatus::Complete {
                return Err(other(format!("request completed as {:?}", request.status())).into());
            }
            let mut total_bytes = 0usize;
            for stream in &streams {
                let buffer: &MemoryMappedFrameBuffer<FrameBuffer> = request
                    .buffer(stream)
                    .ok_or_else(|| other("completed request lost a mapped buffer"))?;
                let metadata = buffer
                    .metadata()
                    .ok_or_else(|| other("completed buffer has no metadata"))?;
                let metadata_planes = metadata.planes();
                for (plane, bytes) in buffer.data().iter().zip(&metadata_planes) {
                    let used = bytes.bytes_used as usize;
                    if used > plane.len() {
                        return Err(other("buffer metadata exceeds mapped plane").into());
                    }
                    total_bytes += used;
                }
            }
            let exposure = request.metadata().get::<controls::ExposureTime>().ok();
            let gain = request.metadata().get::<controls::AnalogueGain>().ok();
            println!(
                "FRAME sequence={} cookie={} bytes={} exposure_us={:?} gain={:?}",
                request.sequence(),
                request.cookie(),
                total_bytes,
                exposure.map(|value| value.0),
                gain.map(|value| value.0)
            );
            completed += 1;
            if completed < target_frames {
                request.reuse(ReuseFlag::REUSE_BUFFERS);
                let ev = if completed >= request_count {
                    -0.5
                } else {
                    0.0
                };
                request.controls_mut().set(controls::ExposureValue(ev))?;
                request
                    .controls_mut()
                    .set(controls::FrameDurationLimits([125_000, 125_000]))?;
                camera.queue_request(request).map_err(|(_, error)| error)?;
            }
        }
        camera.stop()?;
        println!(
            "REUSE PASS profile={} completed_frames={} requests={}",
            profile.name, completed, request_count
        );
        Ok(())
    }

    fn copy_buffer(buffer: &MemoryMappedFrameBuffer<FrameBuffer>) -> Result<Vec<u8>> {
        let metadata = buffer
            .metadata()
            .ok_or_else(|| other("completed buffer has no metadata"))?;
        let planes = buffer.data();
        let metadata_planes = metadata.planes();
        if planes.len() != metadata_planes.len() {
            return Err(other("buffer plane metadata count does not match mapping").into());
        }

        let capacity = metadata_planes
            .into_iter()
            .map(|plane| plane.bytes_used as usize)
            .sum();
        let mut output = Vec::with_capacity(capacity);
        for (plane, bytes) in planes.into_iter().zip(&metadata_planes) {
            let used = bytes.bytes_used as usize;
            if used > plane.len() {
                return Err(other("buffer metadata exceeds mapped plane").into());
            }
            output.extend_from_slice(&plane[..used]);
        }
        Ok(output)
    }

    fn capture_full_resolution_outputs(
        camera: &mut ActiveCamera<'_>,
        profile: Profile,
        model: &str,
    ) -> Result<()> {
        let mut config = camera
            .generate_configuration(&[StreamRole::StillCapture, StreamRole::Raw])
            .ok_or_else(|| other("camera rejected still plus raw streams"))?;
        configure_stream(&mut config, 0, format("YUV420")?, profile.still, 3)?;
        configure_stream(&mut config, 1, format(profile.raw_format)?, profile.raw, 3)?;
        let validation = config.validate();
        if matches!(validation, CameraConfigurationStatus::Invalid) {
            return Err(other("full-resolution capture configuration is invalid").into());
        }

        let still_config = config
            .get(0)
            .ok_or_else(|| other("validated still stream is unavailable"))?;
        let still_size = still_config.get_size();
        let still_stride = still_config.get_stride();
        let still_format = still_config.get_pixel_format().to_string();
        let raw_config = config
            .get(1)
            .ok_or_else(|| other("validated raw stream is unavailable"))?;
        let raw_size = raw_config.get_size();
        let raw_stride = raw_config.get_stride();
        let raw_format = raw_config.get_pixel_format().to_string();
        if still_format != "YUV420" || raw_format != "BGGR_PISP_COMP1" {
            return Err(other(format!(
                "unexpected validated formats: still={still_format}, raw={raw_format}"
            ))
            .into());
        }
        println!(
            "OUTPUT CONFIG status={validation:?} still={still_format} {}x{} stride={} raw={raw_format} {}x{} stride={}",
            still_size.width,
            still_size.height,
            still_stride,
            raw_size.width,
            raw_size.height,
            raw_stride
        );

        camera.configure(&mut config)?;
        let streams = (0..config.len())
            .map(|index| {
                config
                    .get(index)
                    .and_then(|stream| stream.stream())
                    .ok_or_else(|| other(format!("configured stream {index} is unavailable")))
            })
            .collect::<std::result::Result<Vec<Stream>, io::Error>>()?;
        let mut allocator = FrameBufferAllocator::new(camera);
        let mut buffers = Vec::with_capacity(streams.len());
        for stream in &streams {
            buffers.push(
                allocator
                    .alloc(stream)?
                    .into_iter()
                    .map(MemoryMappedFrameBuffer::new)
                    .collect::<std::result::Result<VecDeque<_>, _>>()?,
            );
        }
        let request_count = buffers.iter().map(VecDeque::len).min().unwrap_or(0);
        if request_count == 0 {
            return Err(other("no full-resolution buffers were allocated").into());
        }

        let mut requests = Vec::with_capacity(request_count);
        for cookie in 0..request_count {
            let mut request = camera
                .create_request(Some(cookie as u64))
                .ok_or_else(|| other("failed to create full-resolution request"))?;
            for (stream, stream_buffers) in streams.iter().zip(&mut buffers) {
                request.add_buffer(
                    stream,
                    stream_buffers
                        .pop_front()
                        .ok_or_else(|| other("full-resolution buffer set became unbalanced"))?,
                )?;
            }
            request
                .controls_mut()
                .set(controls::FrameDurationLimits([125_000, 125_000]))?;
            requests.push(request);
        }

        let (completed_tx, completed_rx) = mpsc::channel();
        camera.on_request_completed(move |request| {
            let _ = completed_tx.send(request);
        });
        camera.start(None)?;
        for request in requests {
            camera.queue_request(request).map_err(|(_, error)| error)?;
        }

        let target_frames = request_count + 8;
        let mut completed = 0usize;
        let mut captured = None;
        while completed < target_frames {
            let mut request = completed_rx.recv_timeout(Duration::from_secs(10))?;
            if request.status() != RequestStatus::Complete {
                return Err(other(format!("request completed as {:?}", request.status())).into());
            }

            if completed + 1 == target_frames {
                let yuv = copy_buffer(
                    request
                        .buffer(&streams[0])
                        .ok_or_else(|| other("completed request lost the still buffer"))?,
                )?;
                let raw = copy_buffer(
                    request
                        .buffer(&streams[1])
                        .ok_or_else(|| other("completed request lost the raw buffer"))?,
                )?;
                let request_metadata = request.metadata();
                let mut metadata = DngMetadata {
                    model: model.to_owned(),
                    ..DngMetadata::default()
                };
                if let Ok(value) = request_metadata.get::<controls::SensorBlackLevels>() {
                    metadata.black_levels = value.0;
                }
                if let Ok(value) = request_metadata.get::<controls::ExposureTime>() {
                    metadata.exposure_us = value.0;
                }
                if let Ok(value) = request_metadata.get::<controls::AnalogueGain>() {
                    metadata.analogue_gain = value.0;
                }
                if let Ok(value) = request_metadata.get::<controls::ColourGains>() {
                    metadata.colour_gains = value.0;
                }
                if let Ok(value) = request_metadata.get::<controls::ColourCorrectionMatrix>() {
                    metadata.colour_correction_matrix = value.0;
                }
                captured = Some((yuv, raw, metadata, request.sequence()));
            }

            completed += 1;
            if completed < target_frames {
                request.reuse(ReuseFlag::REUSE_BUFFERS);
                request
                    .controls_mut()
                    .set(controls::FrameDurationLimits([125_000, 125_000]))?;
                camera.queue_request(request).map_err(|(_, error)| error)?;
            }
        }
        camera.stop()?;

        let (yuv, raw, metadata, sequence) =
            captured.ok_or_else(|| other("no full-resolution frame was captured"))?;
        let jpeg =
            encode_yuv420_jpeg(&yuv, still_size.width, still_size.height, still_stride, 100)?;
        let pixels = decode_pisp_comp1(&raw, raw_size.width, raw_size.height, raw_stride)?;
        let minimum = pixels.iter().copied().min().unwrap_or(0);
        let maximum = pixels.iter().copied().max().unwrap_or(0);
        let dng = encode_bayer16_dng(&pixels, raw_size.width, raw_size.height, &metadata)?;

        let output_dir = env::var_os("OPTIC_PROBE_OUTPUT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        fs::create_dir_all(&output_dir)?;
        let jpeg_path = output_dir.join("optic-native.jpg");
        let dng_path = output_dir.join("optic-native.dng");
        fs::write(&jpeg_path, &jpeg)?;
        fs::write(&dng_path, &dng)?;
        println!(
            "OUTPUT PASS sequence={sequence} yuv_bytes={} raw_bytes={} raw_range={minimum}..={maximum} jpeg={}({} bytes) dng={}({} bytes) metadata={metadata:?}",
            yuv.len(),
            raw.len(),
            jpeg_path.display(),
            jpeg.len(),
            dng_path.display(),
            dng.len()
        );
        Ok(())
    }

    pub fn run() -> Result<()> {
        let manager = CameraManager::new()?;
        println!("libcamera={}", manager.version());
        let cameras = manager.cameras();
        println!("cameras={}", cameras.len());
        let camera = cameras
            .iter()
            .find(|camera| {
                camera
                    .properties()
                    .get::<properties::Model>()
                    .map(|model| model.0.to_ascii_lowercase().contains("imx477"))
                    .unwrap_or(false)
            })
            .ok_or_else(|| other("IMX477 was not found"))?;
        let model = camera.properties().get::<properties::Model>()?.0;
        println!("camera_id={}", camera.id());
        println!("model={model}");
        println!("properties={:#?}", camera.properties());
        println!("controls={:#?}", camera.controls());

        let mut camera = camera.acquire()?;
        for profile in PROFILES {
            describe_configuration(&camera, profile, false, false)?;
            describe_configuration(&camera, profile, true, false)?;
            if profile.name != "binning_2k" {
                describe_configuration(&camera, profile, false, true)?;
                describe_configuration(&camera, profile, true, true)?;
            }
        }
        capture_reusable_requests(&mut camera, PROFILES[2])?;
        capture_full_resolution_outputs(&mut camera, PROFILES[0], &model)?;
        println!("NATIVE LIBCAMERA PROBE PASS");
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::run()
}
