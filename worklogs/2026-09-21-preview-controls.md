# Dated Worklog: 2026-09-21 - Preview Downsample Toggle and Stop/Resume

Status: **downsample user-accepted; Stop/Resume Pi-tested** — merged to `main`
in PR #10 (95381e0, 2026-09-21).
Stop/Resume browser behaviour has not been confirmed by the user yet.

## Objective

User requests, on the existing `feat/capture-latency` branch (no new branch or
worktree, per the user):

1. A checkbox under the live preview to downsample the preview image.
2. A "Stop / Resume preview" button beside Capture & transfer.
3. The preview's stopped/resumed state survives a reboot.

Context: the Master Archive preview sends full 4056×3040 JPEGs at Q95 and
2 FPS. Measured 2026-09-21 on `optic.local`: ~2.7 MB per frame, ~40 Mbit/s.
The 4K DCI (1352×720) and 2K Binning (1014×760) previews are already small.

## Acceptance Criteria

- **Downsample checkbox.** When checked, the Master Archive preview is
  1352×1014 (same 4:3 framing) at 8 FPS instead of 4056×3040 at 2 FPS. 4K DCI
  and 2K Binning previews are unchanged: they are already that small, and the
  help text says so. Still captures are unaffected. Toggling it while live
  reconfigures the running preview. The choice persists across page reload,
  daemon restart and reboot.
- **Stop preview.** Stops the live stream and shows a "Preview stopped"
  placeholder. No tab auto-restarts it: the page's auto-start and the 3 s
  status poll respect the flag, and the daemon rejects
  `/api/stream/start` with 409 while stopped, so another open tab can't
  restart it either.
- **Resume preview.** Clears the flag and starts the preview.
- **Capture while stopped.** Capture & transfer still works. The preview
  stays stopped afterwards, and the button comes back without the 3 s
  frozen-still hold, because there's no live view to return to.
- **Persistence.** The state lives in
  `~/.local/state/optic-daemon/preview_state.json` (durable), mirrored to
  the tmpfs cache dir. This is the same `durable_state` write-through
  pattern as `schedule_run_state.json`. Missing or corrupt file → defaults
  (not stopped, not downsampled).
- `/api/status` exposes `preview: { stopped, downsample }`.

## Design

- `src/camera.rs`: `preview_spec(self, downsample: bool)`. Master Archive +
  downsample → `1352×1014 @ 8`. `StreamRequest` gains `#[serde(default)]
  downsample`; the web layer sets it from persisted state, so the daemon is
  authoritative.
- `src/native_camera.rs`: `Pipeline.downsample`. `start_pipeline` takes it.
  A reconfigure whose `downsample` differs restarts the pipeline, the same
  as a profile change.
- `src/web.rs`: `PreviewState { stopped, downsample }`, read from the cache
  on each request, like config. New `POST /api/preview` merges a partial
  body; setting `stopped: true` also stops the stream. `start_stream`
  returns 409 while stopped. `start`/`reconfigure` fill `downsample` from
  the state.
- `src/main.rs`: hydrate `preview_state.json` into the cache at startup and
  pass both paths to `AppState`.
- `src/web/index.html` / `app.js`: checkbox row under the preview frame and
  a toggle button in the button row. `ensurePreview` is a no-op while
  stopped.

## Coordination Note

This touches `src/web.rs`, `src/main.rs`, `src/web/*` and `src/camera.rs`.
Most of these are outside the capture-latency track's owned files. The user
asked for it on this branch, but the merge-conflict risk with the other
tracks (`web.rs`, `main.rs`, dashboard files) is real and should be weighed
at merge time. PR #10 will include these commits.

## Test Plan

Automated (`cargo test --locked`, Biome):

- U1 `preview_spec(false)` is unchanged for all three profiles.
  `preview_spec(true)` gives Master Archive 1352×1014 @ 8 and leaves the
  others unchanged.
- U2 `StreamRequest` without `downsample` deserializes to `false`.
- U3 `PreviewState`: missing file → default. Corrupt JSON → default. A
  partial update (`{"stopped":true}`) keeps `downsample`. The round trip
  through `durable_state` survives re-hydration from the durable file
  (simulated reboot: fresh cache dir).
- Existing suite, clippy `-D warnings`, fmt, and Biome on the dashboard
  files all pass.

On the Pi (requires a full deploy with daemon restart, user approval):

- P1 Master Archive with downsample on: the log shows the preview pipeline at
  `processed=1352x1014`, plus the selected sensor mode. A 5 s stream sample
  shows ~8 fps and bandwidth far below 40 Mbit/s. Off → back to 4056×3040
  @ 2.
- P2 Stop: the stream stops. `/api/status` shows `stopped: true`. A second
  tab/curl `POST /api/stream/start` gets 409. After a page reload it stays
  stopped.
- P3 Capture while stopped: the capture succeeds, the preview stays stopped,
  and no 3 s hold.
- P4 Persistence: restart the daemon (`systemctl --user restart`) → the
  state is kept. A full reboot only with explicit user approval, otherwise
  recorded as covered by the restart plus the U3 simulated-reboot test.
- P5 Resume: the preview starts and the profile/settings are unchanged.
- P6 Regression: capture latency for 2K / Master Archive + DNG stays in the
  PR #10 range.

## Implementation

- `src/camera.rs`: `preview_spec(downsample)` (Master Archive + downsample →
  1352×1014 @ 8); `StreamRequest.downsample` (`#[serde(default)]`); test
  `downsample_only_shrinks_the_master_archive_preview`, and
  `stream_request_uses_selected_profile` asserts the default `false`.
- `src/native_camera.rs`: `Pipeline.downsample`. `start_pipeline(..,
  downsample)`. A reconfigure with a different `downsample` restarts the
  pipeline. Still capture passes `false`, which only matters for preview
  streams.
- `src/web.rs`: `PreviewState { stopped, downsample }` (`#[serde(default)]`)
  and `PreviewStateUpdate` (partial, `deny_unknown_fields`).
  `read_preview_state` returns defaults on a missing or corrupt file.
  `POST /api/preview` holds a mutex, writes through durable then cache, and
  stops the stream when `stopped`. `start_stream` returns 409 "preview is
  stopped; resume it first". Start/reconfigure take `downsample` from the
  state. `/api/status` includes `preview`. Tests:
  `preview_state_defaults_when_missing_or_corrupt`,
  `preview_state_partial_update_keeps_other_fields`,
  `preview_state_survives_a_simulated_reboot`.
- `src/main.rs`: hydrates `preview_state.json` into the cache dir at startup
  and passes both paths to `AppState::new`.
- `src/web/index.html`: "Downsample preview" checkbox under the preview
  frame, "Stop preview" button beside Capture & transfer, and an id on the
  placeholder title.
- `src/web/styles.css`: `.preview-option` spacing (2 lines).
- `src/web/app.js`: `previewStopped`. `applyPreviewState` runs on every
  status poll. `togglePreview`, `setPreviewDownsample`, `stopLivePreview`.
  `ensurePreview` is a no-op while stopped. The status poll tears down a
  local stream and beacons `/api/stream/stop` if the daemon is stopped but
  still streaming (covers a start already in flight). Capture while stopped
  skips the 3 s hold and leaves the preview stopped.

## Validation (macOS)

| Check | Result |
|---|---|
| `cargo fmt --all -- --check` | pass |
| `cargo test --locked` | pass: 116 passed / 0 failed (4 new) |
| `cargo clippy --locked --all-targets -- -D warnings` | pass |
| Biome 2.5.14 on `src/web/app.js`, `src/web/index.html` (the files the deploy script lints) | pass |
| Biome on `src/web/styles.css` | fails, **pre-existing**: the committed `HEAD` version fails the same check. The deploy script does not lint CSS. Not changed here. |
| P1–P6 on the Pi | not run: need a full deploy with daemon restart (user approval) |

Docs: `docs/optic-daemon.md` responsibilities and API list updated
(`POST /api/preview`, `preview` in `/api/status`, the 409 on start while
stopped, and the MEASUREMENT capture card).

## Pi Test Run 1 (2026-09-21, deployed 19:19:26 PDT, user-approved)

Deploy: exit 0, 119 passed on the Pi; deployed `src/web.rs`/`src/camera.rs`
sha256 match the branch; `/api/status` → `"preview":{"stopped":false,"downsample":false}`.
Script: session scratchpad `p.sh` (dashboard API calls), 19:19:37–19:20:34.

| Test | Result |
|---|---|
| P1 downsample on | `processed=1352x1014`, 8.0 fps, 313 KB/frame, **20.0 Mbit/s** |
| P1 downsample off | `processed=4056x3040`, 2.0 fps, 2,495 KB/frame, 39.9 Mbit/s |
| P2 stop | `{"stopped":true}`, `streaming:false`; `POST /api/stream/start` → **409** "preview is stopped; resume it first" |
| P3 capture while stopped | 200 in 2,221 ms; preview stays stopped. The missing 3 s hold is browser-side and not exercised by curl. |
| P4 daemon restart | `systemctl --user restart` → active; `preview_state.json` = `{"stopped": true, …}`; `/api/status` still stopped |
| P5 resume | `{"stopped":false}`, start 200, `streaming:true` |
| P6 latency | 2K 1,050 ms; Master Archive + DNG 2,630 ms (PR #10 range) |

**Finding: the downsampled preview was cropped.** The journal shows libcamera
selected sensor format `1332x990-SBGGR12` for the 1352×1014 downsampled
preview. That IMX477 mode is a binned **centre crop**: the preview framed
tighter than the capture, and was slightly upscaled. The full-size preview
uses `4056x3040`.

Fix: `PreviewSpec.sensor_mode`; the Master Archive downsampled preview forces
`SensorConfiguration` (12-bit, 2028×1520, the full-field 2×2 binned mode) in
`start_pipeline`. The test asserts it. macOS: 116 passed, clippy clean.
Needs redeploy (continuation of the approved deploy/test).

**Pre-existing, not changed (reported to the user):** the 2K Binning preview
(1014×760) also selects `1332x990` (seen in the 2026-09-21 17:45 PDT
journal), so its preview is probably cropped relative to the 2K capture too.

Bandwidth prediction corrected: I estimated 3–5 Mbit/s assuming 2 fps. The
measured 20 Mbit/s reflects 8 fps × ~313 KB at Q95. Lowering preview JPEG
quality is a separate follow-up.

## Pi Test Run 2 (2026-09-21, redeployed 19:24:05 PDT)

Deploy: exit 0, 119 passed on the Pi, "SUCCESS". My post-deploy SSH hash
check hit a transient `connect … port 22` failure (exit 255). Rerun:
deployed `src/native_camera.rs` sha256 `aff7411b…` matches the branch.

P1 again with downsample on: the journal shows `Selected sensor format:
2028x1520-SBGGR12_1X12` (full-field binned, **no longer the 1332×990 crop**),
`processed=1352x1014`, mode range 22,131 µs..; 8.0 fps, 288 KB/frame,
18.5 Mbit/s (vs 39.9 Mbit/s at 2 fps full-size). A `POST /api/stream/start`
right after the reconfigure returned 409 because the stream was already
running (Busy) — expected.

Left as found: downsample off, preview running on Master Archive.

## Changed Files

`src/camera.rs`, `src/native_camera.rs`, `src/web.rs`, `src/main.rs`,
`src/web/app.js`, `src/web/index.html`, `src/web/styles.css`,
`docs/optic-daemon.md`, this worklog. macOS: 116 passed, clippy/fmt clean;
Biome clean on `app.js`/`index.html`.

## Limitations / Next Steps

- Browser-only behaviour is not yet user-checked: the button label flips,
  the checkbox reflects the state, the placeholder reads "Preview stopped"
  after a reload, and there is no 3 s hold after a capture while stopped.
- The downsampled preview is still ~18.5 Mbit/s because Q95 at 8 fps is
  ~290 KB/frame. A lower preview JPEG quality (e.g. 80) would cut it further
  (not done).
- Pre-existing: the 2K Binning preview selects the cropped 1332×990 mode;
  forcing 2028×1520 for it (as done here) is a candidate fix, pending the
  user's decision.
- A real reboot was not performed (user chose restart-only). Persistence is
  covered by the daemon restart (P4) and the simulated-reboot unit test.
- Capture while stopped took 2,221 ms for 2K: frames 1–3 still inherit the
  last preview's frame limit (known PR #10 limitation).
- Not committed; PR #10 would include it once committed and pushed.

## Sharpness Follow-up (2026-09-21)

User report: the downsampled preview (1352×1014 from the 2028×1520 binned
mode) looks less sharp than the full-size preview downscaled by the browser.
The browser draws the preview at 723×542 (user-measured), so both are
downscaled in the browser and upscaling is not the cause. Likely cause
(reasoned, not measured): the binned path has less source detail (on-sensor
2×2 binning, then demosaic at 2028×1520, then a 1.5× ISP resize). The
full-size path supersamples ~5.6× from a full-resolution demosaic.

Change: the downsampled Master Archive preview now forces the 4056×3040
sensor mode (`sensor_mode: Some((4056, 3040))`) and lets the ISP scale to
1352×1014 at 8 fps. The full mode's minimum frame duration is 85,335 µs, so
8 fps (125 ms) fits. The unit test is updated. macOS: 116 passed, clippy
clean. Pending: redeploy (approval), then check the journal's selected
sensor format, fps and bandwidth, and the user's visual sharpness
comparison.

Deployed (user-approved): exit 0, 119 passed on the Pi, "SUCCESS";
deployed `src/camera.rs` sha256 `33d94368…` matches the branch. With the
user's downsample on, the journal shows `Selected sensor format:
4056x3040-SBGGR12_1X12` → `processed=1352x1014` (mode range 85,335 µs..).
A 5 s sample gave 7.8 fps, 255 KB/frame, 15.9 Mbit/s. Pending: the user's
visual sharpness comparison.

## User Acceptance: Downsample (2026-09-21)

User: the full-size and downsampled previews now look the same ("can't tell
diff"), and the downsampled preview renders much more smoothly.
`docs/optic-daemon.md` now describes the full-resolution readout with ISP
scaling and the measured 255 KB/frame, ~16 Mbit/s.
