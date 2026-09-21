# Design Note: Mac-Side Timelapse Builder (`optic-timelapse`)

> **Status:** User-accepted 2026-09-21 and merged to `main` (PR #5).
> Implemented and tested on the iMac (unit and integration tests, plus a
> real end-to-end encode of the 2026-09-20 `every1min` captures). The
> validation record is in `worklogs/2026-09-20-timelapse-builder.md`.

## 1. Purpose

`optic_sync` transfers every capture to the iMac at
`/Users/admin/Pictures/Optic` as flat files: JPG, optional DNG, and a
`.log.json` sidecar that share a basename. `optic-timelapse` is a manual,
Mac-only CLI that turns those captures into timelapse videos. It groups them
**per rule slug** and **per local day** (or per date range), then encodes
each group with `ffmpeg`.

It is not part of the daemon. It lives in its own crate at `tools/timelapse/`
with its own `Cargo.toml` and `Cargo.lock`, so it doesn't touch the daemon's
dependency graph or build. It never runs on the Pi.

## 2. Decisions (confirmed with the user 2026-09-20)

| Topic | Decision |
|---|---|
| Form | Separate crate `tools/timelapse/`, binary `optic-timelapse` |
| Encoder | `ffmpeg` from Homebrew. Default `libx265` CRF 20 (HEVC, `.mp4`, `hvc1` tag). Revised from `hevc_videotoolbox`; see below |
| Frame rate / size | 24 fps. Scaled to 3840 px wide with aspect ratio kept and height rounded to even (master-archive gives 3840×2878). Never upscaled |
| Deflicker | On by default (`deflicker=mode=pm:size=5`); `--no-deflicker` turns it off |
| Gaps | Frames play back to back at a constant fps. Gaps are reported. Long gaps split a group into separate segments |
| Input | JPG only. Sidecars are used for metadata. DNGs are ignored |
| Trigger | Manual CLI only. No launchd job |
| Output | `~/Movies/Optic Timelapses/` by default (`--out` overrides) |

**Encoder revision (2026-09-20).** This iMac Pro (Intel Xeon W, T2) can't open
a VideoToolbox *hardware* HEVC or H.264 session above 3840×2160. At
3840×2878 or 4056×3040 it fails with `Cannot create compression session:
-12903`, and it only works with `-allow_sw 1` (Apple's software path). For
72 synthetic 4056×3040 frames:

| Option | Time |
|---|---|
| VT with `-allow_sw 1` at 3840 wide | 39.3 s |
| VT hardware at 2880×2160 | 2.0 s |
| `libx265` CRF 20 medium at 3840 wide | 8.1 s |
| `libx264` CRF 18 at 3840 wide | 4.2 s |

The user chose `libx265` at full 3840 width as the default. `--codec hevc-vt`
remains available and fits the output inside a 3840×2160 box, so
master-archive gives 2882×2160.

## 3. Safety: The Capture Folder Is Read-Only

- The tool only lists the source directory (non-recursive, so the receiver's
  `.incoming/` staging dir is never read), reads `.log.json` files, and passes
  JPG paths to ffmpeg as inputs.
- It never creates, renames, moves, or deletes anything in the source
  directory. Frame lists for ffmpeg are built as symlinks in a private temp
  directory (`$TMPDIR/optic-timelapse-<pid>-<n>/`), which is removed
  afterwards.
- It refuses to run if the output directory is the source directory or inside
  it.
- Output is written to `<name>.partial.mp4` and renamed only after ffmpeg
  succeeds. An existing output file is never overwritten unless `--overwrite`
  is passed.

## 4. Filename Parsing

The filename families come from `docs/optic-daemon-scheduler.md` §3.1 and
`docs/optic-daemon-capture-log.md` §3.1:

```
testshot-<profile>-<ms>.{jpg,dng,log.json}          manual (web UI)
testshot-<profile>-failed-<ms>.log.json             manual, failed
scheduler-<profile>[-<tags>]-<ms>.{jpg,dng,log.json} scheduler
scheduler-<profile>-failed-<ms>.log.json            scheduler, failed
```

- `<profile>` is one of the fixed slugs `master-archive`, `4k-dci`, and
  `2k-binning`. It is matched from that fixed set, so its internal hyphens
  are never ambiguous.
- `<ms>` is the unix-millisecond suffix, and it's the frame's sort key and
  timestamp. It is within about 10 ms of the sidecar's
  `completed_at_unix_ms`.
- `<tags>` is the rule-slug list: sorted, joined with `-`, capped at 3, with
  `+N` for any overflow (`src/camera.rs::format_rule_tags`). **Slugs can
  contain hyphens, so the filename can't be split into tags reliably.** Rule
  membership is resolved in this order:
  1. The sidecar's `triggered_by` array, when present and non-empty. This is
     authoritative.
  2. Otherwise the filename tag string (with any `+N` removed) is split into
     slugs that are *known* from other sidecars in the same scan, if it
     splits exactly. For example, `dawn-golden-hour` becomes `[dawn,
     golden-hour]` when both are known slugs.
  3. Otherwise the whole tag string is treated as one slug.

  Captures made before `triggered_by` existed (the first `every1min` frame
  on 2026-09-20) resolve through step 2 or 3.
- Unrecognized names are ignored. So are non-JPG files, as sources
  (`.dng`, `config.json`, `preview_config.json`, `.DS_Store`).

## 5. Frame Selection and Grouping

1. Scan the source directory. Keep scheduler JPGs by default; `--include-manual`
   also groups `testshot-` JPGs under the pseudo-rule `manual`.
2. Drop a frame if its sidecar says `success: false`, or if the sidecar's
   `width`/`height` disagree with the majority for its group. This keeps
   every group at one resolution. A frame with no sidecar is kept and
   counted in the summary.
3. Filter by `--rule` (repeatable; a frame matches if any of its slugs
   matches), `--profile`, and the date window (`--date D`, or `--from D`
   and/or `--to D`, inclusive, in Mac local time).
4. Assign each frame to one group per rule slug it belongs to. A merged
   multi-rule shot appears in each rule's video. The group key is
   `(rule, profile, period)`, where period is the local calendar date
   (`--group-by day`, the default) or the whole window (`--group-by range`).
5. Sort each group by `<ms>` and split it into **segments** wherever
   consecutive frames are more than `--split-gap` apart. The default is `1h`
   in day mode and `off` in range mode, so a month of dawn-only captures
   becomes one video rather than 30. `--split-gap off` disables splitting.
6. Report gaps within each segment that are larger than 1.5× the segment's
   median interval (missed shots). They are reported only, not filled.
7. Skip segments with fewer than `--min-frames` frames (default 10) and
   report them.

## 6. Encoding

One ffmpeg run per segment (default codec shown):

```
ffmpeg -hide_banner -nostdin -loglevel error -stats -y \
  -framerate 24 -start_number 0 -i <tmp>/%06d.jpg \
  -vf scale=3840:2878:flags=lanczos:out_range=tv:out_color_matrix=bt709,setsar=1,\
setparams=range=tv:color_primaries=bt709:color_trc=bt709:colorspace=bt709,\
deflicker=mode=pm:size=5,format=yuv420p \
  -an -c:v libx265 -crf 20 -preset medium -x265-params log-level=error \
  -tag:v hvc1 -movflags +faststart <out>/<name>.partial.mp4
```

The output size is computed by the tool rather than by ffmpeg's `-2`, so the
dry run shows the exact dimensions. Deflicker runs after scaling, which is
cheaper and gives the same luminance statistics. `-y` only ever applies to
the tool's own `.partial.mp4`.

**Colour and pixel aspect.** The JPEGs are full-range BT.601. The first real
encode showed that ffmpeg 9 passes that straight through (`yuvj420p`,
`color_range=pc`, `bt470bg`, primaries and transfer unknown), even with
`format=yuv420p` and output-level `-color_primaries`/`-color_trc` flags. It
also showed that `scale` sets a 0.99996 sample aspect ratio to keep
4056:3040 exact after rounding the height to even, which AVFoundation
reports as a 3839.86 px wide frame. The chain therefore converts to limited
range and the BT.709 matrix inside `scale`, forces square pixels with
`setsar=1`, and tags primaries and transfer on the frames with `setparams`.
Output is `yuv420p`, `tv`, `bt709`/`bt709`/`bt709`, SAR 1:1.

- `--codec hevc` (default: `libx265 -crf 20 -preset medium`), `hevc-vt`
  (`hevc_videotoolbox -b:v <bitrate>`, fit inside 3840×2160), or `h264`
  (`libx264 -crf 18 -preset medium`). If the chosen encoder isn't listed by
  `ffmpeg -encoders`, the tool exits with an error. It never falls back
  silently.
- `--fps`, `--width` (clamped to the source width; the height is scaled
  proportionally and rounded to the nearest even number), `--bitrate`
  (VideoToolbox only, default `40M`), and `--no-deflicker`.
- Output name: `<rule>_<profile>_<YYYYMMDD-HHMM>_<YYYYMMDD-HHMM>.mp4`, using
  the segment's first and last frame times in local time.
- `--dry-run` prints each segment's frame list (index, local time, file,
  gap markers) and the exact ffmpeg command, then exits without creating
  anything.

## 7. CLI

```
optic-timelapse [--src DIR] [--out DIR] [--rule SLUG]... [--profile SLUG]
                [--date YYYY-MM-DD | --from YYYY-MM-DD --to YYYY-MM-DD]
                [--group-by day|range] [--split-gap DUR|off] [--min-frames N]
                [--fps N] [--width PX] [--codec hevc|hevc-vt|h264]
                [--bitrate RATE] [--no-deflicker] [--include-manual]
                [--overwrite] [--ffmpeg PATH] [--dry-run]
```

Build, test, and run:

```bash
cd tools/timelapse
cargo test                      # unit + integration (encode test needs ffmpeg)
cargo build --release
./target/release/optic-timelapse --date 2026-09-20 --rule every1min --dry-run
./target/release/optic-timelapse --date 2026-09-20 --rule every1min
```

Exit status: 0 on success, including "nothing to encode". 1 if any segment
failed, for example because its output already exists without
`--overwrite`; the other segments still run. 2 for argument, source, output
directory, or ffmpeg-availability errors, before anything is written.

## 7a. Code Layout and Tests

- `src/names.rs`: filename parsing (pure).
- `src/plan.rs`: rule resolution, grouping, segmentation, and gaps (pure,
  timezone passed in).
- `src/encode.rs`: output size and name, the filter chain, ffmpeg args,
  and the output-dir check (pure).
- `src/cli.rs`: argument parsing (pure; the home directory is passed in).
- `src/main.rs`: the I/O shell (directory scan, sidecar reads, temp symlink
  dir, ffmpeg process, atomic rename).
- `tests/cli_e2e.rs`: runs the real binary on generated 64×48 frames in a
  temp dir. It checks encoded frame order by mean luma, that the source
  directory is untouched, refusal of `--out` inside the source, the error
  for a missing ffmpeg, and that dry runs write nothing. The encode test is
  skipped, with a message, if ffmpeg isn't on PATH, unless
  `OPTIC_REQUIRE_FFMPEG` is set.

**CI:** the `Timelapse tool (macOS)` job in `.github/workflows/ci.yml`
runs fmt, clippy, the tests with Homebrew ffmpeg and
`OPTIC_REQUIRE_FFMPEG=1`, and a release build on `macos-15`. It runs only
when `tools/timelapse/**` or the CI definition changes (see
`docs/optic-daemon-ci-cd.md` §5.1).

## 8. Known Limitations

- Times use the Mac's local timezone, not the station timezone in the
  daemon config. They match today (both US Pacific).
- Frame spacing isn't preserved: missed shots shorten the video instead of
  holding frames.
- Deflicker smooths auto-exposure jitter but can't fix large exposure jumps.
  A per-frame exposure lock on the Pi side would be the real fix.
- **A still-growing segment gets a new file on each run.** The name encodes
  the last frame's time, so re-running while the day's captures are still
  arriving writes a new file next to the old one instead of replacing it
  (`--overwrite` only replaces an identical name). Build closed days, or
  clean up by hand.
- The BT.709 primaries and transfer tags are nominal. The JPEGs carry no
  colour profile, and sRGB primaries match BT.709.
- DNG input is not supported (see
  `docs/optic-daemon-dng-compatibility.md` §5 for the macOS 15.8 ImageIO
  deadlock).
