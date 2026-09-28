# Dated Worklog: 2026-09-28 - Live Preview Fails on iOS Safari ("load failed")

Status: **implemented, deployed (0.1.32 + assets), and user-verified on iOS**
(flashing confirmed gone by the user; layout change awaiting their comment).

## Objective

The dashboard's live preview does not work on iOS Safari. The page shows
"Preview stream interrupted: Load failed". Every other page and API call
works on the same device. Chrome on the Mac works fine against the same
daemon, through the same Tailscale proxy.

## Evidence (collected before any change)

From the Mac-side reverse proxy's access log
(`~/Library/Logs/optic-proxy.log`, Caddy JSON), same daemon, same URL:

| Client | `GET /api/stream/mjpeg` | Bytes delivered |
| --- | --- | --- |
| Chrome / curl (Mac) | 200 | 73,841 then 4,499,493 |
| iPhone, iOS 18.7 Safari | 200 (x6 retries) | **0**, request ends immediately |

So the response headers arrive, the body delivers nothing, and the fetch
fails. "Load failed" is Safari's generic `TypeError` for a failed fetch.
The retries are `consumeMjpeg`'s own reconnect path in `app.js`.

## Root cause (hypothesis to confirm with the fix)

`app.js` does not use `<img src=...>` for the preview. It calls
`fetch('/api/stream/mjpeg')` and parses the multipart framing itself, so it
can read the per-frame `X-Optic-*` headers (sequence, exposure, gain,
clipped fraction) that the responsiveness panel and the ramp UI display.

WebKit handles `multipart/x-mixed-replace` inside its network layer — the
legacy "server push replace" mechanism it implements for `<img>`. A
`fetch()` of such a response does not hand the parts to JavaScript;
WebKit consumes them, so `response.body` yields nothing and the fetch
fails. Chromium has no such interception, which is why Chrome works.

The framing bytes are identical either way. Only the `Content-Type` makes
WebKit intercept.

## Approach

Let the caller ask for the same byte stream under a neutral media type:

- `GET /api/stream/mjpeg` keeps `multipart/x-mixed-replace; boundary=frame`
  (unchanged, still valid for direct `<img>` use, as `docs/optic-daemon.md`
  documents).
- `GET /api/stream/mjpeg?parser=js` returns exactly the same bytes with
  `Content-Type: application/octet-stream`, which WebKit does not
  intercept.
- `app.js` requests the `parser=js` form, since it always parses the
  stream itself.

This keeps one server code path and one client parser. It is not an
iOS-specific branch: every browser gets the non-intercepted type.

## Acceptance Criteria

1. The live preview runs on iOS Safari: frames paint, and the stream is
   not interrupted.
2. Chrome and Safari on macOS still work, with the responsiveness panel,
   the focus tools and the ramp readouts unchanged (they depend on the
   per-frame headers, which the framing still carries).
3. `GET /api/stream/mjpeg` without the query parameter still returns
   `multipart/x-mixed-replace`, so a direct `<img>` still works.
4. No change to frame content, framing, headers or cadence.

## Test Plan (written before implementation)

Local (macOS):

1. New `web.rs` unit tests: the default request keeps
   `multipart/x-mixed-replace; boundary=frame`; `?parser=js` returns
   `application/octet-stream`; both keep the no-store headers. An asset
   test asserts `app.js` requests the `parser=js` form.
2. `cargo fmt --check`, `cargo test --locked`, `cargo clippy --locked
   --all-targets -- -D warnings`.
3. Biome on the changed web assets.

On the Pi (needs user approval; this is a Rust change, so a full deploy
with a version bump at merge):

4. `curl -sI 'http://optic.local:8000/api/stream/mjpeg'` → multipart;
   `curl -sI '...?parser=js'` → application/octet-stream.
5. Confirm bytes still flow: read the first part header from the
   `parser=js` form and check it contains `X-Optic-Sequence`.
6. **User verification on the iPhone** (the actual acceptance): open the
   dashboard over the tailnet, confirm the preview paints and no
   "interrupted" notice appears. Then re-check Chrome on the Mac.

## Second Defect Found After the Fix: Flashing on iOS

With the stream working, the preview flashed black 2-3 times a second on
iOS Safari (not on Chrome/macOS). Cause: `app.js` rendered each frame by
assigning a blob URL to `<img id="preview">.src` and awaiting `decode()`.
WebKit blanks an image element as soon as `src` changes and only paints
once the new frame has decoded, so every frame left a visible gap. Chromium
keeps the previous frame on screen during the decode, which is why the Mac
looked fine.

Fix: the preview is now a `<canvas>`. Each frame is decoded with
`createImageBitmap()` and drawn with `drawImage()`, so the previous frame's
pixels stay until the next draw and there is no blank state. It also
removes the per-frame `createObjectURL`/`revokeObjectURL` pair.

## Third Change (user request, same session)

The status line (`#stream-state`, e.g. "Live · 4056 × 3040 · 2 FPS · White
balance Daylight · … · Analogue gain 11.5×") sat above the preview and
re-wrapped whenever a control changed, pushing the image down the page. It
now sits **below the preview frame and below the Capture/Stop buttons**,
with a reserved `min-height` so one- and two-line states do not move
anything.

## Implementation Summary

- `src/web.rs`: `MjpegParams { parser }`, `mjpeg_content_type()`, and
  `mjpeg_stream` takes `Query<MjpegParams>`. Default stays
  `multipart/x-mixed-replace; boundary=frame`; `?parser=js` returns
  `application/octet-stream`. Same bytes either way.
- `src/web/app.js`: requests `?parser=js`; renders through
  `createImageBitmap` + `drawPreviewFrame()` into the canvas;
  `clearPreviewCanvas()` replaces `removeAttribute("src")`; the `<img>`
  `error` listener and all blob-URL handling are gone.
- `src/web/index.html`: `<img id="preview">` → `<canvas id="preview"
  role="img" aria-label="HQ Camera preview">`; `#stream-state` moved out of
  the section heading to below the button row.
- `src/web/focus-tools.js`: `frameWidth()`/`frameHeight()` helpers, so the
  tools read the intrinsic size of either a canvas or an image.
- `src/web/styles.css`: the preview box rules apply to the canvas;
  `#stream-state` restyled for its new position.
- `docs/optic-daemon.md`: the `/api/stream/mjpeg` entry documents `?parser=js`
  and why it exists.
- `Cargo.toml`/`Cargo.lock`: `0.1.31` → `0.1.32` (this deploy also carried
  the six PRs merged since 0.1.31 to the Pi for the first time).

## Validation

| # | Check | Environment | Result |
|---|---|---|---|
| 1 | `cargo test --locked` | macOS | **271 passed**, including the three new tests |
| 2 | `cargo clippy --locked --all-targets -- -D warnings` | macOS | clean |
| 3 | `cargo fmt --check` | macOS | clean |
| 4 | Biome 2.5.14 on the changed web assets | macOS | clean |
| 5 | `node --test tests/web/*.test.js` | macOS | pass |
| 6 | Full deploy | Pi | **265 tests passed on the Pi**, `SUCCESS: optic-daemon 0.1.32 is active` |
| 7 | Content types on the Pi, with a preview running | Pi | default → `multipart/x-mixed-replace; boundary=frame`; `?parser=js` → `application/octet-stream`, 8,550,394 bytes in ~5 s, framing and `X-Optic-Sequence` intact |
| 8 | `--assets` deploy of the canvas + layout change | Pi | `SUCCESS`; served page contains `<canvas id="preview"`, `app.js` contains `createImageBitmap`, `#stream-state` follows `#preview-toggle` |
| 9 | **iOS Safari 18.7** | iPhone, over the tailnet proxy | Preview renders, and the user confirmed the **flashing is gone** |

Diagnosis evidence for #9's root cause is in "Evidence" above: the Caddy
access log showed the iPhone receiving 0 bytes on six MJPEG attempts while
the Mac received 73 KB and 4.5 MB on the same endpoint.

## Remaining Limitations / Follow-up

- The user has not yet commented on the moved status line; that part is
  deployed but not confirmed.
- The `<img>`-only path (`/api/stream/mjpeg` without `parser=js`) is no
  longer exercised by any page. It is kept because `docs/optic-daemon.md`
  documents it for direct `<img>` use, and a unit test pins the content
  type.
- Not re-checked after the canvas change: the focus tools (histogram,
  peaking, clipping, loupe) on a real device. Their size lookups changed,
  their unit tests pass, but nobody has clicked through them since.
- The responsiveness panel's "first visible" timing now measures canvas
  draw plus two animation frames rather than `<img>` decode plus paint.
  Numbers before and after this change are not directly comparable.
