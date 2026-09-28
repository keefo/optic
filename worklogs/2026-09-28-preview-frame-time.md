# Dated Worklog: 2026-09-28 - Show the Pi-Side Capture Time on the Live Preview

Status: **implemented and locally tested**; Pi deploy and user acceptance pending.

## Objective

User request: show, in the top-right of the live preview, the time the Pi
took the frame being displayed, so it is obvious how fresh the image is.
Preview only — scheduled captures must not be overlaid or otherwise changed.

## Design

- `PreviewFrame` gains `captured_at: Option<SystemTime>`, stamped in
  `native_camera.rs` at the moment the frame is encoded and published.
- `mjpeg_part` sends it as `X-Optic-Captured-At`, epoch milliseconds, beside
  the existing per-frame `X-Optic-*` headers. `unavailable` when absent, like
  the other optional headers.
- `app.js` reads the header on each painted frame and renders it into a new
  `#frame-time` overlay, formatted with `Intl.DateTimeFormat` in the
  **station timezone** from `/api/status`
  (`config.schedule.station.timezone`, e.g. `America/Vancouver`), not the
  viewer's. A phone in another timezone, or with a clock that differs from
  the Pi's, still shows the Pi's time.
- The label hides when the preview stops and when a frame carries no
  timestamp.

Captures are untouched: no pixels are drawn into any frame, and the overlay
is a DOM element over the preview canvas, so scheduled shots and their
EXIF/sidecar data are unaffected.

## Acceptance Criteria

1. The live preview shows the frame's capture time, top-right, updating with
   the stream.
2. The time is the Pi's, in the station timezone, regardless of the viewing
   device's clock or zone.
3. Scheduled captures are unchanged: no overlay, no new file content.
4. The label disappears when the preview is stopped.
5. It never blocks the loupe or the focus-tool overlays (`pointer-events: none`).

## Test Plan (before implementation)

1. `cargo test --locked`: `mjpeg_part` emits `X-Optic-Captured-At` with epoch
   milliseconds, and `unavailable` when the frame has no timestamp; asset
   tests pin `#frame-time`, the header name, `stationTimezone` and the render
   call in `app.js`.
2. `cargo clippy`, `cargo fmt`, Biome on the changed assets.
3. On the Pi (needs approval): confirm a live stream part contains
   `X-Optic-Captured-At` with a plausible epoch, and that the value tracks
   the Pi's clock.
4. **User acceptance:** the label appears, reads the Pi's local time, and a
   scheduled capture is unaffected.

## Implementation Summary

- `src/camera.rs`: `PreviewFrame.captured_at`.
- `src/native_camera.rs`: stamped with `SystemTime::now()` at publish.
- `src/web.rs`: `X-Optic-Captured-At` header; tests extended.
- `src/web/index.html`, `styles.css`: the `#frame-time` overlay.
- `src/web/app.js`: `renderFrameTime()`, the cached `Intl.DateTimeFormat`
  keyed on the station timezone, and `stationTimezone` learned from
  `/api/status`.
- `Cargo.toml`/`Cargo.lock`: `0.1.32` → `0.1.33` (0.1.32 is already deployed).

## Validation (local, macOS)

| # | Check | Result |
|---|---|---|
| 1 | `cargo test --locked` | **272 passed** (2 new assertions + 1 new test) |
| 2 | `cargo clippy --locked --all-targets -- -D warnings` | clean |
| 3 | `cargo fmt` | clean |
| 4 | Biome 2.5.14 on `app.js`, `index.html`, `styles.css` | clean (one formatting fix applied) |
| 5 | Pi deploy | **pending** |
| 6 | User acceptance | **pending** |
