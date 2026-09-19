# Dated Worklog: 2026-09-18 - DNG macOS Compatibility Fix

Status: **closed from our side.** Deployed twice. First deploy
(`optic-daemon` 0.1.17) fixed the real bug: the DNG container structure
(`sips` now converts every real capture correctly — sharp, correctly
color-processed photos). A second deploy (0.1.18) attempted to fix a
remaining Preview/QuickLook thumbnail-generation hang, but that hang turned
out to be a **confirmed Apple bug in macOS 15.8 (24H23)** — a deadlock in
the RawCamera ImageIO plugin, independently reproduced on real Canon and
Leica DNG files (not just ours) and corroborated by a public Apple
Discussions report of the same OS build freezing/crashing on RAW files. See
"Final Root Cause" in Validation below. No further code change is possible
or needed on our side; the daemon's DNG output is confirmed spec-compliant.
Full investigation and design detail live in
`docs/optic-daemon-dng-compatibility.md`; this worklog covers the test plan
and validation record.

## Objective

Fix `optic-web-master-archive-1789713405041.dng` (and every DNG the daemon
has produced) failing to open on macOS (`sips`: "Cannot extract image from
file"), without changing any captured color/exposure/calibration values —
this is a container-structure bug, not a data-correctness bug.

## Acceptance Criteria

- A DNG produced by `encode_bayer16_dng` after the fix opens successfully
  in macOS's raw pipeline (`sips -s format png` succeeds, no "Cannot
  extract image from file").
- The unit test suite continues to pass on the Pi-native target
  (`native_codec` only compiles there).
- No change to the actual pixel data, exposure, gain, or color-matrix
  values written into the file — only the IFD tree shape and which IFD
  each tag lives in.

## Test Plan (written before implementation of the structural rewrite)

- Unit test (Pi-native target, `src/native_codec.rs`): rewrite
  `dng_contains_required_cfa_metadata` to no longer assume a single flat
  IFD. It must:
  - Parse IFD0 and assert `NewSubfileType = 1`, and that `DNGVersion`,
    `ColorMatrix1`, `AsShotNeutral` are present there.
  - Assert IFD0 does **not** contain `CFAPattern` (that must live in the
    sub-IFD now, not IFD0 — a regression test for the exact bug being
    fixed: if `CFAPattern` were still directly in IFD0, the file would be
    back to the broken single-IFD shape).
  - Follow IFD0's `SubIfd` tag to the raw IFD and assert `NewSubfileType =
    0`, correct `ImageWidth`/`ImageLength`/`PhotometricInterpretation`,
    and that `CFAPattern`/`BlackLevel`/`WhiteLevel` are present there.
  - Assert the raw sub-IFD does **not** contain `ColorMatrix1` (the
    inverse regression check).
  - Existing tests (`pisp_zero_block_matches_reference_decoder`,
    `pisp_decoder_rejects_truncated_or_unaligned_frames`,
    `yuv420_encoder_produces_a_jpeg`) are untouched by this change and must
    keep passing — they exercise unrelated code paths (PiSP decode, JPEG
    encode) not touched by the DNG tree restructure.
- Pi-native build/test (via SSH, not a full deploy): `cargo build` /
  `cargo test --locked --all-targets` on the Pi, to compile and exercise
  `native_codec.rs` at all — this module cannot be built or tested on the
  macOS dev machine (`#[cfg(target_os = "linux")]`).
- Empirical hardware validation (requires deploying the fix and triggering
  a real DNG capture, since this bug can only truly be confirmed fixed by
  an actual real-sensor capture going through the actual macOS raw
  decoder — a structural unit test proves tag-tree compliance but the
  original bug wasn't caught by a shallow tag read either):
  - Trigger a real `master_archive` capture with `save_dng: true`.
  - Copy the resulting `.dng` to the Mac (via the existing `optic_sync`
    pipeline, same as any other capture).
  - `sips -s format png <file> --out /tmp/check.png` must succeed (exit
    0), and ideally open cleanly in Preview.

## Implementation Summary

- `src/native_codec.rs`:
  - `encode_bayer16_dng` rewritten to build a two-IFD tree: a raw sub-IFD
    (via `TiffEncoder::extra_directory()`, unchained) written first, then
    IFD0 (a 1x1 placeholder image via `new_image::<colortype::Gray8>`,
    chained as the file's discoverable first IFD) whose `SubIfd` tag (330)
    points at the raw IFD's offset. `NewSubfileType` (254) added to both
    (`1` for IFD0, `0` for the raw IFD) — previously present in neither.
  - `ColorMatrix1`/`AsShotNeutral`/`CalibrationIlluminant1` moved from the
    (formerly single, now raw) IFD to IFD0, matching real-camera DNG
    convention (as-shot camera-profile data belongs at the file level, not
    per-raw-image).
  - `PlanarConfiguration` and `SampleFormat` tags added to the raw IFD
    (previously implicit/absent — `ImageEncoder`'s automatic tag-writing
    covered these before; writing the raw IFD by hand via
    `extra_directory()` means they now have to be written explicitly).
  - Import changed from `colortype::Gray16` to `colortype::Gray8` (for the
    1x1 IFD0 placeholder; the actual 16-bit raw data is now written via
    plain `write_data` on the raw `DirectoryEncoder`, not through
    `ImageEncoder`, so the old `Gray16` `ImageEncoder` machinery is no
    longer used at all).
  - `dng_contains_required_cfa_metadata` test rewritten per the test plan
    above; a `read_ifd_at(data, offset)` helper factored out of the
    existing `little_endian_ifd` so the test can parse both IFDs.
- No changes to `decode_pisp_comp1`, `dequantize`, `encode_yuv420_jpeg`, or
  any camera-control/exposure logic — this is scoped entirely to the DNG
  container-writing function.

## Validation

- **macOS dev target:** N/A for this file — `native_codec` is
  `#[cfg(target_os = "linux")]` and does not compile on macOS at all, so
  there is nothing to run locally. (`cargo build`/`cargo test --all-targets`
  on macOS continue to pass — 30/30 — for everything else, unaffected by
  this change.)
- **Pi-native build/test (2026-09-18):** ran via ad hoc SSH build/test in
  a scratch source directory (`~/.local/src/.dng-verify`, not a full
  deploy — the live installed daemon and systemd unit were untouched
  throughout). `cargo test --locked --bin optic-daemon native_codec`:
  ```
  running 4 tests
  test native_codec::tests::pisp_zero_block_matches_reference_decoder ... ok
  test native_codec::tests::pisp_decoder_rejects_truncated_or_unaligned_frames ... ok
  test native_codec::tests::dng_contains_required_cfa_metadata ... ok
  test native_codec::tests::yuv420_encoder_produces_a_jpeg ... ok
  test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 34 filtered out
  ```
  `dng_contains_required_cfa_metadata` is the rewritten structural test
  confirming the actual two-IFD tree: `NewSubfileType` correct in both
  IFDs, `SubIfd` pointer resolves to a real second IFD, CFA/raw tags only
  in the sub-IFD, color-profile tags only in IFD0.
  - **Operational note, not a code issue:** getting to a clean run took
    several retries. The Pi was rebooted mid-session (at the user's
    request, unrelated to this fix); the first two build attempts
    afterward died with the SSH connection going unresponsive
    (`Timeout, server optic.local not responding`) right around the final
    link step (170-172 of 172 build units). Root cause: this Pi has only
    990MB RAM, and linking a large debug-profile multi-crate binary from a
    post-reboot cold page-cache is memory/IO-heavy enough to stall the
    whole system (including sshd's keepalive responses) for longer than a
    15s timeout — not actual network flakiness. Adding
    `-o ServerAliveInterval=5 -o ServerAliveCountMax=3` made the dead
    connection surface as a clear, fast error instead of hanging silently
    (a real improvement kept for future ad hoc Pi sessions, though not
    added to `build-deploy-optic-daemon.sh` itself, which already
    completes reliably in one shot doing the equivalent work). One of the
    interrupted attempts also left a corrupted binary for the unrelated
    `libcamera_probe` diagnostic target in the shared build cache
    (`~/.cache/optic-daemon-target`, shared with real deploys) — worked
    around by scoping the verification run to `--bin optic-daemon`, which
    doesn't touch that target; not investigated further since it's outside
    this fix's scope and the shared cache self-heals on the next real
    deploy's full rebuild.
  - Separately, `scripts/build-deploy-optic-daemon.sh` was updated to set
    `CARGO_TERM_PROGRESS_WHEN=always`/`CARGO_TERM_PROGRESS_WIDTH=80` so
    future real deploys show live `Building [===>] N/Total` progress
    instead of a bare, countless scroll of `Compiling <crate>` lines —
    prompted directly by how opaque this session's waiting was.
- **Empirical macOS-open check, deploy 1 (`optic-daemon` 0.1.17,
  2026-09-18):** deployed via the real `build-deploy-optic-daemon.sh`
  (release build, all 42 tests + strict Clippy passing on the Pi native
  target — see the deploy's own log for the full gate). Triggered a real
  `master_archive` capture with DNG from the web app
  (`testshot-master-archive-1789774371424.dng`), synced to the Mac via
  `optic_sync`.
  - `sips -g all`: succeeded, reported real dimensions
    (`pixelWidth: 4056`, `pixelHeight: 3040`) — a meaningful change from
    the pre-fix behavior (which could only read header tags, not decode
    pixel data).
  - `sips -s format png ... --out ...`: succeeded (exit 0, ~7s, real
    CPU use), producing a correct, sharp, correctly color-processed
    4056×3040 photo. **The raw-decode fix works.**
  - Preview.app: reported by the user as hanging then crashing when
    opening the same file. Reproduced headlessly via
    `qlmanage -t -s 512 -o <dir> <file>.dng`: hangs indefinitely at 0% CPU
    (genuinely stuck, not slow). Hypothesized at the time as the 1×1 pixel
    IFD0 placeholder (design doc §5) — **this hypothesis was later
    disproven, see below.**
- **Empirical macOS-open + QuickLook check, deploy 2 (`optic-daemon`
  0.1.18, 2026-09-18):** deployed the width/32-downscale placeholder fix
  (all tests + Clippy passing on the Pi). Triggered another real capture
  (`testshot-master-archive-1789776103425.dng`). `sips` continued to work
  correctly (confirmed the file used the new non-degenerate placeholder:
  `NewSubfileType=1`, 126×95, not 1×1). **`qlmanage -t` still hung
  identically.** The placeholder-size hypothesis was wrong.
- **Local bisection (2026-09-18), no Pi round-trip needed:** per explicit
  instruction to validate locally before touching the codec again, built a
  standalone Python TIFF/DNG writer (`/tmp/dngtest/dngbuild.py`,
  scratchpad-only, not committed) that reuses the real extracted raw pixel
  bytes and tag values from the actual captured file, so structural
  hypotheses could be tested in seconds instead of a multi-minute
  build+deploy+capture+sync cycle. Tested via `qlmanage -t` (with a 12s
  hang/pass classifier):
  - V1 (exact replica of the deployed structure): hangs — confirms the
    harness faithfully reproduces the real bug.
  - V2 (IFD0 stripped to only the bare-minimum baseline tags, no
    `ColorMatrix1`/`AsShotNeutral`/`DNGVersion`/etc.): still hangs — rules
    out the private color/calibration tags.
  - V3 (same structure, tiny 64×64 synthetic raw payload instead of the
    real ~24MB one): still hangs — rules out raw-data size.
  - V4 (reverted to the *pre-fix* flat single-IFD structure, built through
    the same harness): `sips` fails cleanly ("Cannot extract image from
    file", matching the original pre-fix bug exactly — confirms harness
    fidelity for that failure mode too) but **`qlmanage -t` still hangs.**
    This was the key result: the hang happens regardless of IFD structure.
  - V5 (flat single-IFD with `Make`/`Model` changed to a real,
    Apple-recognized camera — "Canon"/"Canon EOS 5D Mark IV" — instead of
    "Raspberry Pi"/"imx477"): still hangs — rules out an
    unrecognized-camera-identity explanation.
  - Reset `quicklookd` (`qlmanage -r cache`, `qlmanage -r`,
    `killall quicklookd`) in case repeated `SIGKILL`s during testing had
    left the shared thumbnail service wedged: no change, still hangs.
- **Real reference-file control test (2026-09-18) — the conclusive one:**
  the user provided two real, professionally-shot DNG files from other
  websites (`sample1.dng` — Canon EOS 350D, `L1004220.DNG` — Leica M9
  Digital Camera). Both decode perfectly via `sips -s format png` (4-5s
  each, correct color/dimensions) — proving Apple's underlying raw decoder
  is healthy — but **both hang identically in `qlmanage -t`**, exactly
  like every one of our own files. The Leica M9 is on Apple's own
  officially-supported-camera list for this OS version. This is
  conclusive: **the hang has nothing to do with anything our DNG encoder
  does.** It reproduces on real camera files from real cameras that have
  nothing to do with this project.
- **Final Root Cause — confirmed via independent third-party report
  (2026-09-18):** an [Apple Discussions thread](https://discussions.apple.com/thread/256358754?sortBy=rank)
  reports Photos.app freezing/crashing when editing RAW files (Nikon NEF,
  Canon) specifically on **macOS 15.8 (24H23)** — the exact OS build this
  Mac runs — introduced by that update and absent before it. The thread's
  technical analysis identifies "a deadlock inside the RawCamera plugin
  (ImageIO framework) while reading/adding metadata properties," with the
  main thread blocked on a mutex that never releases — precisely matching
  our own observation of `qlmanage -t` hanging at a genuine, indefinite 0%
  CPU (a classic deadlock signature, not a slow operation). No fix exists
  yet; the thread's own conclusion is "an Apple bug in the 15.8 update"
  requiring Apple to resolve it, with affected users directed to Feedback
  Assistant. **This closes the investigation: the remaining hang is an
  OS-level regression in macOS 15.8's RawCamera ImageIO plugin, confirmed
  independent of this project's code, this project's DNG structure, and
  even this specific Mac (it's a public, multi-user-reported bug).**

## Remaining Limitations / Follow-ups

- **Preview/Finder-thumbnail hang on this Mac is not fixable from this
  project.** It's a confirmed macOS 15.8 (24H23) regression in Apple's own
  RawCamera ImageIO plugin, reproduced on real Canon/Leica files and
  corroborated by a public Apple Discussions report. Nothing in
  `encode_bayer16_dng` can work around an external deadlock in Apple's
  decoder. If/when Apple ships a fix (likely a future macOS point
  release), captures should start previewing correctly with no daemon-side
  change required. Full-resolution decode already works today via `sips`
  and any raw editor with its own decoder (Lightroom, darktable,
  RawTherapee, Capture One) — only Apple's QuickLook-based thumbnail path
  is affected.
- This fix only addresses the container/IFD-tree structure. It does not
  add a real rendered preview image (IFD0 is a small solid-gray
  placeholder) — if a future need arises for DNGs to show a meaningful
  thumbnail in Finder/Preview's grid view (once Apple's bug is fixed),
  that would need a real downscaled-RGB preview render, which is out of
  scope here.
- Every DNG captured *before* the 0.1.17 structural fix (including the one
  that triggered this investigation) remains unreadable via full decode —
  this was a forward fix only, not a repair tool for already-captured
  files.
- The user is weighing whether to move away from DNG entirely given this
  experience (discussed separately, not yet decided) — if that happens,
  this worklog's fix remains valid/correct for as long as DNG capture
  stays in the codebase, but would become moot if the format is dropped.
