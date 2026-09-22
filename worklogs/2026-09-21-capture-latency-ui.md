# Dated Worklog: 2026-09-21 - Capture Latency in the MEASUREMENT Panel

Status: **deployed (assets only), awaiting user check** — T3–T5 pending the
user's captures.

## Objective

User request: show capture latency in the dashboard's MEASUREMENT panel,
alongside the existing preview-responsiveness metrics. The data already
exists server-side (`capture perf` logs, capture history `duration_ms`), but
the operator only sees it by reading the journal. Builds on
`worklogs/2026-09-20-capture-latency.md`.

## Acceptance Criteria

- After each Capture & transfer, the MEASUREMENT panel shows the capture
  latency: click to response, which is the wait the operator feels. The
  panel already measures preview responsiveness this way, in the browser.
- The card also shows which capture it describes (profile, and DNG when on)
  and the median of recent captures of the **same** profile/DNG combination.
  Mixing profiles would give a meaningless median (0.7 s vs 2.5 s).
- A failed capture shows as failed and is not added to the median.
- Existing "First visible" / "Median" preview metrics are unchanged.
- The layout works at desktop width (the three-column grid already has a free
  cell) and at phone width (the existing single-column rule).

## Design

- Browser-side timing with `performance.now()` around the `/api/capture`
  request in `captureAndTransfer()` (`src/web/app.js`). No API, `src/web.rs`
  or Rust change.
- Samples are kept per `profile + save_dng` key, last 10 each, for this page
  session only. This matches the existing preview metrics, which also reset
  on reload.
- One new card in `.responsiveness-metrics` (`src/web/index.html`); no CSS
  change needed.
- Browser time includes the network round trip, a few ms on the LAN; it
  should track the daemon's `http_handler_total` closely.

## Coordination Note

`src/web/app.js` and `src/web/index.html` are outside this track's owned
files (`CLAUDE.md` / the capture-latency worklog). The user requested the
change directly. It is kept small and local: one card and a few lines around
`captureAndTransfer()`. Merge-conflict risk: other tracks that edit the same
dashboard files (likely `feat/health-alerts`).

## Test Plan

- T1 lint: Biome, using the repo's `biome.json`, on `src/web/app.js` and
  `src/web/index.html` (the same check the deploy script runs).
- T2 Rust suite unaffected: `cargo test --locked`. It includes
  `web::tests::embedded_assets_are_present`.
- T3 on the Pi (needs a deploy; assets-only fast path `--assets`, user
  approval): after one capture per profile, the card shows a value within
  ~50 ms of that capture's `http_handler_total` in the journal. The label
  names the right profile/DNG. The median uses only matching captures.
- T4 failure case: a capture that fails (e.g. sent while the camera is busy)
  shows "Failed" and leaves the median unchanged. If this can't be triggered
  on the Pi, verify it by code review and record it as such.
- T5 phone width: the card stacks in one column (existing media rule).

## Implementation

- `src/web/index.html`: third card in `.responsiveness-metrics`: "Capture",
  `#capture-latency`, `#capture-latency-detail` (initial text "Click to files
  queued").
- `src/web/app.js`: `captureSamples` map (per `profile:save_dng`, last
  `MAX_MEASUREMENT_SAMPLES` = 10); `recordCaptureLatency()`;
  `captureAndTransfer()` timestamps at the click, shows "Measuring…", records
  `performance.now()` delta once the response JSON is parsed, and shows
  "Failed · not counted" on error. `api()` throws on any non-OK response, so
  HTTP errors reach that path.

## Validation

| Check | Result |
|---|---|
| T1 `npx @biomejs/biome@2.5.14 check src/web/app.js src/web/index.html` (repo `biome.json`) | pass: "Checked 2 files … No fixes applied." |
| T2 `cargo test --locked` (macOS) | pass: 112 passed / 0 failed, incl. `web::tests::embedded_assets_are_present` |
| Assets deploy (user-approved) | Pi daemon confirmed still on this branch (`camera.rs` sha256 `934acbd5…`, service up since 01:18:31 PDT). `./scripts/build-deploy-optic-daemon.sh --assets`: "SUCCESS: static web assets deployed and verified"; no restart. Served `/` contains `capture-latency`, served `/app.js` contains `recordCaptureLatency`. |
| T3–T5 on the Pi | pending user captures after a dashboard reload |

## Follow-up: Button-Ready Trace (2026-09-21)

User report: the card showed 780 ms for a 2K capture, but the button felt
1.5–3 s slow. Cause (by code reading): `captureAndTransfer()` keeps the
button disabled after the capture returns, through `POST_CAPTURE_FREEZE_MS`
(3,000 ms frozen-still hold) and then `ensurePreview()` (preview restart).
Only then does it call `setBusy(false)`.

Change: `traceButtonReady()` in `src/web/app.js` appends
`button ready +X ms (hold Y ms, preview Z ms)` to the card's detail line, and
logs the same string with `console.info("capture perf: …")`. Timing behaviour
is unchanged; the 3 s hold is a deliberate UX choice (existing comment) and
is left for the user to decide.

Checks: Biome 2.5.14 pass ("Checked 2 files … No fixes applied."). Assets
deploy (user-approved): "SUCCESS: static web assets deployed and verified",
served `/app.js` contains `traceButtonReady`. Daemon not restarted by this
deploy. It had restarted at 18:24:45 PDT for another reason (PID 900, likely
a reboot) and still runs this branch (`camera.rs` sha256 `934acbd5…`).
Pending: user reload + captures to read the trace.
