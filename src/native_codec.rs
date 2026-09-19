use std::{
    ffi::CStr,
    io::{self, Cursor},
};

use tiff::{
    encoder::{Rational, SRational, TiffEncoder, colortype::Gray8},
    tags::Tag,
};
use turbojpeg_sys as tj;

const DNG_VERSION: [u8; 4] = [1, 4, 0, 0];
const DNG_BACKWARD_VERSION: [u8; 4] = [1, 1, 0, 0];
const CFA_BGGR: [u8; 4] = [2, 1, 1, 0];
const PISP_COMPRESS_OFFSET: u16 = 2048;

const TAG_CFA_REPEAT_PATTERN_DIM: u16 = 33421;
const TAG_CFA_PATTERN: u16 = 33422;
const TAG_EXPOSURE_TIME: u16 = 33434;
const TAG_ISO_SPEED_RATINGS: u16 = 34855;
const TAG_DNG_VERSION: u16 = 50706;
const TAG_DNG_BACKWARD_VERSION: u16 = 50707;
const TAG_UNIQUE_CAMERA_MODEL: u16 = 50708;
const TAG_CFA_PLANE_COLOR: u16 = 50710;
const TAG_CFA_LAYOUT: u16 = 50711;
const TAG_BLACK_LEVEL_REPEAT_DIM: u16 = 50713;
const TAG_BLACK_LEVEL: u16 = 50714;
const TAG_WHITE_LEVEL: u16 = 50717;
const TAG_DEFAULT_CROP_ORIGIN: u16 = 50719;
const TAG_DEFAULT_CROP_SIZE: u16 = 50720;
const TAG_COLOR_MATRIX_1: u16 = 50721;
const TAG_AS_SHOT_NEUTRAL: u16 = 50728;
const TAG_CALIBRATION_ILLUMINANT_1: u16 = 50778;
const TAG_ACTIVE_AREA: u16 = 50829;
const PHOTOMETRIC_CFA: u16 = 32803;

fn integer_error(error: std::num::TryFromIntError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}

#[derive(Debug, Clone)]
pub(crate) struct DngMetadata {
    pub model: String,
    pub cfa_pattern: [u8; 4],
    pub black_levels: [i32; 4],
    pub exposure_us: i32,
    pub analogue_gain: f32,
    pub colour_gains: [f32; 2],
    pub colour_correction_matrix: [[f32; 3]; 3],
}

impl Default for DngMetadata {
    fn default() -> Self {
        Self {
            model: "imx477".to_owned(),
            cfa_pattern: CFA_BGGR,
            black_levels: [4096; 4],
            exposure_us: 10_000,
            analogue_gain: 1.0,
            colour_gains: [1.0, 1.0],
            colour_correction_matrix: [
                [1.90255, -0.77478, -0.12777],
                [-0.31338, 1.88197, -0.56858],
                [-0.06001, -0.61785, 1.67786],
            ],
        }
    }
}

struct TurboJpegHandle(tj::tjhandle);

impl Drop for TurboJpegHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: The handle was returned by tjInitCompress and is owned here.
            unsafe { tj::tjDestroy(self.0) };
        }
    }
}

pub(crate) fn encode_yuv420_jpeg(
    data: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    quality: u8,
) -> io::Result<Vec<u8>> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "YUV420 dimensions must be non-zero and even",
        ));
    }
    if stride < width || !stride.is_multiple_of(2) || !(1..=100).contains(&quality) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid YUV420 stride or JPEG quality",
        ));
    }

    let y_len = usize::try_from(stride).map_err(integer_error)?
        * usize::try_from(height).map_err(integer_error)?;
    let uv_stride = stride / 2;
    let uv_len = usize::try_from(uv_stride).map_err(integer_error)?
        * usize::try_from(height / 2).map_err(integer_error)?;
    let required = y_len
        .checked_add(uv_len.checked_mul(2).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "YUV420 buffer size overflow")
        })?)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "YUV420 buffer size overflow")
        })?;
    if data.len() < required {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "YUV420 buffer has {} bytes, expected {required}",
                data.len()
            ),
        ));
    }

    // SAFETY: TurboJPEG returns an opaque owned handle or null.
    let handle = TurboJpegHandle(unsafe { tj::tjInitCompress() });
    if handle.0.is_null() {
        return Err(io::Error::other(
            "TurboJPEG compressor initialization failed",
        ));
    }
    let width = i32::try_from(width).map_err(integer_error)?;
    let height = i32::try_from(height).map_err(integer_error)?;
    let subsampling = i32::try_from(tj::TJSAMP_TJSAMP_420).map_err(integer_error)?;
    let max_len = unsafe { tj::tjBufSize(width, height, subsampling) };
    if max_len == 0 || max_len == u64::MAX {
        return Err(turbo_error(handle.0));
    }
    let mut output = vec![0_u8; usize::try_from(max_len).map_err(integer_error)?];
    let mut output_ptr = output.as_mut_ptr();
    let mut output_len = max_len;
    let mut planes = [
        data.as_ptr(),
        // SAFETY: The bounds check above proves both offsets are in `data`.
        unsafe { data.as_ptr().add(y_len) },
        unsafe { data.as_ptr().add(y_len + uv_len) },
    ];
    let strides = [
        i32::try_from(stride).map_err(integer_error)?,
        i32::try_from(uv_stride).map_err(integer_error)?,
        i32::try_from(uv_stride).map_err(integer_error)?,
    ];

    // SAFETY: All input planes and strides describe initialized data for the
    // supplied dimensions. The output is preallocated to tjBufSize and
    // NOREALLOC prevents TurboJPEG from changing its pointer.
    let result = unsafe {
        tj::tjCompressFromYUVPlanes(
            handle.0,
            planes.as_mut_ptr(),
            width,
            strides.as_ptr(),
            height,
            subsampling,
            &mut output_ptr,
            &mut output_len,
            i32::from(quality),
            tj::TJFLAG_NOREALLOC as i32,
        )
    };
    if result != 0 {
        return Err(turbo_error(handle.0));
    }
    let output_len = usize::try_from(output_len).map_err(integer_error)?;
    if output_ptr != output.as_mut_ptr() || output_len > output.len() {
        return Err(io::Error::other(
            "TurboJPEG violated the fixed output-buffer contract",
        ));
    }
    output.truncate(output_len);
    Ok(output)
}

fn turbo_error(handle: tj::tjhandle) -> io::Error {
    // SAFETY: TurboJPEG owns the returned nul-terminated error string for the
    // lifetime of the handle.
    let message = unsafe {
        let pointer = tj::tjGetErrorStr2(handle);
        if pointer.is_null() {
            "unknown TurboJPEG error".to_owned()
        } else {
            CStr::from_ptr(pointer).to_string_lossy().into_owned()
        }
    };
    io::Error::other(message)
}

pub(crate) fn decode_pisp_comp1(
    source: &[u8],
    width: u32,
    height: u32,
    stride: u32,
) -> io::Result<Vec<u16>> {
    if width == 0 || height == 0 || !width.is_multiple_of(8) || stride < width {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PiSP COMP1 dimensions require a non-zero width divisible by 8 and stride >= width",
        ));
    }
    let required = usize::try_from(stride)
        .map_err(integer_error)?
        .checked_mul(usize::try_from(height).map_err(integer_error)?)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "raw buffer size overflow"))?;
    if source.len() < required {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("raw buffer has {} bytes, expected {required}", source.len()),
        ));
    }

    let width_usize = usize::try_from(width).map_err(integer_error)?;
    let height_usize = usize::try_from(height).map_err(integer_error)?;
    let stride_usize = usize::try_from(stride).map_err(integer_error)?;
    let mut output = vec![0_u16; width_usize * height_usize];
    for y in 0..height_usize {
        let mut source_offset = y * stride_usize;
        let output_row = &mut output[y * width_usize..(y + 1) * width_usize];
        for block in output_row.as_chunks_mut::<8>().0 {
            let first =
                u32::from_le_bytes(source[source_offset..source_offset + 4].try_into().unwrap());
            let second = u32::from_le_bytes(
                source[source_offset + 4..source_offset + 8]
                    .try_into()
                    .unwrap(),
            );
            decode_pisp_subblock(block, first, 0);
            decode_pisp_subblock(block, second, 1);
            for sample in block {
                *sample = sample.saturating_add(PISP_COMPRESS_OFFSET);
            }
            source_offset += 8;
        }
    }
    Ok(output)
}

fn decode_pisp_subblock(output: &mut [u16], word: u32, parity: usize) {
    let mode = (word & 3) as u16;
    let values = if mode < 3 {
        let field0 = ((word >> 2) & 511) as u16;
        let field1 = ((word >> 11) & 127) as u16;
        let field2 = ((word >> 18) & 127) as u16;
        let field3 = ((word >> 25) & 127) as u16;
        let (q1, q2) = if mode == 2 && field0 >= 384 {
            (field0, field1 + 384)
        } else if field1 >= 64 {
            (field0, field0 + field1 - 64)
        } else {
            (field0 + 64 - field1, field0)
        };
        let mut p1 = q1.saturating_sub(64);
        let mut p2 = q2.saturating_sub(64);
        if mode == 2 {
            p1 = p1.min(384);
            p2 = p2.min(384);
        }
        [p1 + field2, q1, q2, p2 + field3]
    } else {
        let packed0 = ((word >> 2) & 32767) as u16;
        let packed1 = ((word >> 17) & 32767) as u16;
        [
            (packed0 & 15) + 16 * ((packed0 >> 8) / 11),
            (packed0 >> 4) % 176,
            (packed1 & 15) + 16 * ((packed1 >> 8) / 11),
            (packed1 >> 4) % 176,
        ]
    };
    for (index, value) in values.into_iter().enumerate() {
        output[parity + index * 2] = dequantize(value, mode);
    }
}

fn dequantize(value: u16, mode: u16) -> u16 {
    let value = u32::from(value);
    let expanded = match mode {
        0 => {
            if value < 320 {
                16 * value
            } else {
                32 * (value - 160)
            }
        }
        1 => 64 * value,
        2 => 128 * value,
        _ => {
            if value < 94 {
                256 * value
            } else {
                512 * (value - 47)
            }
        }
    };
    expanded.min(u32::from(u16::MAX)) as u16
}

/// Writes a DNG as the two-IFD tree real raw converters (and, notably,
/// macOS's raw decoder) actually require: a tiny placeholder "preview" as
/// IFD0 (the file's main/discoverable image, `NewSubfileType = 1`) whose
/// `SubIFDs` tag points at a second, otherwise-unlinked IFD holding the
/// full-resolution CFA sensor data (`NewSubfileType = 0`). A single flat
/// IFD containing the raw data directly — what this function used to
/// write — parses fine under a generic TIFF reader (tags are all
/// individually valid) but Apple's raw pipeline silently refuses to decode
/// it ("Cannot extract image from file"); see
/// `docs/optic-daemon-dng-compatibility.md` for the investigation that
/// found this.
pub(crate) fn encode_bayer16_dng(
    pixels: &[u16],
    width: u32,
    height: u32,
    metadata: &DngMetadata,
) -> io::Result<Vec<u8>> {
    let expected = usize::try_from(width)
        .map_err(integer_error)?
        .checked_mul(usize::try_from(height).map_err(integer_error)?)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "DNG dimensions overflow"))?;
    if pixels.len() != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("DNG pixel count is {}, expected {expected}", pixels.len()),
        ));
    }

    // SensorBlackLevels is R, Gr, Gb, B; DNG BlackLevel follows CFA order.
    let indices = match metadata.cfa_pattern {
        [0, 1, 1, 2] => [0, 1, 2, 3],
        [1, 0, 2, 1] => [1, 0, 3, 2],
        [1, 2, 0, 1] => [2, 3, 0, 1],
        [2, 1, 1, 0] => [3, 2, 1, 0],
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported Bayer CFA pattern",
            ));
        }
    };
    let black_levels = indices.map(|index| {
        let value = metadata.black_levels[index];
        Rational {
            n: value.max(0) as u32,
            d: 1,
        }
    });
    let neutral = [
        positive_rational(1.0 / metadata.colour_gains[0].max(0.000_001)),
        positive_rational(1.0),
        positive_rational(1.0 / metadata.colour_gains[1].max(0.000_001)),
    ];
    let color_matrix = camera_to_xyz(metadata.colour_correction_matrix, metadata.colour_gains)
        .map(signed_rational);
    let exposure = Rational {
        n: metadata.exposure_us.max(1) as u32,
        d: 1_000_000,
    };
    let iso = (metadata.analogue_gain.max(1.0) * 100.0)
        .round()
        .clamp(1.0, f32::from(u16::MAX)) as u16;

    let strip_bytes = pixels
        .len()
        .checked_mul(2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "DNG strip size overflow"))?;
    let strip_bytes = u32::try_from(strip_bytes).map_err(integer_error)?;

    let mut cursor = Cursor::new(Vec::new());
    {
        let mut encoder = TiffEncoder::new(&mut cursor).map_err(io::Error::other)?;

        // --- Raw sub-IFD: the actual full-resolution CFA sensor data.
        // Written first (so its file offset is known), but not chained
        // into the main IFD sequence — only IFD0's SubIFDs tag below
        // references it, exactly as `extra_directory` is documented to be
        // used for ("encode Exif directories or SubIfd directories").
        let mut raw = encoder.extra_directory().map_err(io::Error::other)?;
        raw.write_tag(Tag::NewSubfileType, 0_u32)
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::ImageWidth, width)
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::ImageLength, height)
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::BitsPerSample, 16_u16)
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::Compression, 1_u16)
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::PhotometricInterpretation, PHOTOMETRIC_CFA)
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::SamplesPerPixel, 1_u16)
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::PlanarConfiguration, 1_u16)
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::SampleFormat, 1_u16)
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::RowsPerStrip, height)
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::Unknown(TAG_CFA_REPEAT_PATTERN_DIM), &[2_u16, 2][..])
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::Unknown(TAG_CFA_PATTERN), &metadata.cfa_pattern[..])
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::Unknown(TAG_CFA_PLANE_COLOR), &[0_u8, 1, 2][..])
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::Unknown(TAG_CFA_LAYOUT), 1_u16)
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::Unknown(TAG_BLACK_LEVEL_REPEAT_DIM), &[2_u16, 2][..])
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::Unknown(TAG_BLACK_LEVEL), &black_levels[..])
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::Unknown(TAG_WHITE_LEVEL), u32::from(u16::MAX))
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::Unknown(TAG_DEFAULT_CROP_ORIGIN), &[0_u32, 0][..])
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::Unknown(TAG_DEFAULT_CROP_SIZE), &[width, height][..])
            .map_err(io::Error::other)?;
        raw.write_tag(
            Tag::Unknown(TAG_ACTIVE_AREA),
            &[0_u32, 0, height, width][..],
        )
        .map_err(io::Error::other)?;
        let strip_offset = raw.write_data(pixels).map_err(io::Error::other)?;
        let strip_offset = u32::try_from(strip_offset).map_err(integer_error)?;
        raw.write_tag(Tag::StripOffsets, strip_offset)
            .map_err(io::Error::other)?;
        raw.write_tag(Tag::StripByteCounts, strip_bytes)
            .map_err(io::Error::other)?;
        let raw_ifd = raw.finish_with_offsets().map_err(io::Error::other)?;

        // --- IFD0: a tiny placeholder "preview" — the file's main,
        // normally-discoverable image — carrying the camera/DNG
        // identification and as-shot color profile tags, plus the
        // `SubIFDs` pointer to the raw IFD above. DNG readers (including
        // macOS's) load the real pixel data via that pointer, not from
        // IFD0 itself.
        //
        // Deliberately *not* 1x1: macOS's QuickLook/Preview thumbnail
        // pipeline hangs indefinitely (0% CPU — genuinely stuck, not slow)
        // trying to scale a 1-pixel embedded preview for an icon/thumbnail,
        // even though the raw image itself decodes fine (`sips -s format
        // png` succeeds). A small-but-real aspect-preserving downscale
        // avoids whatever degenerate-size edge case that trips. See
        // `docs/optic-daemon-dng-compatibility.md` §5.
        let thumb_width = (width / 32).max(2);
        let thumb_height = (height / 32).max(2);
        let thumb_pixels = vec![128_u8; (thumb_width as usize) * (thumb_height as usize)];
        let mut thumbnail = encoder
            .new_image::<Gray8>(thumb_width, thumb_height)
            .map_err(io::Error::other)?;
        let tags = thumbnail.encoder();
        tags.write_tag(Tag::NewSubfileType, 1_u32)
            .map_err(io::Error::other)?;
        tags.write_tag(Tag::Make, "Raspberry Pi")
            .map_err(io::Error::other)?;
        tags.write_tag(Tag::Model, metadata.model.as_str())
            .map_err(io::Error::other)?;
        tags.write_tag(Tag::Software, "Optic native libcamera")
            .map_err(io::Error::other)?;
        tags.write_tag(Tag::Orientation, 1_u16)
            .map_err(io::Error::other)?;
        tags.write_tag(Tag::Unknown(TAG_DNG_VERSION), &DNG_VERSION[..])
            .map_err(io::Error::other)?;
        tags.write_tag(
            Tag::Unknown(TAG_DNG_BACKWARD_VERSION),
            &DNG_BACKWARD_VERSION[..],
        )
        .map_err(io::Error::other)?;
        tags.write_tag(
            Tag::Unknown(TAG_UNIQUE_CAMERA_MODEL),
            metadata.model.as_str(),
        )
        .map_err(io::Error::other)?;
        tags.write_tag(Tag::Unknown(TAG_COLOR_MATRIX_1), &color_matrix[..])
            .map_err(io::Error::other)?;
        tags.write_tag(Tag::Unknown(TAG_AS_SHOT_NEUTRAL), &neutral[..])
            .map_err(io::Error::other)?;
        tags.write_tag(Tag::Unknown(TAG_CALIBRATION_ILLUMINANT_1), 21_u16)
            .map_err(io::Error::other)?;
        tags.write_tag(Tag::Unknown(TAG_EXPOSURE_TIME), exposure)
            .map_err(io::Error::other)?;
        tags.write_tag(Tag::Unknown(TAG_ISO_SPEED_RATINGS), iso)
            .map_err(io::Error::other)?;
        tags.write_tag(Tag::SubIfd, raw_ifd.offset)
            .map_err(io::Error::other)?;
        thumbnail
            .write_data(&thumb_pixels)
            .map_err(io::Error::other)?;
    }
    Ok(cursor.into_inner())
}

fn positive_rational(value: f32) -> Rational {
    Rational {
        n: (value.max(0.0) * 1_000_000.0).round() as u32,
        d: 1_000_000,
    }
}

fn signed_rational(value: f32) -> SRational {
    SRational {
        n: (value * 1_000_000.0).round() as i32,
        d: 1_000_000,
    }
}

fn camera_to_xyz(ccm: [[f32; 3]; 3], gains: [f32; 2]) -> [f32; 9] {
    let rgb_to_xyz = [
        [0.412_456_4, 0.357_576_1, 0.180_437_5],
        [0.212_672_9, 0.715_152_2, 0.072_175],
        [0.019_333_9, 0.119_192, 0.950_304_1],
    ];
    let white_balance = [[gains[0], 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, gains[1]]];
    let product = multiply(multiply(rgb_to_xyz, ccm), white_balance);
    invert(product)
        .unwrap_or([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]])
        .concat()
        .try_into()
        .unwrap()
}

fn multiply(left: [[f32; 3]; 3], right: [[f32; 3]; 3]) -> [[f32; 3]; 3] {
    let mut output = [[0.0; 3]; 3];
    for row in 0..3 {
        for column in 0..3 {
            output[row][column] = (0..3)
                .map(|index| left[row][index] * right[index][column])
                .sum();
        }
    }
    output
}

fn invert(matrix: [[f32; 3]; 3]) -> Option<[[f32; 3]; 3]> {
    let determinant = matrix[0][0] * (matrix[1][1] * matrix[2][2] - matrix[1][2] * matrix[2][1])
        - matrix[0][1] * (matrix[1][0] * matrix[2][2] - matrix[1][2] * matrix[2][0])
        + matrix[0][2] * (matrix[1][0] * matrix[2][1] - matrix[1][1] * matrix[2][0]);
    if determinant.abs() < f32::EPSILON {
        return None;
    }
    let reciprocal = 1.0 / determinant;
    Some([
        [
            (matrix[1][1] * matrix[2][2] - matrix[1][2] * matrix[2][1]) * reciprocal,
            (matrix[0][2] * matrix[2][1] - matrix[0][1] * matrix[2][2]) * reciprocal,
            (matrix[0][1] * matrix[1][2] - matrix[0][2] * matrix[1][1]) * reciprocal,
        ],
        [
            (matrix[1][2] * matrix[2][0] - matrix[1][0] * matrix[2][2]) * reciprocal,
            (matrix[0][0] * matrix[2][2] - matrix[0][2] * matrix[2][0]) * reciprocal,
            (matrix[0][2] * matrix[1][0] - matrix[0][0] * matrix[1][2]) * reciprocal,
        ],
        [
            (matrix[1][0] * matrix[2][1] - matrix[1][1] * matrix[2][0]) * reciprocal,
            (matrix[0][1] * matrix[2][0] - matrix[0][0] * matrix[2][1]) * reciprocal,
            (matrix[0][0] * matrix[1][1] - matrix[0][1] * matrix[1][0]) * reciprocal,
        ],
    ])
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// Parses one IFD at `offset` into (field_type, count, inline-value-or-offset-bytes) by tag.
    /// Only handles fields whose value fits inline (<=4 bytes), which covers every scalar tag
    /// this module's tests inspect.
    fn read_ifd_at(data: &[u8], offset: usize) -> HashMap<u16, (u16, u32, [u8; 4])> {
        let count = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
        (0..count)
            .map(|index| {
                let start = offset + 2 + index * 12;
                (
                    u16::from_le_bytes(data[start..start + 2].try_into().unwrap()),
                    (
                        u16::from_le_bytes(data[start + 2..start + 4].try_into().unwrap()),
                        u32::from_le_bytes(data[start + 4..start + 8].try_into().unwrap()),
                        data[start + 8..start + 12].try_into().unwrap(),
                    ),
                )
            })
            .collect()
    }

    /// Parses IFD0, the one the TIFF header's first-IFD offset points to.
    fn little_endian_ifd(data: &[u8]) -> HashMap<u16, (u16, u32, [u8; 4])> {
        assert_eq!(&data[..4], b"II*\0");
        let offset = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
        read_ifd_at(data, offset)
    }

    fn inline_unsigned(entry: &(u16, u32, [u8; 4])) -> u32 {
        match entry.0 {
            3 => u32::from(u16::from_le_bytes(entry.2[..2].try_into().unwrap())),
            4 => u32::from_le_bytes(entry.2),
            field_type => panic!("unexpected TIFF integer type {field_type}"),
        }
    }

    #[test]
    fn pisp_zero_block_matches_reference_decoder() {
        let decoded = decode_pisp_comp1(&[0; 16], 8, 2, 8).unwrap();
        let row = [2048, 2048, 3072, 3072, 2048, 2048, 2048, 2048];
        assert_eq!(decoded, [row, row].concat());
    }

    #[test]
    fn pisp_decoder_rejects_truncated_or_unaligned_frames() {
        assert!(decode_pisp_comp1(&[0; 7], 8, 1, 8).is_err());
        assert!(decode_pisp_comp1(&[0; 8], 7, 1, 8).is_err());
    }

    #[test]
    fn dng_contains_required_cfa_metadata() {
        const TAG_NEW_SUBFILE_TYPE: u16 = 254;
        const TAG_SUB_IFD: u16 = 330;

        let dng =
            encode_bayer16_dng(&[PISP_COMPRESS_OFFSET; 64], 8, 8, &DngMetadata::default()).unwrap();

        // IFD0 is the tiny placeholder "preview" macOS/Apple's raw decoder
        // (and, per Adobe's own guidance, any DNG-compliant reader) expects
        // to find first — it must NOT be the raw CFA data directly.
        let ifd0 = little_endian_ifd(&dng);
        assert_eq!(
            inline_unsigned(&ifd0[&TAG_NEW_SUBFILE_TYPE]),
            1,
            "IFD0 must be marked as a reduced-resolution/preview image"
        );
        assert!(ifd0.contains_key(&TAG_DNG_VERSION));
        assert!(ifd0.contains_key(&TAG_COLOR_MATRIX_1));
        assert!(ifd0.contains_key(&TAG_AS_SHOT_NEUTRAL));
        assert!(
            !ifd0.contains_key(&TAG_CFA_PATTERN),
            "raw CFA tags belong in the sub-IFD, not IFD0"
        );

        // Follow IFD0's SubIfd pointer to the real raw image and confirm
        // the actual sensor data lives there.
        let sub_ifd_offset = inline_unsigned(&ifd0[&TAG_SUB_IFD]) as usize;
        let raw = read_ifd_at(&dng, sub_ifd_offset);
        assert_eq!(
            inline_unsigned(&raw[&TAG_NEW_SUBFILE_TYPE]),
            0,
            "the raw sub-IFD must be marked as the main/full-resolution image"
        );
        assert_eq!(inline_unsigned(&raw[&256]), 8); // ImageWidth
        assert_eq!(inline_unsigned(&raw[&257]), 8); // ImageLength
        assert_eq!(inline_unsigned(&raw[&262]), u32::from(PHOTOMETRIC_CFA));
        assert!(raw.contains_key(&TAG_CFA_PATTERN));
        assert!(raw.contains_key(&TAG_BLACK_LEVEL));
        assert!(raw.contains_key(&TAG_WHITE_LEVEL));
        assert!(
            !raw.contains_key(&TAG_COLOR_MATRIX_1),
            "as-shot color profile tags belong in IFD0, not the raw sub-IFD"
        );
    }

    #[test]
    fn yuv420_encoder_produces_a_jpeg() {
        let mut yuv = vec![128_u8; 8 * 8 + 4 * 4 * 2];
        yuv[..8 * 8].fill(96);
        let jpeg = encode_yuv420_jpeg(&yuv, 8, 8, 8, 90).unwrap();
        assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xff, 0xd9]);
    }
}
