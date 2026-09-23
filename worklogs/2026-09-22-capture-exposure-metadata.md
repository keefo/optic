# Dated Worklog: 2026-09-22 - Exposure Metadata in JPEG EXIF and the Sidecar

Status: **implemented and host-tested; not deployed** (the Pi is mid-run with
the user's 6-hour timelapse, so no deploy and no on-camera check yet).

## Objective

Record what the camera *actually* did with each capture, so a frame can be
diagnosed and post-processed from metadata alone:

1. **EXIF in the JPEG** (`APP1`), so Finder, Preview, Lightroom, exiftool and
   LRTimelapse can read shutter, ISO, time and camera.
2. **Actual exposure in the `.log.json` sidecar**, alongside the settings that
   were *requested*.

## Why (user request, 2026-09-22)

A frame in the user's run (15:28:02) came out ~0.8 EV darker than its
neighbours. Answering "did the camera use the exposure the ramp asked for?"
needed a pixel investigation, because:

- The sidecar records `settings` — what was **requested** (626 µs there).
- The JPEG carries **no EXIF at all** (`ffprobe` shows no tags); the daemon
  encodes bare JPEG through turbojpeg.
- Only the DNG path records reality (`ExposureTime`, `ISOSpeedRatings`,
  `AsShotNeutral` — `native_codec.rs`), and that run was JPEG-only.

The values already exist in the capture path: each completed request's
`ExposureTime`, `AnalogueGain` and `ColourGains` are read for the DNG and,
since the exposure-ramping branch, for the ramp (`CaptureResult.exposure`,
which also carries the frame meter). They are simply discarded for JPEG.

## Scope note

Built on the `exposure-ramping` branch because it reuses `CaptureExposure`
and `FrameMeter` from PR #16. Touches `src/native_codec.rs` and
`src/optic_capture_log.rs`, which are outside the exposure-ramping track's
owned files; recorded here per `CLAUDE.md`.

## Acceptance criteria

E1. A JPEG from a still capture contains a valid `APP1`/`Exif\0\0` segment
    immediately after `SOI`, and the rest of the JPEG stream is byte-identical
    to what turbojpeg produced.
E2. It records, from the frame's own metadata (not the request):
    `ExposureTime`, `ISOSpeedRatings` (analogue gain × 100), `DateTimeOriginal`
    /`DateTimeDigitized`/`DateTime`, `Make`, `Model`, `Software`,
    `Orientation`, `ColorSpace`, `PixelXDimension`/`PixelYDimension`,
    `WhiteBalance`, and a `UserComment` carrying the colour gains.
E3. `exiftool`/`ffprobe` read the values back and they match the capture's
    own log entry.
E4. The sidecar gains `exposure` with the actual `exposure_us`,
    `analogue_gain`, `colour_gains` and the frame `meter`; `#[serde(default)]`
    so sidecars and database rows written before this field still parse.
E5. A capture whose backend reports no metadata still writes a valid JPEG
    (no EXIF) and a valid sidecar (`exposure: null`).
E6. No measurable capture-latency regression: building EXIF is byte work on a
    few hundred bytes.

## Test plan (written before implementation)

Host unit tests (`cargo test`, Mac):

- `native_codec`: `exif_app1` produces a segment whose length field matches,
  starts with `Exif\0\0` + `II*\0`, and whose IFD0/ExifIFD offsets and counts
  are self-consistent; a tiny in-test TIFF reader parses it back and asserts
  `ExposureTime` = 626/1_000_000 s, ISO 100, the timestamp string, and the
  `UserComment` text.
- `insert_exif` puts the segment after `SOI`, leaves the remaining bytes
  untouched, and returns the input unchanged when it isn't a JPEG.
- Rounding: 626 µs → rational; gain 1.0 → ISO 100; gain 15.5 → ISO 1550.
- `optic_capture_log`: an entry with `exposure` round-trips through JSON; a
  JSON body without the field deserializes with `exposure: None`.
- `camera`: `CaptureExposure`/`FrameMeter` round-trip (they gain
  `Deserialize`).

Linux/hardware (needs user approval — the Pi is mid-run):

- Deploy, take one test shot per profile, then `exiftool` the JPEG and compare
  against the sidecar and the DNG's own tags.
- Confirm the JPEG still opens in Preview/Finder and that Quick Look previews
  it (EXIF errors would break both).

## Files

`src/native_codec.rs` (EXIF writer), `src/native_camera.rs` (build and insert
it), `src/camera.rs` (`Deserialize` on the metadata types),
`src/optic_capture_log.rs` (sidecar field), `docs/optic-daemon-capture-log.md`
and `docs/optic-daemon.md` (document the new metadata).

## Implementation

- `src/exif.rs` (new, **cross-platform on purpose**): `ExifMetadata`,
  `exif_app1` (builds the whole `APP1` segment: TIFF header, IFD0, Exif IFD,
  word-aligned value heaps) and `insert_exif` (splices it after `SOI`).
  `native_codec.rs` is Linux-only, so putting it there would have made the
  byte logic untestable on the Mac and in CI; this module is pure bytes.
- `src/native_camera.rs`: `encode_jpeg` now builds the block from the frame's
  own metadata and inserts it; `exif_date_time` formats UTC as EXIF requires.
  A failure to build EXIF is logged and skipped, never fatal.
- `src/optic_capture_log.rs`: `CaptureLogEntry.exposure`
  (`Option<CaptureExposure>`, `#[serde(default)]`), populated from the
  capture result, so it lands in both the `.log.json` sidecar and the SQLite
  row.
- `src/camera.rs`, `src/exposure_ramp.rs`: `CaptureExposure` and `FrameMeter`
  gain `Deserialize` (they are now read back from sidecars).
- Docs: `docs/optic-daemon-capture-log.md` §3.3 (sidecar) and §3.4 (EXIF).

## Validation

Mac: `cargo fmt --check`, **246 tests** (7 new), `cargo clippy --all-targets
-- -D warnings`, all clean.

New tests:
- `exif`: a small in-test TIFF reader parses the produced segment back and
  asserts the APP1 marker, the declared length, `Exif\0\0`, the header, and
  every tag's value (626 µs as 626/1000000, ISO 100, timestamps, dimensions,
  sRGB, manual white balance, and the `UserComment` gains text) — E1/E2.
- ISO follows gain (1.0 → 100, 8.0 → 800, 15.515152 → 1552).
- Missing metadata omits `ExposureTime`/`ISOSpeedRatings` instead of writing
  zeros, and a `0 µs` exposure counts as unknown — E5.
- `insert_exif` leaves every byte turbojpeg produced intact, and returns a
  non-JPEG buffer unchanged.
- `optic_capture_log`: an entry records the actual exposure and round-trips
  through JSON; a failed capture has `exposure: None`; a sidecar written
  before the field existed still parses — E4.

**Not verified:** E3 and E6 need hardware. `native_camera.rs` is Linux-only
and does not compile on macOS, so the wiring there is checked by CI's
`Rust (Debian 13 arm64)` job and, before deploying, by the deploy script's
own on-Pi build. Two mistakes in that code were caught by review rather than
the compiler (`model` was not in scope in `publish_capture`, and a
`Default` impl I should not have assumed).

## Next steps

1. Wait for the user's timelapse run to finish; deploy then.
2. On hardware: one test shot per profile, then `exiftool` the JPEG and
   compare against its sidecar and the DNG's tags (E3), and confirm
   Preview/Quick Look still open it.
