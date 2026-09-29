# Dated Worklog: 2026-09-29 - Focus Tools Silently Stopped Working After the Canvas Preview

Status: **implemented and locally tested**; Pi `--assets` deploy and user
acceptance pending.

## Objective

User report: Histogram, Peaking, Clipping and Loupe no longer do anything.
The buttons toggle, but nothing is drawn.

## Root Cause

A regression I introduced in PR #19. The iOS flicker fix replaced the
preview `<img id="preview">` with a `<canvas id="preview">`
(`worklogs/2026-09-28-ios-preview-stream.md`). `focus-tools.js` gained
`frameWidth()`/`frameHeight()` helpers for exactly this, and three call
sites were updated — but **`onFrame()` was missed**:

```js
if (!anyEnabled() || !img.naturalWidth || !img.naturalHeight) return;
```

`naturalWidth` is an `<img>` property; a `<canvas>` has `width`/`height`
and no `naturalWidth`. So the guard saw `undefined`, `onFrame()` returned
immediately on every frame, and all four tools did nothing. No error was
logged, because returning early is a legitimate path (it is how a
not-yet-loaded frame is skipped).

Why nothing caught it:

- The JS unit tests only covered the pure functions; the helpers lived
  inside the browser-only half of the file and were never exercised.
- The Rust asset tests pinned the canvas and `createImageBitmap`, not how
  the tools read the frame size.
- `worklogs/2026-09-28-ios-preview-stream.md` explicitly listed "focus tools
  not re-checked on a real device" as a remaining limitation. That gap is
  exactly where the defect sat.

## Fix

- `onFrame()` now reads the size through `frameWidth()`/`frameHeight()`.
- Those helpers moved from the browser-only half into the exported pure
  surface, so Node tests cover them.
- Tests:
  - `tests/web/focus-tools.test.js`: a canvas-shaped object, an image-shaped
    object (intrinsic size beats layout size), and not-ready sources
    returning 0.
  - `src/web.rs`: asserts `onFrame` uses `frameWidth(img)` and that the only
    `.naturalWidth`/`.naturalHeight` reads in the file are the helpers.

## Validation (local, macOS)

| # | Check | Result |
|---|---|---|
| 1 | `node --test tests/web/*.test.js` | **31 passed** (4 new assertions) |
| 2 | `cargo test --locked` | **280 passed** |
| 3 | Mutation check: reintroduce `img.naturalWidth` in `onFrame` and re-run | the new Rust test **fails**, so the guard is real |
| 4 | Biome 2.5.14 on `focus-tools.js` | clean |
| 5 | Pi deploy (`--assets`) | pending |
| 6 | User acceptance: all four tools on a device | pending |

## Remaining Limitations / Follow-up

- Still no automated test that exercises the tools against a real rendered
  frame; they are verified by unit tests plus a human clicking. A headless
  browser check would close this properly.
- The same class of bug could exist wherever other code assumes the preview
  is an `<img>`. `app.js` and `scheduled-exposure.js` were grepped for
  `naturalWidth`/`naturalHeight` and are clean.
