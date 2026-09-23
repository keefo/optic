//! EXIF (`APP1`) metadata for captured JPEGs.
//!
//! turbojpeg emits a bare JPEG, so captures used to carry no metadata at
//! all: only the DNG recorded what the sensor actually did. This builds a
//! small EXIF block from the completed request's own metadata, which
//! `native_camera` splices in after `SOI`, so every tool (Finder, Preview,
//! Lightroom, LRTimelapse, exiftool) can read each frame's real shutter,
//! ISO and time. See `worklogs/2026-09-22-capture-exposure-metadata.md`.
//!
//! Deliberately cross-platform (unlike `native_codec`, which needs
//! libcamera and turbojpeg): this is pure byte assembly, so it is unit
//! tested on the development Mac and in CI, not only on the Pi.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::io;

const TAG_EXPOSURE_TIME: u16 = 33434;
const TAG_ISO_SPEED_RATINGS: u16 = 34855;

fn integer_error(error: std::num::TryFromIntError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}

const EXIF_TAG_IMAGE_DESCRIPTION: u16 = 270;
const EXIF_TAG_MAKE: u16 = 271;
const EXIF_TAG_MODEL: u16 = 272;
const EXIF_TAG_ORIENTATION: u16 = 274;
const EXIF_TAG_SOFTWARE: u16 = 305;
const EXIF_TAG_DATE_TIME: u16 = 306;
const EXIF_TAG_EXIF_IFD: u16 = 34665;
const EXIF_TAG_EXIF_VERSION: u16 = 36864;
const EXIF_TAG_DATE_TIME_ORIGINAL: u16 = 36867;
const EXIF_TAG_DATE_TIME_DIGITIZED: u16 = 36868;
const EXIF_TAG_USER_COMMENT: u16 = 37510;
const EXIF_TAG_COLOR_SPACE: u16 = 40961;
const EXIF_TAG_PIXEL_X_DIMENSION: u16 = 40962;
const EXIF_TAG_PIXEL_Y_DIMENSION: u16 = 40963;
const EXIF_TAG_WHITE_BALANCE: u16 = 41987;

const EXIF_TYPE_ASCII: u16 = 2;
const EXIF_TYPE_SHORT: u16 = 3;
const EXIF_TYPE_LONG: u16 = 4;
const EXIF_TYPE_RATIONAL: u16 = 5;
const EXIF_TYPE_UNDEFINED: u16 = 7;

/// What one capture recorded about itself, straight from the completed
/// request's metadata (never the requested settings).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExifMetadata {
    pub model: String,
    pub width: u32,
    pub height: u32,
    /// Local capture time, already formatted `YYYY:MM:DD HH:MM:SS` as EXIF
    /// requires.
    pub date_time: String,
    pub exposure_us: Option<i32>,
    pub analogue_gain: Option<f32>,
    pub colour_gains: Option<[f32; 2]>,
    /// True when white balance was fixed by the operator rather than left
    /// to per-frame AWB; EXIF only distinguishes auto from manual.
    pub manual_white_balance: bool,
}

/// One entry being assembled: the 12-byte IFD record, plus any value too
/// large to sit inline (> 4 bytes), which lives in a heap after the IFD.
struct ExifEntry {
    tag: u16,
    kind: u16,
    count: u32,
    inline: [u8; 4],
    overflow: Vec<u8>,
}

fn ascii_entry(tag: u16, value: &str) -> ExifEntry {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    sized_entry(tag, EXIF_TYPE_ASCII, bytes.len() as u32, bytes)
}

fn short_entry(tag: u16, value: u16) -> ExifEntry {
    let mut inline = [0_u8; 4];
    inline[..2].copy_from_slice(&value.to_le_bytes());
    ExifEntry {
        tag,
        kind: EXIF_TYPE_SHORT,
        count: 1,
        inline,
        overflow: Vec::new(),
    }
}

fn long_entry(tag: u16, value: u32) -> ExifEntry {
    ExifEntry {
        tag,
        kind: EXIF_TYPE_LONG,
        count: 1,
        inline: value.to_le_bytes(),
        overflow: Vec::new(),
    }
}

fn rational_entry(tag: u16, numerator: u32, denominator: u32) -> ExifEntry {
    let mut bytes = numerator.to_le_bytes().to_vec();
    bytes.extend_from_slice(&denominator.to_le_bytes());
    sized_entry(tag, EXIF_TYPE_RATIONAL, 1, bytes)
}

fn undefined_entry(tag: u16, bytes: Vec<u8>) -> ExifEntry {
    sized_entry(tag, EXIF_TYPE_UNDEFINED, bytes.len() as u32, bytes)
}

/// Inlines a value of 4 bytes or fewer, otherwise parks it in the overflow
/// heap and lets `encode_ifd` patch in its offset.
fn sized_entry(tag: u16, kind: u16, count: u32, bytes: Vec<u8>) -> ExifEntry {
    let mut inline = [0_u8; 4];
    if bytes.len() <= 4 {
        inline[..bytes.len()].copy_from_slice(&bytes);
        ExifEntry {
            tag,
            kind,
            count,
            inline,
            overflow: Vec::new(),
        }
    } else {
        ExifEntry {
            tag,
            kind,
            count,
            inline,
            overflow: bytes,
        }
    }
}

/// Serializes one IFD. `heap_start` is the TIFF-relative offset where this
/// IFD's overflow values begin (after the IFD itself), and `next_ifd` is the
/// link written in the IFD's trailer.
fn encode_ifd(entries: Vec<ExifEntry>, heap_start: u32, next_ifd: u32) -> (Vec<u8>, Vec<u8>) {
    let mut ifd = Vec::with_capacity(2 + entries.len() * 12 + 4);
    let mut heap = Vec::new();
    ifd.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for entry in entries {
        ifd.extend_from_slice(&entry.tag.to_le_bytes());
        ifd.extend_from_slice(&entry.kind.to_le_bytes());
        ifd.extend_from_slice(&entry.count.to_le_bytes());
        if entry.overflow.is_empty() {
            ifd.extend_from_slice(&entry.inline);
        } else {
            let offset = heap_start + heap.len() as u32;
            ifd.extend_from_slice(&offset.to_le_bytes());
            heap.extend_from_slice(&entry.overflow);
            // Values are word-aligned; EXIF readers expect even offsets.
            if !heap.len().is_multiple_of(2) {
                heap.push(0);
            }
        }
    }
    ifd.extend_from_slice(&next_ifd.to_le_bytes());
    (ifd, heap)
}

/// Builds the whole `APP1` segment, marker and length included.
pub(crate) fn exif_app1(metadata: &ExifMetadata) -> io::Result<Vec<u8>> {
    let comment = match metadata.colour_gains {
        Some([red, blue]) => format!("colour gains red {red:.4} blue {blue:.4}"),
        None => "colour gains unavailable".to_owned(),
    };
    // UserComment is 8 bytes of character-set id followed by the text.
    let mut user_comment = b"ASCII\0\0\0".to_vec();
    user_comment.extend_from_slice(comment.as_bytes());

    let mut exif_entries = vec![undefined_entry(EXIF_TAG_EXIF_VERSION, b"0231".to_vec())];
    if let Some(exposure_us) = metadata.exposure_us.filter(|value| *value > 0) {
        exif_entries.push(rational_entry(
            TAG_EXPOSURE_TIME,
            u32::try_from(exposure_us).map_err(integer_error)?,
            1_000_000,
        ));
    }
    if let Some(gain) = metadata.analogue_gain.filter(|value| *value > 0.0) {
        // EXIF has no analogue-gain tag; the convention is ISO = gain x 100,
        // matching what the DNG writer already records.
        let iso = (f64::from(gain) * 100.0)
            .round()
            .clamp(1.0, f64::from(u16::MAX));
        exif_entries.push(short_entry(TAG_ISO_SPEED_RATINGS, iso as u16));
    }
    exif_entries.push(ascii_entry(
        EXIF_TAG_DATE_TIME_ORIGINAL,
        &metadata.date_time,
    ));
    exif_entries.push(ascii_entry(
        EXIF_TAG_DATE_TIME_DIGITIZED,
        &metadata.date_time,
    ));
    exif_entries.push(undefined_entry(EXIF_TAG_USER_COMMENT, user_comment));
    exif_entries.push(short_entry(EXIF_TAG_COLOR_SPACE, 1)); // sRGB
    exif_entries.push(long_entry(EXIF_TAG_PIXEL_X_DIMENSION, metadata.width));
    exif_entries.push(long_entry(EXIF_TAG_PIXEL_Y_DIMENSION, metadata.height));
    exif_entries.push(short_entry(
        EXIF_TAG_WHITE_BALANCE,
        u16::from(metadata.manual_white_balance),
    ));
    exif_entries.sort_by_key(|entry| entry.tag);

    let ifd0_entry_count = 7_u32;
    let ifd0_len = 2 + ifd0_entry_count * 12 + 4;
    // IFD0 sits right after the 8-byte TIFF header; its heap follows it, and
    // the Exif IFD follows that. Sizes are known up front because the heap
    // only depends on the strings already built.
    let mut ifd0_entries = vec![
        ascii_entry(EXIF_TAG_IMAGE_DESCRIPTION, "Project Optic capture"),
        ascii_entry(EXIF_TAG_MAKE, "Raspberry Pi"),
        ascii_entry(EXIF_TAG_MODEL, &metadata.model),
        short_entry(EXIF_TAG_ORIENTATION, 1),
        ascii_entry(EXIF_TAG_SOFTWARE, "Optic native libcamera"),
        ascii_entry(EXIF_TAG_DATE_TIME, &metadata.date_time),
    ];
    let ifd0_heap_len: u32 = ifd0_entries
        .iter()
        .map(|entry| {
            let len = entry.overflow.len() as u32;
            len + (len % 2)
        })
        .sum();
    let exif_ifd_offset = 8 + ifd0_len + ifd0_heap_len;
    ifd0_entries.push(long_entry(EXIF_TAG_EXIF_IFD, exif_ifd_offset));
    ifd0_entries.sort_by_key(|entry| entry.tag);
    debug_assert_eq!(ifd0_entries.len() as u32, ifd0_entry_count);

    let (ifd0, ifd0_heap) = encode_ifd(ifd0_entries, 8 + ifd0_len, 0);
    let exif_heap_start = exif_ifd_offset + 2 + exif_entries.len() as u32 * 12 + 4;
    let (exif_ifd, exif_heap) = encode_ifd(exif_entries, exif_heap_start, 0);

    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II\x2a\x00");
    tiff.extend_from_slice(&8_u32.to_le_bytes());
    tiff.extend_from_slice(&ifd0);
    tiff.extend_from_slice(&ifd0_heap);
    tiff.extend_from_slice(&exif_ifd);
    tiff.extend_from_slice(&exif_heap);

    let mut payload = b"Exif\0\0".to_vec();
    payload.extend_from_slice(&tiff);
    let length = u16::try_from(payload.len() + 2)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "EXIF block exceeds 64 KiB"))?;

    let mut segment = vec![0xFF, 0xE1];
    segment.extend_from_slice(&length.to_be_bytes());
    segment.extend_from_slice(&payload);
    Ok(segment)
}

/// Splices an `APP1` segment in directly after `SOI`, leaving every other
/// byte of the encoded image untouched. A buffer that isn't a JPEG is
/// returned unchanged rather than corrupted.
pub(crate) fn insert_exif(jpeg: Vec<u8>, segment: &[u8]) -> Vec<u8> {
    if jpeg.len() < 2 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return jpeg;
    }
    let mut out = Vec::with_capacity(jpeg.len() + segment.len());
    out.extend_from_slice(&jpeg[..2]);
    out.extend_from_slice(segment);
    out.extend_from_slice(&jpeg[2..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn sample() -> ExifMetadata {
        ExifMetadata {
            model: "imx477".to_owned(),
            width: 4056,
            height: 3040,
            date_time: "2026:09:22 15:28:02".to_owned(),
            exposure_us: Some(626),
            analogue_gain: Some(1.0),
            colour_gains: Some([2.6337, 1.8228]),
            manual_white_balance: true,
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Value {
        Ascii(String),
        Short(u16),
        Long(u32),
        Rational(u32, u32),
        Undefined(Vec<u8>),
    }

    fn u16le(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([bytes[at], bytes[at + 1]])
    }

    fn u32le(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    /// Minimal TIFF/EXIF reader: parses one IFD from `tiff` at `offset`,
    /// so the tests assert against a real parse rather than the bytes the
    /// writer happened to emit.
    fn read_ifd(tiff: &[u8], offset: usize) -> HashMap<u16, Value> {
        let count = usize::from(u16le(tiff, offset));
        let mut out = HashMap::new();
        for index in 0..count {
            let entry = offset + 2 + index * 12;
            let tag = u16le(tiff, entry);
            let kind = u16le(tiff, entry + 2);
            let len = u32le(tiff, entry + 4) as usize;
            let inline = &tiff[entry + 8..entry + 12];
            let size = match kind {
                EXIF_TYPE_ASCII | EXIF_TYPE_UNDEFINED => len,
                EXIF_TYPE_SHORT => len * 2,
                EXIF_TYPE_LONG => len * 4,
                EXIF_TYPE_RATIONAL => len * 8,
                other => panic!("unexpected type {other}"),
            };
            let data: Vec<u8> = if size <= 4 {
                inline[..size].to_vec()
            } else {
                let at = u32le(tiff, entry + 8) as usize;
                assert!(at.is_multiple_of(2), "value offsets must be word aligned");
                tiff[at..at + size].to_vec()
            };
            let value = match kind {
                EXIF_TYPE_ASCII => {
                    Value::Ascii(String::from_utf8(data[..data.len() - 1].to_vec()).expect("ascii"))
                }
                EXIF_TYPE_SHORT => Value::Short(u16::from_le_bytes([data[0], data[1]])),
                EXIF_TYPE_LONG => Value::Long(u32le(&data, 0)),
                EXIF_TYPE_RATIONAL => Value::Rational(u32le(&data, 0), u32le(&data, 4)),
                _ => Value::Undefined(data),
            };
            out.insert(tag, value);
        }
        out
    }

    fn parse(segment: &[u8]) -> (HashMap<u16, Value>, HashMap<u16, Value>) {
        assert_eq!(&segment[..2], &[0xFF, 0xE1], "APP1 marker");
        let declared = u16::from_be_bytes([segment[2], segment[3]]) as usize;
        assert_eq!(declared, segment.len() - 2, "segment length field");
        assert_eq!(&segment[4..10], b"Exif\0\0");
        let tiff = &segment[10..];
        assert_eq!(&tiff[..4], b"II\x2a\x00", "little-endian TIFF header");
        let ifd0 = read_ifd(tiff, u32le(tiff, 4) as usize);
        let Value::Long(exif_offset) = ifd0[&EXIF_TAG_EXIF_IFD] else {
            panic!("missing Exif IFD pointer");
        };
        let exif = read_ifd(tiff, exif_offset as usize);
        (ifd0, exif)
    }

    #[test]
    fn exif_records_what_the_sensor_actually_did() {
        let (ifd0, exif) = parse(&exif_app1(&sample()).unwrap());

        assert_eq!(ifd0[&EXIF_TAG_MAKE], Value::Ascii("Raspberry Pi".into()));
        assert_eq!(ifd0[&EXIF_TAG_MODEL], Value::Ascii("imx477".into()));
        assert_eq!(
            ifd0[&EXIF_TAG_SOFTWARE],
            Value::Ascii("Optic native libcamera".into())
        );
        assert_eq!(
            ifd0[&EXIF_TAG_DATE_TIME],
            Value::Ascii("2026:09:22 15:28:02".into())
        );
        assert_eq!(ifd0[&EXIF_TAG_ORIENTATION], Value::Short(1));

        // 626 µs, exactly as the frame reported it.
        assert_eq!(exif[&TAG_EXPOSURE_TIME], Value::Rational(626, 1_000_000));
        // Gain 1.0 -> ISO 100, the same convention the DNG writer uses.
        assert_eq!(exif[&TAG_ISO_SPEED_RATINGS], Value::Short(100));
        assert_eq!(
            exif[&EXIF_TAG_DATE_TIME_ORIGINAL],
            Value::Ascii("2026:09:22 15:28:02".into())
        );
        assert_eq!(exif[&EXIF_TAG_PIXEL_X_DIMENSION], Value::Long(4056));
        assert_eq!(exif[&EXIF_TAG_PIXEL_Y_DIMENSION], Value::Long(3040));
        assert_eq!(exif[&EXIF_TAG_COLOR_SPACE], Value::Short(1));
        // Fixed white balance reads as "manual".
        assert_eq!(exif[&EXIF_TAG_WHITE_BALANCE], Value::Short(1));
        let Value::Undefined(comment) = &exif[&EXIF_TAG_USER_COMMENT] else {
            panic!("missing UserComment");
        };
        assert_eq!(&comment[..8], b"ASCII\0\0\0");
        assert_eq!(
            String::from_utf8_lossy(&comment[8..]),
            "colour gains red 2.6337 blue 1.8228"
        );
    }

    #[test]
    fn iso_follows_analogue_gain() {
        let iso = |gain: f32| {
            let (_, exif) = parse(
                &exif_app1(&ExifMetadata {
                    analogue_gain: Some(gain),
                    ..sample()
                })
                .unwrap(),
            );
            exif.get(&TAG_ISO_SPEED_RATINGS).cloned()
        };
        assert_eq!(iso(1.0), Some(Value::Short(100)));
        assert_eq!(iso(15.515152), Some(Value::Short(1552)));
        assert_eq!(iso(8.0), Some(Value::Short(800)));
    }

    #[test]
    fn missing_metadata_is_omitted_rather_than_guessed() {
        let (_, exif) = parse(
            &exif_app1(&ExifMetadata {
                exposure_us: None,
                analogue_gain: None,
                colour_gains: None,
                manual_white_balance: false,
                ..sample()
            })
            .unwrap(),
        );
        assert!(!exif.contains_key(&TAG_EXPOSURE_TIME));
        assert!(!exif.contains_key(&TAG_ISO_SPEED_RATINGS));
        assert_eq!(exif[&EXIF_TAG_WHITE_BALANCE], Value::Short(0));
        let Value::Undefined(comment) = &exif[&EXIF_TAG_USER_COMMENT] else {
            panic!("missing UserComment");
        };
        assert_eq!(
            String::from_utf8_lossy(&comment[8..]),
            "colour gains unavailable"
        );
        // A zero exposure is "unknown", not a real 0 s shutter.
        let (_, zeroed) = parse(
            &exif_app1(&ExifMetadata {
                exposure_us: Some(0),
                ..sample()
            })
            .unwrap(),
        );
        assert!(!zeroed.contains_key(&TAG_EXPOSURE_TIME));
    }

    #[test]
    fn insert_exif_only_prefixes_the_scan_data() {
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xDB, 1, 2, 3, 0xFF, 0xD9];
        let segment = exif_app1(&sample()).unwrap();
        let out = insert_exif(jpeg.clone(), &segment);
        assert_eq!(&out[..2], &[0xFF, 0xD8]);
        assert_eq!(&out[2..2 + segment.len()], &segment[..]);
        // Everything turbojpeg produced survives byte for byte.
        assert_eq!(&out[2 + segment.len()..], &jpeg[2..]);
    }

    #[test]
    fn a_non_jpeg_buffer_is_returned_untouched() {
        let not_jpeg = vec![0x89, 0x50, 0x4E, 0x47];
        let segment = exif_app1(&sample()).unwrap();
        assert_eq!(insert_exif(not_jpeg.clone(), &segment), not_jpeg);
        assert_eq!(insert_exif(Vec::new(), &segment), Vec::<u8>::new());
    }
}
