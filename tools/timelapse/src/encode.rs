//! Output sizing, naming, ffmpeg argument construction, and the output
//! path safety check (`docs/timelapse-builder.md` §3 and §6). Pure.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone};

use crate::names::Profile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// `libx265` (default).
    Hevc,
    /// `hevc_videotoolbox`; hardware-limited to 3840×2160 on this iMac Pro.
    HevcVt,
    /// `libx264`.
    H264,
}

/// Largest frame the VideoToolbox hardware encoder accepts on the iMac Pro
/// (see the worklog's "Findings Before Implementation").
pub const VT_MAX_SIZE: (u32, u32) = (3840, 2160);

impl Codec {
    pub fn parse(value: &str) -> Option<Codec> {
        match value {
            "hevc" => Some(Codec::Hevc),
            "hevc-vt" => Some(Codec::HevcVt),
            "h264" => Some(Codec::H264),
            _ => None,
        }
    }

    pub fn encoder_name(self) -> &'static str {
        match self {
            Codec::Hevc => "libx265",
            Codec::HevcVt => "hevc_videotoolbox",
            Codec::H264 => "libx264",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeOptions {
    pub codec: Codec,
    pub fps: u32,
    pub max_width: u32,
    pub bitrate: String,
    pub deflicker: bool,
}

/// Fits `source` inside `max_width` (and the codec's size limit), never
/// upscaling, keeping aspect ratio, with both sides rounded to even.
pub fn output_size(source: (u32, u32), max_width: u32, codec: Codec) -> (u32, u32) {
    let (sw, sh) = (source.0 as f64, source.1 as f64);
    let mut scale = (max_width as f64 / sw).min(1.0);
    if codec == Codec::HevcVt {
        scale = scale
            .min(VT_MAX_SIZE.0 as f64 / sw)
            .min(VT_MAX_SIZE.1 as f64 / sh);
    }
    let even = |x: f64| (((x / 2.0).round() as u32) * 2).max(2);
    (even(sw * scale), even(sh * scale))
}

/// `<rule>_<profile>_<YYYYMMDD-HHMM>_<YYYYMMDD-HHMM>.mp4`. Any character
/// outside `[a-z0-9-]` in the rule (it may come from an untrusted sidecar)
/// becomes `_`.
pub fn output_file_name<Tz: TimeZone>(
    rule: &str,
    profile: Profile,
    first: &DateTime<Tz>,
    last: &DateTime<Tz>,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let rule: String = rule
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "{rule}_{}_{}_{}.mp4",
        profile.slug(),
        first.format("%Y%m%d-%H%M"),
        last.format("%Y%m%d-%H%M")
    )
}

/// The in-progress name ffmpeg writes to before the final rename.
pub fn partial_path(output: &Path) -> PathBuf {
    let stem = output
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    output.with_file_name(format!("{stem}.partial.mp4"))
}

/// Scales and converts the full-range BT.601 JPEG frames to limited-range
/// BT.709 with square pixels, which is what QuickTime and most players
/// assume for HEVC/H.264. Found in the first real run (see the worklog):
/// without `out_range`/`out_color_matrix`, ffmpeg 9 keeps the JPEG's full
/// range and 601 matrix; output-level `-color_primaries`/`-color_trc` are
/// ignored (`setparams` sets them on the frames instead); and `scale` sets a
/// 0.99996 sample aspect ratio to keep 4056:3040 exact after rounding the
/// height to even, which AVFoundation shows as a 3839.86 px wide frame
/// (`setsar=1`).
pub fn filter_chain(size: (u32, u32), deflicker: bool) -> String {
    let mut chain = format!(
        "scale={}:{}:flags=lanczos:out_range=tv:out_color_matrix=bt709,setsar=1,\
         setparams=range=tv:color_primaries=bt709:color_trc=bt709:colorspace=bt709",
        size.0, size.1
    );
    if deflicker {
        chain.push_str(",deflicker=mode=pm:size=5");
    }
    chain.push_str(",format=yuv420p");
    chain
}

/// ffmpeg arguments (excluding the program name) for one segment.
pub fn ffmpeg_args(
    input_pattern: &Path,
    output_partial: &Path,
    size: (u32, u32),
    options: &EncodeOptions,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-stats",
        "-y",
        "-framerate",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    args.push(options.fps.to_string().into());
    args.extend(["-start_number", "0", "-i"].map(OsString::from));
    args.push(input_pattern.into());
    args.push("-vf".into());
    args.push(filter_chain(size, options.deflicker).into());
    args.push("-an".into());
    let codec_args: Vec<String> = match options.codec {
        Codec::Hevc => vec![
            "-c:v",
            "libx265",
            "-crf",
            "20",
            "-preset",
            "medium",
            "-x265-params",
            "log-level=error",
            "-tag:v",
            "hvc1",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        Codec::HevcVt => vec![
            "-c:v".into(),
            "hevc_videotoolbox".into(),
            "-b:v".into(),
            options.bitrate.clone(),
            "-tag:v".into(),
            "hvc1".into(),
        ],
        Codec::H264 => ["-c:v", "libx264", "-crf", "18", "-preset", "medium"]
            .into_iter()
            .map(String::from)
            .collect(),
    };
    args.extend(codec_args.into_iter().map(OsString::from));
    args.extend(["-movflags", "+faststart"].map(OsString::from));
    args.push(output_partial.into());
    args
}

/// Renders a command for display, quoting arguments that need it.
pub fn display_command(program: &Path, args: &[OsString]) -> String {
    std::iter::once(program.as_os_str())
        .chain(args.iter().map(OsString::as_os_str))
        .map(|arg| {
            let s = arg.to_string_lossy();
            let plain = !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./:=+,%@".contains(c));
            if plain {
                s.into_owned()
            } else {
                format!("'{}'", s.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Rejects an output directory that is the source directory or inside it.
/// Both paths must already be absolute and symlink-resolved.
pub fn check_output_dir(source: &Path, output: &Path) -> Result<(), String> {
    if output.starts_with(source) {
        Err(format!(
            "output directory {} is inside the capture directory {}; the capture \
             directory is read-only for this tool",
            output.display(),
            source.display()
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn opts(codec: Codec, deflicker: bool) -> EncodeOptions {
        EncodeOptions {
            codec,
            fps: 24,
            max_width: 3840,
            bitrate: "40M".into(),
            deflicker,
        }
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn output_size_scales_to_width_keeping_aspect_with_even_sides() {
        assert_eq!(output_size((4056, 3040), 3840, Codec::Hevc), (3840, 2878));
        assert_eq!(output_size((4056, 2160), 3840, Codec::Hevc), (3840, 2044));
        assert_eq!(output_size((4056, 3040), 1920, Codec::H264), (1920, 1440));
    }

    #[test]
    fn output_size_never_upscales() {
        assert_eq!(output_size((2028, 1520), 3840, Codec::Hevc), (2028, 1520));
    }

    #[test]
    fn output_size_fits_the_videotoolbox_box() {
        assert_eq!(output_size((4056, 3040), 3840, Codec::HevcVt), (2882, 2160));
        assert_eq!(output_size((4056, 2160), 3840, Codec::HevcVt), (3840, 2044));
        assert_eq!(output_size((4056, 3040), 1920, Codec::HevcVt), (1920, 1440));
    }

    #[test]
    fn output_file_name_follows_the_documented_format() {
        let tz = FixedOffset::west_opt(7 * 3600).unwrap();
        let first = tz.timestamp_millis_opt(1_789_957_445_965).unwrap(); // 19:24 PDT
        let last = tz.timestamp_millis_opt(1_789_961_645_992).unwrap(); // 20:34 PDT
        assert_eq!(
            output_file_name("every1min", Profile::MasterArchive, &first, &last),
            "every1min_master-archive_20260920-1924_20260920-2034.mp4"
        );
        assert_eq!(
            output_file_name("../Evil name", Profile::Dci4k, &first, &first),
            "____vil_name_4k-dci_20260920-1924_20260920-1924.mp4"
        );
    }

    #[test]
    fn partial_path_sits_next_to_the_output() {
        assert_eq!(
            partial_path(Path::new("/out/a_b.mp4")),
            PathBuf::from("/out/a_b.partial.mp4")
        );
    }

    #[test]
    fn ffmpeg_args_for_default_hevc_with_deflicker() {
        let args = ffmpeg_args(
            Path::new("/tmp/x/%06d.jpg"),
            Path::new("/out/a.partial.mp4"),
            (3840, 2878),
            &opts(Codec::Hevc, true),
        );
        assert_eq!(
            strings(&args).join(" "),
            "-hide_banner -nostdin -loglevel error -stats -y -framerate 24 \
             -start_number 0 -i /tmp/x/%06d.jpg \
             -vf scale=3840:2878:flags=lanczos:out_range=tv:out_color_matrix=bt709,setsar=1,\
             setparams=range=tv:color_primaries=bt709:color_trc=bt709:colorspace=bt709,\
             deflicker=mode=pm:size=5,format=yuv420p \
             -an -c:v libx265 -crf 20 -preset medium -x265-params log-level=error \
             -tag:v hvc1 -movflags +faststart /out/a.partial.mp4"
        );
    }

    #[test]
    fn ffmpeg_args_per_codec_and_without_deflicker() {
        let vt = strings(&ffmpeg_args(
            Path::new("p"),
            Path::new("o"),
            (2882, 2160),
            &opts(Codec::HevcVt, false),
        ))
        .join(" ");
        assert!(vt.contains(
            "-vf scale=2882:2160:flags=lanczos:out_range=tv:out_color_matrix=bt709,setsar=1,\
             setparams=range=tv:color_primaries=bt709:color_trc=bt709:colorspace=bt709,\
             format=yuv420p "
        ));
        assert!(vt.contains("-c:v hevc_videotoolbox -b:v 40M -tag:v hvc1"));
        assert!(!vt.contains("deflicker"));

        let h264 = strings(&ffmpeg_args(
            Path::new("p"),
            Path::new("o"),
            (1920, 1440),
            &opts(Codec::H264, true),
        ))
        .join(" ");
        assert!(h264.contains("-c:v libx264 -crf 18 -preset medium -movflags"));
        assert!(!h264.contains("hvc1"));
    }

    #[test]
    fn codec_parse_round_trips() {
        assert_eq!(Codec::parse("hevc"), Some(Codec::Hevc));
        assert_eq!(Codec::parse("hevc-vt"), Some(Codec::HevcVt));
        assert_eq!(Codec::parse("h264"), Some(Codec::H264));
        assert_eq!(Codec::parse("prores"), None);
    }

    #[test]
    fn output_dir_inside_or_equal_to_source_is_rejected() {
        let src = Path::new("/Users/admin/Pictures/Optic");
        assert!(check_output_dir(src, src).is_err());
        assert!(check_output_dir(src, &src.join("videos")).is_err());
        assert!(check_output_dir(src, Path::new("/Users/admin/Pictures/Optic-Timelapses")).is_ok());
        assert!(check_output_dir(src, Path::new("/Users/admin/Movies/Optic Timelapses")).is_ok());
    }

    #[test]
    fn display_command_quotes_spaces() {
        let shown = display_command(
            Path::new("ffmpeg"),
            &[
                "-i".into(),
                "/Users/admin/Movies/Optic Timelapses/a.mp4".into(),
            ],
        );
        assert_eq!(
            shown,
            "ffmpeg -i '/Users/admin/Movies/Optic Timelapses/a.mp4'"
        );
    }
}
