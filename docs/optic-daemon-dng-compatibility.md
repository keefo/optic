# Design Note: DNG macOS Compatibility Fix

> **Status:** Closed from this project's side. The real container-structure
> bug (§§1-4) is root-caused and fixed in
> `src/native_codec.rs::encode_bayer16_dng`; every DNG the daemon produces
> is now confirmed spec-compliant and decodes correctly via `sips` and any
> real raw processor. A second, separate symptom — Preview/QuickLook
> hanging when generating a thumbnail — turned out to be a confirmed
> macOS 15.8 (24H23) bug in Apple's own RawCamera plugin, unrelated to
> anything this project controls (§5). See
> `worklogs/2026-09-18-dng-macos-compatibility.md` for the full test plan
> and validation record.

## 1. Symptom

A real DNG captured by the daemon
(`optic-web-master-archive-1789713405041.dng`, captured before the
`testshot-` filename rename) would not open on macOS: `sips` recognized the
file as a DNG and could read its Make/Model/Software tags, but
`sips -s format png ... --out ...` failed with `Error: Cannot extract image
from file` (exit code 13). Preview/Photos would show the same failure.

## 2. Investigation

The file was not corrupt at the byte level — its TIFF/IFD structure parsed
cleanly, and the raw pixel data itself (hand-decoded from the strip data)
spanned a plausible, non-clipped 16-bit tonal range. This ruled out simple
explanations (truncated file, bad strip offsets, all-black data from a
levels bug) and pointed at something structural that a generic TIFF/EXIF
reader tolerates but Apple's raw decoder does not.

Manually parsing the file's single IFD showed every individual tag value
was reasonable (`ImageWidth`/`ImageLength`/`BitsPerSample`/`Compression`/
`PhotometricInterpretation = 32803 (CFA)`/`CFAPattern`/`BlackLevel`/
`WhiteLevel`/`ColorMatrix1`/etc. all present and internally consistent) —
but the file had **only one IFD**, with the raw CFA sensor data written
directly into it.

Cross-referencing Adobe's own DNG documentation (see Sources) confirmed the
actual required shape: *"A DNG's defining shape is an IFD tree... IFD0
holds a small preview/thumbnail and points, through the `SubIFDs` tag, at
the full-resolution raw image in a sub-IFD... the raw subIFD requires the
`NewSubfileType` tag, and its value must be 0."* Our encoder had neither a
`SubIFDs` tag (330) nor a `NewSubfileType` tag (254) anywhere in the file —
confirmed absent by direct inspection of every IFD entry. Real camera DNGs,
and apparently Apple's raw pipeline specifically, depend on this two-tier
tree even though a looser reading of baseline TIFF would technically permit
a flat single-IFD raw file (which is why more lenient tools, e.g. whatever
originally produced/consumed this file without complaint, didn't catch it).

## 3. Fix

`encode_bayer16_dng` now writes two IFDs instead of one, using the `tiff`
crate's `extra_directory()` (an unchained `DirectoryEncoder`, documented for
exactly this: *"encode Exif directories or SubIfd directories"*) alongside
its normal chained `new_image()`:

1. **Raw sub-IFD** (written first, via `extra_directory()` so it is *not*
   linked into the main IFD sequence — only reachable via IFD0's pointer):
   `NewSubfileType = 0`, `ImageWidth`/`ImageLength`/`BitsPerSample`/
   `Compression`/`PhotometricInterpretation`/`SamplesPerPixel`/
   `PlanarConfiguration`/`SampleFormat`/`RowsPerStrip`/`StripOffsets`/
   `StripByteCounts`, plus every CFA/DNG raw-image tag that was previously
   in the single IFD: `CFARepeatPatternDim`, `CFAPattern`, `CFAPlaneColor`,
   `CFALayout`, `BlackLevelRepeatDim`, `BlackLevel`, `WhiteLevel`,
   `DefaultCropOrigin`, `DefaultCropSize`, `ActiveArea`.
2. **IFD0** (written second, via the normal chained `new_image::<Gray8>(1,
   1)` — becomes the file's discoverable "first IFD" automatically):
   `NewSubfileType = 1`, `Make`/`Model`/`Software`/`Orientation`,
   `DNGVersion`/`DNGBackwardVersion`/`UniqueCameraModel`, the as-shot color
   profile (`ColorMatrix1`/`AsShotNeutral`/`CalibrationIlluminant1`),
   `ExposureTime`/`ISOSpeedRatings`, and a `SubIfd` tag holding the raw
   IFD's file offset (from `DirectoryOffset::offset`, captured before IFD0
   is written). The placeholder image itself is a single mid-gray pixel —
   there is no real preview-rendering pipeline; it exists purely to give
   readers a structurally valid, spec-compliant IFD0 to land on.

The tag *values* are unchanged from the previous version — this is purely a
structural fix (where each tag lives), not a change to color/exposure/
calibration data. `ColorMatrix1`/`AsShotNeutral`/`CalibrationIlluminant1`
moved from the (now-raw) IFD to IFD0 to match how real camera DNGs place
as-shot camera-profile data, distinct from per-image raw pixel-format data.

## 4. Verification

- Unit test `dng_contains_required_cfa_metadata` (`src/native_codec.rs`)
  rewritten to follow the `SubIfd` pointer and assert the tree shape:
  `NewSubfileType` correct in both IFDs, CFA/raw tags only in the sub-IFD,
  color-profile tags only in IFD0. This runs on the Pi-native test target
  only (`native_codec` is `#[cfg(target_os = "linux")]`) — see the worklog
  for the actual run.
- Empirical check: a fresh real capture from the deployed fix, copied to
  the Mac and opened with `sips`/Preview — see the worklog's Validation
  section for the result.

## 5. Follow-up: QuickLook Thumbnail Hang (Root-Caused as an External Apple Bug)

The first deployed version of this fix (still 1×1 IFD0 placeholder)
produced a DNG that `sips -s format png` converted correctly — a real,
sharp, correctly color-processed 4056×3040 photo — but Preview.app hung
and then crashed trying to open the same file. Reproduced deterministically
and without needing the GUI via `qlmanage -t -s 512 -o <dir> <file>.dng`:
the process hangs indefinitely at **0% CPU** (genuinely stuck/blocked, not
slow-but-working — contrast with `sips`'s real, ~7s, CPU-active conversion
of the same file). `qlmanage -t` exercises the same QuickLook/ImageIO
thumbnail-generation path Preview and Finder icon view use, which is
distinct from the full-image raw-decode path `sips -s format` exercises.

**Initial hypothesis (disproven):** a 1×1 pixel embedded preview being a
degenerate size for Apple's thumbnail-scaling code. Deployed a fix
(non-degenerate `width/32`×`height/32` placeholder) — the hang persisted
identically on a fresh real capture. Wrong hypothesis.

**Local bisection.** Rather than keep round-tripping through a Pi
build+deploy+capture cycle to test structural hypotheses, built a
standalone local Python TIFF/DNG writer reusing the real extracted raw
pixel bytes and tag values from an actual captured file, letting each
hypothesis be tested against `qlmanage -t` in seconds. Systematically ruled
out, in order: the private color/calibration tags in IFD0; raw-payload size
(tested down to a 64×64 synthetic payload); the two-IFD/`SubIfd` structure
itself (reverted to the *pre-fix* flat single-IFD shape through the same
harness — `sips` failed cleanly exactly as the original bug did, but
`qlmanage -t` **still hung**); and unrecognized camera identity (swapped
`Make`/`Model` for a real, Apple-listed camera — still hung). Also reset
`quicklookd` in case repeated test-process kills had wedged the shared
thumbnail service — no change.

**Conclusive control test.** The user supplied two real, professionally
shot DNG files downloaded from elsewhere (Canon EOS 350D, Leica M9 Digital
Camera — the Leica is on Apple's own officially-supported-camera list for
this OS version). Both decode perfectly via `sips` but **both hang
identically in `qlmanage -t`**, exactly like every one of our own files.
This is conclusive: the hang is unrelated to anything this project's DNG
encoder does.

**Confirmed external root cause.** An
[Apple Discussions thread](https://discussions.apple.com/thread/256358754?sortBy=rank)
reports Photos.app freezing/crashing on RAW files (Nikon NEF, Canon)
specifically on **macOS 15.8 (24H23)** — the exact OS build this Mac
runs — introduced by that update. Its technical analysis: *"a deadlock
inside the RawCamera plugin (ImageIO framework) while reading/adding
metadata properties,"* with the main thread blocked on a mutex that never
releases — precisely matching the observed indefinite 0% CPU hang (a
deadlock signature, not a slow operation). No fix exists yet; the
thread's own conclusion is that this is an Apple bug in the 15.8 update
requiring an Apple-side fix.

**Consequence for this project:** none. The daemon's DNG output is fully
correct and spec-compliant (§§1-4, independently verified via a
structural TIFF/DNG conformance check — see the worklog). Full-resolution
decode already works via `sips` and any raw editor with its own decoder.
Only Apple's QuickLook-based thumbnail generation is affected, on this
specific macOS version, for *any* DNG file regardless of origin — nothing
further to change here. If Apple ships a fix in a future macOS update,
thumbnails should start working with no daemon-side change required.

## Sources

- [What are the minimum required tags for a DNG file? (Adobe Community)](https://community.adobe.com/questions-563/what-are-the-minimum-required-tags-for-a-dng-file-182411)
- [Digital Negative (DNG) Specification v1.6.0.0 (Adobe)](https://paulbourke.net/dataformats/dng/dng_spec_1_6_0_0.pdf)
- [Apple Discussions: Photos freezing/crashing editing RAW files on macOS 15.8 (24H23)](https://discussions.apple.com/thread/256358754?sortBy=rank) — independent confirmation of the RawCamera plugin deadlock this project's investigation reproduced.
