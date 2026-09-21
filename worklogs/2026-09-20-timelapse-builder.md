# Dated Worklog: 2026-09-20 - Mac-Side Timelapse Builder

Status: **implemented and tested on the iMac** (unit, integration, and a real
end-to-end encode). Committed, pushed, and opened as PR #5 (rebased onto
`main` on 2026-09-21). Awaiting user acceptance. The CI job for the tool is
in `worklogs/2026-09-21-ci-timelapse-macos.md`.

## Objective

Turn transferred captures in `/Users/admin/Pictures/Optic` into timelapse
videos. Scheduler filenames carry the profile and rule slugs
(`scheduler-<profile>[-<rule-tags>]-<millis>.{jpg,dng}`, see
`docs/optic-daemon-scheduler.md` §3.1), and each capture has a synced
`.log.json` with settings, so sequences can be grouped per rule and per
day or range.

## Data Survey (2026-09-20, read-only)

Surveyed `/Users/admin/Pictures/Optic` with `ls`, `find`, `sips -g`, and a
read-only Python pass over filenames and sidecars. Nothing was written there.

- **Volume (at survey time; the folder is live and grows about 2 files a
  minute while `every1min` runs):** 152 files, 395 MB. 72 `.jpg`, 3 `.dng`, 74 `.log.json`, plus
  `config.json`, `preview_config.json`, `.DS_Store`, and an empty
  `.incoming/` staging directory (the receiver's temp dir, which must be
  ignored).
- **Filename families:**
  - `scheduler-master-archive-every1min-<ms>.{jpg,log.json}`: 67 frames, one
    with a `.dng`. This is the only timelapse sequence on disk.
  - `testshot-{master-archive,4k-dci,2k-binning}-<ms>.*`: 5 manual web-UI
    shots, plus 2 `testshot-2k-binning-failed-<ms>.log.json` (failed
    captures that have a sidecar but no image).
  - Only `master-archive` appears in scheduler output so far. All three
    profile slugs (`master-archive`, `4k-dci`, `2k-binning`) appear overall.
- **Tag ambiguity:** rule slugs may contain hyphens, and multiple tags are
  also joined with `-` (`src/camera.rs::format_rule_tags`). So
  `scheduler-master-archive-dawn-golden-hour-<ms>` could mean one slug or
  several. The filename alone can't split tags reliably. The sidecar's
  `triggered_by` array is authoritative when present. The profile slug can be
  parsed unambiguously because the three known slugs form a fixed set.
- **Sidecars:** 66 of 67 scheduler sidecars have `triggered_by:
  ["every1min"]`. The first one (`...-1789894386246`, 01:53 PDT) was written
  by an older daemon and has no `triggered_by` key. Its filename still has
  the tag. All scheduler frames use `exposure: normal`, `awb: auto`, and
  `shutter_us: 0` (auto exposure and auto white balance), so frame-to-frame
  brightness flicker is expected. Each sidecar has `success`, `files[]`,
  `width`/`height`, `requested_at_unix_ms`, and `completed_at_unix_ms`. The
  filename `<ms>` is within about 10 ms of `completed_at_unix_ms`.
- **Cadence and gaps (every1min):** all 67 frames fall on 2026-09-20
  (local PDT). One isolated frame at 01:53:06, then a 17.5 h gap, then
  19:22:06–20:34:05 (66 frames). *Corrected after implementation: the survey
  first said 19:24:05, which is actually the frame before the first gap. The
  dry run's frame list shows 19:22:06.* Intervals are mostly 59–61 s (range 57–64 s: capture
  jitter of about 1.8–9 s past the minute). Four gaps inside the evening run
  are 119 s, 176 s, 178 s, and 179 s, meaning 1–2 missed shots each.
- **Images:** master-archive JPGs are 4056×3040 with no EXIF orientation;
  4k-dci is 4056×2160. Mixed profiles mean mixed resolutions and aspect
  ratios, so a sequence must be homogeneous per profile.
- **DNGs:** only 3 on disk, so DNG input isn't viable for the existing
  sequence. Per `docs/optic-daemon-dng-compatibility.md` §5, macOS 15.8
  (24H23, this Mac's exact build) deadlocks QuickLook/ImageIO thumbnails for
  any DNG. `sips` full decode works but takes about 7 s per frame.
- **Host tooling:** macOS 15.8 24H23, Intel Xeon W-2150B (iMac Pro, T2).
  **`ffmpeg`/`ffprobe` are not installed.** Homebrew is present and offers
  `ffmpeg` 9.0.2 (no bottle listed, so it may build from source). `avconvert`,
  `swift`/`swiftc`, `sips`, and `cargo 1.98.1` are available. The ffmpeg
  encoder list (e.g. `hevc_videotoolbox`) can't be checked until ffmpeg is
  installed. 581 GiB free on the data volume.
- **Existing crate deps:** `chrono`, `serde`, and `serde_json` are already in
  `Cargo.toml`. A `src/bin` target could parse sidecars and handle dates with
  **no new dependencies**.

## Decisions (answered by the user 2026-09-20)

1. **Tool form:** a separate crate at `tools/timelapse/` (binary
   `optic-timelapse`) with its own `Cargo.toml`/`Cargo.lock`. The daemon's
   `Cargo.toml` and `Cargo.lock` are **not touched**.
2. **Output:** first chosen as `hevc_videotoolbox`, `.mp4`, `hvc1` tag,
   24 fps, 3840 px wide, aspect ratio kept (never upscaled). **Revised
   (user's choice, second question round):** the VT hardware encoder on this
   iMac Pro can't open a session above 3840×2160 (`-12903`; see Findings), so
   the default is `libx265` CRF 20 at 3840 wide (3840×2878 for
   master-archive). `--codec hevc-vt` (fit inside 3840×2160) and `h264` are
   selectable, with no silent fallback.
3. **Processing:** deflicker on by default (`--no-deflicker`). Gaps are
   reported, and long gaps (`--split-gap`, default 1h in day mode) split a
   group into segments. JPG input only.
4. **Trigger:** manual CLI only.
5. **Output folder:** `~/Movies/Optic Timelapses/`.
6. **ffmpeg:** the user approved `brew install ffmpeg` (a system-level change
   on the iMac).

Design: `docs/timelapse-builder.md`.

## Findings Before Implementation

- `brew install ffmpeg` installed ffmpeg 9.0.2 (built in 3 min 50 s). It
  has the `libx264`, `libx265`, `h264_videotoolbox`, `hevc_videotoolbox`, and
  `prores_videotoolbox` encoders and the `deflicker` filter.
- VT hardware limit (synthetic `testsrc2`, 1 s, 24 fps, `-b:v 40M`): both
  `hevc_videotoolbox` and `h264_videotoolbox` succeed at 1920×1440 and
  3840×2160. Both fail at 3840×2880 and 4056×3040 with `Cannot create
  compression session: -12903`, and succeed there only with `-allow_sw 1`.
- Timing for 72 frames at 4056×3040 scaled to 3840 wide: VT with
  `allow_sw` took 39.3 s, VT hardware at 2880 wide took 2.0 s, `libx265`
  CRF 20 medium took 8.1 s, and `libx264` CRF 18 medium took 4.2 s.
- zsh gotcha seen during probing: `$sz:r` is a zsh history modifier, so
  `testsrc2=s=$sz:r=24` loses `:r`. Use `${sz}`, or run probes under
  `bash -c`.
- `CLAUDE.md` imported `@AGENTS.md`, but no `AGENTS.md` existed in this
  worktree at the time. *Resolved 2026-09-21:* after rebasing onto `main`,
  `AGENTS.md` is a symlink to `CLAUDE.md`, and the zsh `$sz:r` rule was
  appended to its Environment and Tooling Constraints.
- `CLAUDE.md`'s repository-structure list has no `tools/` entry. Adding one is
  a one-line shared-file edit, left for merge time.

## Files This Track Owns

`tools/timelapse/**` (new crate), `docs/timelapse-builder.md`, and this
worklog. No daemon source, root `Cargo.toml`/`Cargo.lock`, or `src/web/`
changes.

Dependencies of the new crate (its own lockfile, no conflict with other
tracks): `serde` (derive), `serde_json`, `chrono` (clock). No CLI-parsing
crate; arguments are parsed by hand.

## Test Plan (written before implementation)

### Unit tests (`cargo test` in `tools/timelapse/`, synthetic names only)

Filename parsing (`parse_capture_filename`):
- `scheduler-master-archive-every1min-1789894386246.jpg` gives Scheduler,
  `master-archive`, tags `every1min`, ms 1789894386246, Jpg.
- Every profile slug parses, including hyphenated `4k-dci` and
  `2k-binning`.
- `scheduler-<p>-<ms>.jpg` (no tags) gives tags None.
- A multi-hyphen tag string (`dawn-golden-hour`) is kept whole;
  `a-b-c+2` gives tags `a-b-c` with overflow 2.
- `testshot-<p>-<ms>.jpg` gives Manual; `testshot-<p>-failed-<ms>.log.json`
  gives a failed log.
- `.dng` and `.log.json` kinds are recognized.
- Rejected: unknown prefix, unknown profile, non-numeric ms, missing ms,
  `config.json`, `.DS_Store`, uppercase extension, empty tags segment.

Tag resolution (`resolve_rules`):
- A sidecar `triggered_by` wins over the filename.
- An exact split into known slugs (`dawn-golden-hour` with known
  `{dawn, golden-hour}`) gives two rules.
- An unknown string gives one whole slug. No tags on a scheduler frame gives
  `untagged`. Manual frames give `manual`.

Grouping and segmentation (`plan_groups`):
- Frames are sorted by ms regardless of input order.
- A 2-rule frame lands in both rules' groups.
- Day mode splits at local midnight. Range mode keeps one group across days.
- The `--split-gap` boundary: a gap equal to the limit doesn't split, one
  just over does, and `off` never splits.
- Gap report: a 179 s gap in a 60 s series is flagged, a 64 s gap is not.
- Min-frames skip.
- Filters: rule, profile, and inclusive date window.
- Failed sidecar or mismatched resolution drops the frame.

Planning and safety:
- An output name follows the documented format.
- ffmpeg args for each codec, deflicker on/off, width clamped to the source
  width.
- An output dir equal to or inside the source dir is rejected; a sibling is
  accepted.
- Duration and date argument parsing, including errors.

### End-to-end (real data, this iMac)

- Environment: iMac Pro (macOS 15.8), Homebrew ffmpeg. Source
  `/Users/admin/Pictures/Optic` (read-only). Output
  `~/Movies/Optic Timelapses/`.
- Before and after, snapshot the source listing (`ls -la` plus a `shasum` of
  the listing) to prove nothing in it changed.
- `--date 2026-09-20 --rule every1min --dry-run`: expect one segment of 66
  frames (19:22–20:34), with the 01:53 single frame split off and skipped as
  below min-frames. Also expect the 4 gaps (119/176/178/179 s) reported.
- Real encode of the same selection (default `libx265`). Check:
  - `ffprobe -count_frames` gives nb_read_frames = 66;
  - duration is 66/24 = 2.75 s;
  - resolution is 3840×2878 and the codec is hevc/hvc1;
  - a full decode with `ffmpeg -v error -f null -` shows no errors;
  - macOS/AVFoundation can read it (`avconvert` or `mdls` duration, plus a
    QuickLook thumbnail);
  - frame order: with `--no-deflicker`, decoded frame k matches source frame
    k better (PSNR) than frames k±1 at several k.
- Required environment: the Mac only. No Pi time.
- Failure cases to exercise: a missing ffmpeg path gives a clear error,
  `--out` inside the source dir is refused, and an existing output without
  `--overwrite` is refused.

## Implementation Summary

A new standalone crate at `tools/timelapse/` (binary `optic-timelapse`):

- `Cargo.toml`: its own `[workspace]`, so it isn't part of the daemon
  package. Dependencies are `chrono` 0.4 (`clock`, `std`), `serde` 1
  (`derive`), and `serde_json` 1. `Cargo.lock` is new, 44 packages, locked
  by cargo 1.98.1. **The root `Cargo.toml`/`Cargo.lock` are unchanged.**
- `.gitignore`: `/target/`. The root `.gitignore` only covers `/target/`
  at the repo root.
- `src/names.rs`: filename parser.
- `src/plan.rs`: sidecar model, rule resolution (sidecar, then known-slug
  split, then whole string), grouping by (rule, profile, day), gap
  splitting, gap report, and min-frames.
- `src/encode.rs`: output size (never upscales; VT box), output name,
  filter chain, ffmpeg args, command display, and the output-dir check.
- `src/cli.rs`: argument parser and usage.
- `src/main.rs`: I/O. Non-recursive scan, sidecar reads, ffmpeg encoder
  probe, a temp symlink dir per segment (removed on drop), `.partial.mp4`,
  then rename.
- `tests/cli_e2e.rs`: integration tests on generated 64×48 frames.

Docs: new `docs/timelapse-builder.md`. No other docs needed changes: the
daemon docs describe the file formats this tool reads, and those are
unchanged.

## Validation

Environment: iMac Pro, macOS 15.8 (24H23), Intel Xeon W-2150B, cargo
1.98.1, Homebrew ffmpeg 9.0.2. No Pi involvement.

### Automated checks (in `tools/timelapse/`)

| Command | Result |
|---|---|
| `cargo fmt --check` (after `cargo fmt`) | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo test` | **45 passed** (41 unit, 4 integration), 0 failed, 0 skipped (ffmpeg present) |

Integration tests: encoded frame order by mean luma, source dir unchanged,
`--out` inside the source refused with exit 2, a missing ffmpeg gives exit 2
with no output dir created, and a dry run writes nothing.

**Mutation checks** (temporary edits, reverted, and 45/45 re-run
afterwards):
- Reversing the symlink numbering in `main.rs` failed the order test with
  `frames out of order: [215, 198, …, 38]`.
- Reversing the group sort in `plan.rs` failed 3 integration tests (it
  panics on the gap subtraction).

### Real-data end-to-end (source `/Users/admin/Pictures/Optic`)

1. Before: snapshot of `ls -laT` plus `stat` of every file (199 files at
   that point).
2. `optic-timelapse --date 2026-09-20 --rule every1min --dry-run` (exit 0)
   found 91 matched frames, 0 without a sidecar, and 0 unreadable sidecars.
   - The 01:53:06 single frame was skipped (below min-frames 10).
   - Segment 1: 19:22:06–20:34:05, 66 frames, median 59.9 s, with gaps of
     179/178/176/119 s (about 2/2/2/1 missed). This matches the survey
     exactly.
   - Segment 2: 22:19:48 onward, which is live and still growing.
   - Planned output: 4056×3040 → 3840×2878 at 24 fps = 2.75 s, plus the
     exact ffmpeg command.
3. First real encode (exit 0, 23.9 s wall for 90 frames) wrote both
   segments. ffprobe showed 66 and 24 frames, durations 2.75 s and 1.00 s,
   HEVC Main `hvc1`, 3840×2878, and a clean full decode. **But it also
   showed `yuvj420p` / `color_range=pc` / `bt470bg` / primaries and transfer
   unknown.** → Defect 1.
4. After fixing range and matrix, AVFoundation (a Swift `AVURLAsset` check)
   reported `naturalSize` 3839.86×2878, and ffprobe still showed transfer
   and primaries unknown. → Defects 2 and 3.
5. Final encode with all fixes (`--overwrite`, exit 0, 25.2 s wall):

   | Output | Frames | Duration | Stream | AVFoundation |
   |---|---|---|---|---|
   | `every1min_master-archive_20260920-1922_20260920-2034.mp4` (4.2 MB) | 66 (expected 66) | 2.750 s (66/24) | hevc `hvc1` 3840×2878, SAR 1:1, yuv420p, tv, bt709/bt709/bt709, 24/1 | playable, decodable, 3840×2878, 2.75 s, frame at 1 s extracted |
   | `every1min_master-archive_20260920-2219_20260920-2247.mp4` (1.9 MB) | 28 (expected 28 at run time) | 1.167 s (28/24) | same | playable, decodable, 3840×2878, 1.167 s |

   `ffmpeg -v error -i <f> -f null -` gave 0 error lines for both.
   `qlmanage -t` made thumbnails for the first-run outputs.
6. Order on real frames (`--no-deflicker` to a scratch dir): decoded
   frames 0, 1, 2, 3, 20, 40, 55, 62, 63, and 65 each match their own source
   frame best by PSNR (about 34.1 dB vs 18–33 dB for neighbours). Frame 64
   was inconclusive: src[64] scored 34.15 dB and src[65] 34.44 dB, but those
   two night frames are 44.4 dB alike, so PSNR can't tell them apart. The
   deterministic order proof is the synthetic integration test above.
7. Existing-output refusal: re-running without `--overwrite` printed
   `… already exists; pass --overwrite to replace it` and exited **1**.
8. After: every one of the 199 files from the "before" snapshot is present
   with the same size and mtime. The only additions are 8
   `scheduler-master-archive-every1min-<ms>.{jpg,log.json}` pairs delivered
   by sync during the session. `$TMPDIR` has no leftover
   `optic-timelapse-*` dirs.

### Failures found and fixed

1. **Full-range BT.601 output.** ffmpeg 9 kept the JPEG's full range and 601
   matrix despite `format=yuv420p`. Fixed with
   `scale=…:out_range=tv:out_color_matrix=bt709`.
2. **Non-square pixels.** `scale` set SAR 0.99996 to keep 4056:3040 exact
   at 2878 px high, and AVFoundation showed 3839.86 px. Fixed with
   `setsar=1`.
3. **Output-level colour flags ignored.** `-color_primaries`/`-color_trc`
   on the output didn't reach the stream. Removed and replaced with
   `setparams=…` in the filter chain.

   The unit tests `ffmpeg_args_*` were updated to pin the new chain.
4. zsh expanded `$sz:r` as a modifier during encoder probing (a probing
   error, not a product defect). See Findings.

### Leftover output files from test runs (deleted with user approval)

The 4 old files from intermediate runs were deleted from
`~/Movies/Optic Timelapses/` after the user approved it (2026-09-21, `rm -v`
on each named file):

- `…_20260920-0153_20260920-2245.mp4`: an accidental `--split-gap off` run.
- `…_20260920-2219_20260920-2243.mp4`: made before the colour fix.
- `…_20260920-2219_20260920-2244.mp4` and
  `…_20260920-2219_20260920-2246.mp4`: earlier versions of segment 2.

The folder now holds only the two final outputs listed above.

## Limitations and Follow-Up

- A segment that is still growing gets a new file on each run, because the
  name encodes the last frame's time. Possible follow-up: a flag to skip a
  segment whose last frame is within `--split-gap` of now.
- Local timezone only; missed shots are not time-filled; deflicker can't fix
  large auto-exposure jumps (see design doc §8).
- *Resolved 2026-09-21:* `CLAUDE.md` now lists `tools/` in its repository
  structure, and `AGENTS.md` exists on `main`.
- Only `master-archive` scheduler data exists, so `4k-dci`/`2k-binning`
  real encodes are untested. Those paths are covered by unit tests, and the
  integration test uses the `2k-binning` naming.

## User Verification Steps

1. Play `~/Movies/Optic Timelapses/every1min_master-archive_20260920-1922_20260920-2034.mp4`
   in QuickTime. Expect 2.75 s of dusk at the station with no colour shift
   or stretched geometry, and deflicker smoothing the auto-exposure.
2. Try a dry run:
   `cd tools/timelapse && cargo run --release -- --date 2026-09-20 --dry-run`.

## Parallel-Session Coordination (applies to all four tracks)

This track runs in its own git worktree alongside three others
(`feat/capture-latency`, `feat/health-alerts`, `feat/provisioning`,
`feat/timelapse-builder`), all branched from `main` at `a6e1509`.

- **Do not bump `Cargo.toml` `version` on this branch.** The bump happens
  once, at merge time into `main`, immediately before deploy.
- **The Pi and camera are a single shared resource.** Ask the user before any
  deploy, service restart, reboot, or on-hardware test, and do not assume
  another session is not using it.
- **Stay inside the files this track owns (below).** If a change outside them
  becomes necessary, record why here and tell the user; it is a merge-conflict
  risk with the other tracks.
- Do not commit or push without explicit user approval (`CLAUDE.md`).
- Merge order is decided by the user; after each merge the other branches
  rebase onto `main`.
