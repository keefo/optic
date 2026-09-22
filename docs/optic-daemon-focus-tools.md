# Design Document: Dashboard Focus Tools (Histogram, Focus Peaking, Loupe)

Status and validation history: `worklogs/2026-09-21-focus-tools.md`.

## 1. Purpose

Setup-visit tools on the dashboard's live preview for the manual-focus 6 mm
CS-mount lens:

- a live **histogram** with shadow and highlight clipping readouts;
- **focus peaking**, an overlay that highlights in-focus edges;
- a **loupe**, a magnified crop around a chosen point with a numeric
  **sharpness score** to maximise while turning the focus ring.

## 2. Architecture

Everything runs in the browser, on frames the page already decodes. There
are no Rust, camera or API changes, and the processing cost lands on the
viewing computer, not the Pi.

- `app.js` parses the MJPEG multipart stream itself and decodes each frame
  into `#preview` as a same-origin blob URL (`renderMjpegFrame`). Drawing
  that image to a canvas does not taint the canvas, so pixels can be read
  back with `getImageData`. The page never depends on `<img>` rendering
  MJPEG natively.
- `src/web/focus-tools.js` is loaded before `app.js` (both `defer`) and
  exposes `window.OpticFocus`. `app.js` has two hooks:
  - `window.OpticFocus?.onFrame(elements.preview)` after
    `recordRenderedFrame`, so the responsiveness measurement is taken
    before any focus-tool work;
  - `window.OpticFocus?.clear()` in `hidePreview`.
- Assets are served from disk, so the tools ship through
  `./scripts/build-deploy-optic-daemon.sh --assets` with no daemon restart
  (see `docs/optic-daemon.md`, Web Asset Loading).

`focus-tools.js` has two halves:

1. **Pure functions** on `{ data, width, height }` RGBA pixel data, exported
   through `module.exports` for Node tests (`tests/web/focus-tools.test.js`,
   run with `node --test tests/web/*.test.js`):

   | Function | Result |
   | --- | --- |
   | `computeHistogram(image)` | 256-bin R, G, B and Rec. 709 luma histograms; `shadowClip` (fraction with luma ≤ 2) and `highlightClip` (fraction with any channel ≥ 254) |
   | `toGray(image)` | Rec. 709 luma as `Float32Array` |
   | `sobelMagnitude(gray, w, h)` | Sobel gradient magnitude; one-pixel border is 0 |
   | `peakingThreshold(sensitivity)` | Absolute Sobel threshold: sensitivity 0 → 220, 1 → 40 (linear, clamped) |
   | `sharpnessScore(gray, w, h)` | Tenengrad: mean squared Sobel magnitude over the interior |
   | `loupeRect(cx, cy, size, fw, fh)` | Crop rectangle centred on the point, clamped inside the frame, shrunk to the frame if smaller |
   | `clientToImagePoint(x, y, rect, nw, nh, clamp)` | Maps a click to image pixels under `object-fit: contain` letterboxing; `null` outside the image, or with `clamp` the nearest image-edge point (used while dragging) |

2. **A UI controller** that wires the toolbar, overlay, histogram panel and
   loupe panel, and implements `onFrame(img)` and `clear()`.

## 3. Behaviour

### 3.1 Controls

Under the preview is a toolbar with three toggle buttons (`aria-pressed`):
**Histogram**, **Peaking** and **Loupe**. A **Peaking sensitivity** slider
(0–100, default 50) is shown only while peaking is on. All tools are off by
default. The toggle state and sensitivity are remembered per viewer in
`localStorage` (`optic.focusTools`); every access is wrapped in try/catch
and the page works without storage.

### 3.2 Histogram

A small translucent panel in the bottom-left corner of the preview. The
luma histogram is drawn as filled bars with R, G and B curves on top. It is
scaled to the tallest *interior* bin (1–254), so a clipped spike at 0 or
255 does not flatten the rest of the curve. Below it: `Shadows clipped N% ·
Highlights clipped N%`.

### 3.3 Focus peaking

A `<canvas>` (`#peaking-overlay`) stacked on `#preview` with the same
`object-fit: contain` and `pointer-events: none`, so it lines up at any
window size. Pixels whose Sobel magnitude is at or above
`peakingThreshold(sensitivity)` are painted magenta.

The threshold is **absolute**, not a percentile of the frame. A percentile
would always highlight the same fraction of pixels; an absolute threshold
makes a defocused frame light up less, so the highlighted area grows as
focus improves. Higher sensitivity lowers the threshold and highlights
more.

### 3.4 Loupe

When on, a panel shows a 160 × 160 source-pixel crop around the focus
point, drawn at 2× with nearest-neighbour scaling. The crop is read from
the full-resolution decoded frame, not the downscaled working copy. A green
rectangle on the overlay marks the crop on the preview.

- **Focus point:** the frame centre by default. Clicking the preview places
  it, and pressing and dragging moves it live (pointer events with pointer
  capture, so mouse, pen and touch all work; the cursor is a crosshair
  while the loupe is on). A drag past the image edge pins the box to the
  edge. Redraws during a drag are coalesced to one per animation frame, and
  the browser's native image drag is suppressed. Moving the point resets
  the best score.
- **Sharpness:** the Tenengrad score of the crop, with `N% of best`, and the
  best score so far with a **Reset best** button. The score is only
  comparable for the same crop and scene, which is how it is meant to be
  used: turn the ring until the score peaks, then lock the ring.
- **Scale label:** "1:1 sensor pixels" only when the frame is 4056 px wide
  (Master Archive with Downsample preview off). Otherwise it reads "Preview
  pixels (W × H)" and suggests Master Archive with downsampling off for a
  1:1 check.

### 3.5 Processing budget

- With every tool off, `onFrame` returns immediately: no canvas work.
- `onFrame` is synchronous and runs inside the frame render, so processing
  cannot pile up behind the stream. The only extra calls are a toggle, and
  loupe moves, at most one per animation frame.
- The histogram and peaking run on a copy downscaled to at most a 960 px
  long edge. The loupe reads only its own crop.
- Any exception is caught and logged with `console.warn`; a tool can never
  break the preview.

### 3.6 Preview stopped or frozen

`hidePreview` calls `clear()`, which blanks the overlay and histogram and
resets the readouts. While the preview is frozen (for example during a
capture), no frames arrive and the tools keep showing the last frame. They
resume with the next frame.

## 4. Limitations and follow-ups

- The loupe is 1:1 with the sensor only in Master Archive with downsampling
  off. A true sensor-resolution crop in every profile would need libcamera
  `ScalerCrop` on the Pi (a Rust and camera change).
- No clipping zebras on the image, and no waveform or RGB parade.
- The histogram is computed on the JPEG preview after the ISP and JPEG
  encoding, not on RAW data, so it approximates the still capture's
  exposure.
