#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::fmt;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraSettings {
    pub rotation: u16,
    pub horizontal_flip: bool,
    pub vertical_flip: bool,
    pub awb: String,
    pub metering: String,
    pub exposure: String,
    pub ev: f32,
    pub gain: f32,
    pub shutter_us: u64,
    pub denoise: String,
}

impl Default for CameraSettings {
    fn default() -> Self {
        Self {
            rotation: 0,
            horizontal_flip: false,
            vertical_flip: false,
            awb: "auto".to_owned(),
            metering: "centre".to_owned(),
            exposure: "normal".to_owned(),
            ev: 0.0,
            gain: 0.0,
            shutter_us: 0,
            denoise: "auto".to_owned(),
        }
    }
}

impl CameraSettings {
    pub(crate) fn validate(&self) -> Result<(), CameraError> {
        if !matches!(self.rotation, 0 | 180) {
            return Err(CameraError::Invalid("rotation must be 0 or 180"));
        }
        if !matches!(
            self.awb.as_str(),
            "auto" | "incandescent" | "tungsten" | "fluorescent" | "indoor" | "daylight" | "cloudy"
        ) {
            return Err(CameraError::Invalid("unsupported white-balance mode"));
        }
        if !matches!(self.metering.as_str(), "centre" | "spot" | "average") {
            return Err(CameraError::Invalid("unsupported metering mode"));
        }
        if !matches!(self.exposure.as_str(), "normal" | "sport") {
            return Err(CameraError::Invalid("unsupported exposure mode"));
        }
        if !matches!(
            self.denoise.as_str(),
            "auto" | "off" | "cdn_off" | "cdn_fast" | "cdn_hq"
        ) {
            return Err(CameraError::Invalid("unsupported denoise mode"));
        }
        if !(-4.0..=4.0).contains(&self.ev) || !self.ev.is_finite() {
            return Err(CameraError::Invalid("EV must be between -4 and 4"));
        }
        if !(self.gain == 0.0 || (1.0..=16.0).contains(&self.gain)) || !self.gain.is_finite() {
            return Err(CameraError::Invalid(
                "gain must be 0 (auto) or between 1 and 16",
            ));
        }
        if !(self.shutter_us == 0 || (100..=5_000_000).contains(&self.shutter_us)) {
            return Err(CameraError::Invalid(
                "shutter must be 0 (auto) or between 100 and 5000000 microseconds",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureProfile {
    #[default]
    MasterArchive,
    #[serde(rename = "dci_4k")]
    Dci4k,
    #[serde(rename = "binning_2k")]
    Binning2k,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    pub profile: CaptureProfile,
    pub settings: CameraSettings,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            profile: CaptureProfile::Binning2k,
            settings: CameraSettings::default(),
        }
    }
}

impl CaptureProfile {
    pub(crate) fn spec(self) -> CaptureSpec {
        match self {
            Self::MasterArchive => CaptureSpec {
                slug: "master-archive",
                width: 4056,
                height: 3040,
                jpeg_quality: 100,
            },
            Self::Dci4k => CaptureSpec {
                slug: "4k-dci",
                width: 4056,
                height: 2160,
                jpeg_quality: 95,
            },
            Self::Binning2k => CaptureSpec {
                slug: "2k-binning",
                width: 2028,
                height: 1520,
                jpeg_quality: 85,
            },
        }
    }

    pub(crate) fn validate_raw_policy(self, save_dng: bool) -> Result<(), CameraError> {
        match (self, save_dng) {
            (Self::MasterArchive, false) => Err(CameraError::Invalid(
                "Master Archive requires a companion DNG",
            )),
            (Self::Binning2k, true) => Err(CameraError::Invalid(
                "2K Binning does not support companion DNG capture",
            )),
            _ => Ok(()),
        }
    }

    pub(crate) fn preview_spec(self) -> PreviewSpec {
        match self {
            Self::MasterArchive => PreviewSpec {
                width: 4056,
                height: 3040,
                fps: 2,
            },
            Self::Dci4k => PreviewSpec {
                width: 1352,
                height: 720,
                fps: 8,
            },
            Self::Binning2k => PreviewSpec {
                width: 1014,
                height: 760,
                fps: 8,
            },
        }
    }

    pub(crate) fn raw_format(self) -> &'static str {
        match self {
            Self::MasterArchive | Self::Dci4k => "SBGGR12_CSI2P",
            Self::Binning2k => "SBGGR10_CSI2P",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CaptureSpec {
    pub slug: &'static str,
    pub width: u32,
    pub height: u32,
    pub jpeg_quality: u8,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PreviewSpec {
    pub width: u32,
    pub height: u32,
    pub fps: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequest {
    pub settings: CameraSettings,
    pub profile: CaptureProfile,
    pub save_dng: bool,
}

impl CaptureRequest {
    pub(crate) fn validate(&self) -> Result<(), CameraError> {
        self.settings.validate()?;
        self.profile.validate_raw_policy(self.save_dng)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StreamRequest {
    pub settings: CameraSettings,
    pub profile: CaptureProfile,
    #[serde(default)]
    pub control_revision: u64,
}

impl StreamRequest {
    pub(crate) fn validate(&self) -> Result<(), CameraError> {
        self.settings.validate()
    }
}

#[derive(Debug, Clone)]
pub struct PreviewFrame {
    pub jpeg: Bytes,
    pub sequence: u32,
    pub control_revision: u64,
    pub ae_state: Option<&'static str>,
    pub awb_state: Option<&'static str>,
    pub exposure_us: Option<i32>,
    pub analogue_gain: Option<f32>,
    pub colour_gains: Option<[f32; 2]>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CaptureFile {
    pub filename: String,
    pub bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct CaptureResult {
    pub profile: CaptureProfile,
    pub files: Vec<CaptureFile>,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug)]
pub enum CameraError {
    Busy,
    Unavailable,
    Invalid(&'static str),
    Io(std::io::Error),
    Backend(String),
    Timeout,
    NotStreaming,
}

impl fmt::Display for CameraError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => write!(formatter, "camera is busy"),
            Self::Unavailable => write!(formatter, "camera actor is unavailable"),
            Self::Invalid(message) => write!(formatter, "{message}"),
            Self::Io(error) => write!(formatter, "camera I/O error: {error}"),
            Self::Backend(message) => write!(formatter, "camera backend failed: {message}"),
            Self::Timeout => write!(formatter, "camera operation timed out"),
            Self::NotStreaming => write!(formatter, "preview stream is not running"),
        }
    }
}

impl std::error::Error for CameraError {}

impl From<std::io::Error> for CameraError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_are_valid() {
        CameraSettings::default().validate().unwrap();
    }

    #[test]
    fn capture_profiles_match_documented_presets() {
        let master = CaptureProfile::MasterArchive.spec();
        assert_eq!(
            (master.width, master.height, master.jpeg_quality),
            (4056, 3040, 100)
        );

        let dci = CaptureProfile::Dci4k.spec();
        assert_eq!((dci.width, dci.height, dci.jpeg_quality), (4056, 2160, 95));

        let binning = CaptureProfile::Binning2k.spec();
        assert_eq!(
            (binning.width, binning.height, binning.jpeg_quality),
            (2028, 1520, 85)
        );
    }

    #[test]
    fn stream_request_uses_selected_profile() {
        let request: StreamRequest =
            serde_json::from_str(r#"{"settings": {}, "profile": "dci_4k"}"#).unwrap();
        assert_eq!(request.profile, CaptureProfile::Dci4k);
        assert_eq!(request.control_revision, 0);
        let preview = request.profile.preview_spec();
        assert_eq!((preview.width, preview.height), (1352, 720));
    }

    #[test]
    fn capture_profiles_enforce_raw_policy() {
        assert!(
            CaptureProfile::MasterArchive
                .validate_raw_policy(true)
                .is_ok()
        );
        assert!(
            CaptureProfile::MasterArchive
                .validate_raw_policy(false)
                .is_err()
        );
        assert!(CaptureProfile::Dci4k.validate_raw_policy(false).is_ok());
        assert!(CaptureProfile::Dci4k.validate_raw_policy(true).is_ok());
        assert!(CaptureProfile::Binning2k.validate_raw_policy(false).is_ok());
        assert!(CaptureProfile::Binning2k.validate_raw_policy(true).is_err());
    }

    #[test]
    fn settings_reject_unsafe_ranges_and_unknown_modes() {
        let invalid_rotation = CameraSettings {
            rotation: 90,
            ..CameraSettings::default()
        };
        assert!(invalid_rotation.validate().is_err());

        let invalid_gain = CameraSettings {
            gain: 0.5,
            ..CameraSettings::default()
        };
        assert!(invalid_gain.validate().is_err());

        let invalid_awb = CameraSettings {
            awb: "invalid".to_owned(),
            ..CameraSettings::default()
        };
        assert!(invalid_awb.validate().is_err());
    }
}
