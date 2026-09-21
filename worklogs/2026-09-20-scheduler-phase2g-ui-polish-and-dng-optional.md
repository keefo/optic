# Dated Worklog: 2026-09-20 - Phase 2g: Capture-History Quick Range, Dashboard Config-Staging Fix, Master Archive Optional DNG

Status: **implemented, deployed, and verified.**

## Objective

Five items requested directly by the user in quick succession, tonight:

1. A "last X hours" quick filter on the capture-history page.
2. Shorter `.pill` padding specifically in the capture-history results
   table (not the header status pills elsewhere).
3. Migrate `app.js`'s camera-settings Save/Discard dirty-tracking off the
   old client-only `baselineSettings` pattern onto the server-truth
   `config_staged` pattern already used by the Station card and the
   scheduler page (explicitly deferred earlier tonight, now requested).
4. Let Master Archive's companion-DNG checkbox be unchecked for a manual
   capture (previously forced checked + disabled).
5. Swap the dashboard's Save Settings/Discard Changes button order, and
   make Discard *hidden* (not just disabled) when there's nothing staged
   — matching the Station card's existing convention.

## Acceptance Criteria

1. `/capture-history.html` has a "Quick range" selector (1h/6h/24h/7d/30d)
   that fills the "Captured after" filter and applies immediately;
   editing either date field by hand reverts the selector to "Custom."
2. `.pill` elements inside the capture-history results table render with
   `padding: 1px 8px`; header status pills elsewhere are unaffected.
3. The dashboard's Save Settings/Discard Changes buttons reflect
   `status.config_staged` from the server (like Station), not a stale
   client-side snapshot; on page load the settings form shows the real
   committed-or-staged config, not hardcoded defaults.
4. `POST /api/capture` (and `/api/config/commit`, transitively) accept
   `{profile: "master_archive", save_dng: false}` without a 422.
5. Save Settings is first, Discard Changes second; Save is `disabled`
   and Discard is `hidden` when `config_staged` is false, and vice versa.
6. No regression to the scheduler's own Master Archive DNG behavior —
   scheduled (timelapse) Master Archive captures must still always
   include the DNG, since there is still no way to stage a "no DNG for
   scheduled Master Archive shots" preference from the UI.

## Test Plan (written before implementation)

- `cargo test --locked` for the two backend-touching items (4: relaxed
  `validate_raw_policy` + updated `estimated_bytes_per_shot` tests; the
  `fire_capture` DNG-selection change has no direct unit test — covered
  by reasoning + a live capture, see below).
- `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D
  warnings`.
- `node --check` + Biome on every touched web asset before deploying.
- Live verification on the deployed Pi: confirm the quick-range filter's
  API calls narrow results correctly against real data, confirm the
  dashboard's Save/Discard buttons reflect real `config_staged` state
  (not just code review), and — the one item genuinely risky enough to
  need hardware proof, not just review — fire one real manual
  `MasterArchive` capture with `save_dng: false` and confirm it succeeds
  (200, not 422) and actually omits the DNG file.

## Implementation Summary

### 1. Capture-history quick range (`src/web/capture-history.html`, `.js`)
- New `#filter-quick-range` `<select>` (1h/6h/24h/7d/30d/Custom), placed
  first in the filter form.
- `formatForDatetimeLocal(date)`: renders a `Date` as a `datetime-local`
  value in **local** time (the inverse of the existing
  `localInputToUnixMs`, which already parsed `datetime-local` as local
  time) — `toISOString()` would have silently shown the field in UTC.
- Selecting a range fills "Captured after" from `Date.now() - N hours`,
  clears "Captured before," resets `offset`, and applies immediately —
  no separate Apply click needed, the point of a *quick* filter.
- Manually editing either date field resets the selector back to
  "Custom / Any" so it never shows a stale, contradicted selection.

### 2. `.pill` padding (`src/web/styles.css`)
- Added `.forecast-table .pill { padding: 1px 8px; }`, scoped to the
  table class capture-history's Outcome column uses. The shared `.pill`
  rule (used by header status pills on every page) is untouched.

### 3. `app.js` config-staging migration
- Removed `baselineSettings` and `checkSettingsModified()` entirely.
  Investigating this uncovered two real pre-existing bugs, not just an
  architectural nit: `checkSettingsModified()` was never actually wired
  to any input/change listener (only called once at script init and
  after commit/discard), so the Save/Discard buttons likely never
  updated live as a user adjusted a slider; and the settings form always
  displayed hardcoded `defaults` on load, never the real committed/
  staged config (`baselineSettings` started as `{...defaults}` and was
  only ever updated *after* a commit, never read from the server).
- New `applyServerConfig(config)` + `cameraFieldsInitialized` guard
  (mirrors `stationFieldsInitialized`): populates the profile radios and
  settings fields from `status.config` exactly once per page load inside
  `refreshStatus()`, not on every 3s poll (would otherwise fight an
  in-progress edit).
- `elements.saveConfig.disabled` / `.discardConfig.hidden` now driven
  directly from `status.config_staged` on every poll — same pattern as
  Station, same file, same function.
- `fnDiscardConfig` resets **both** `cameraFieldsInitialized` and
  `stationFieldsInitialized` before its `refreshStatus()` call: commit/
  discard act on one shared `preview_config.json` (design doc §2.1), so
  the "Discard Changes" button can revert a staged Station edit too, not
  only camera settings — needed for the fields to actually refresh in
  that case rather than showing a stale discarded value.

### 4. Master Archive optional DNG
- `src/camera.rs::validate_raw_policy`: removed the
  `(MasterArchive, false) => Err(...)` arm. `Binning2k`'s "no DNG" rule
  is untouched — not asked, no reason to touch it.
- `src/web/index.html` / `app.js`: `#save-dng` no longer force-disabled
  for `master_archive`; `updateProfile()` now only handles enable/
  disable + help text, not the checked value (that's the caller's job —
  see below). New `defaultSaveDngFor(profile)` (`true` only for
  `master_archive`) used both by the profile-radio `change` handler
  (user actively switching profile — apply a fresh sensible default) and
  by `applyServerConfig` (page load — same default, *not* sourced from
  `AppConfig.save_dng`, see the next bullet for why).
- **Caught and fixed before shipping**: my first pass had
  `optic_scheduler.rs::fire_capture` read `config.save_dng` for
  `MasterArchive` too (matching `Dci4k`'s existing behavior), reasoning
  that `validate_raw_policy` no longer forced it. This would have been a
  real regression — `AppConfig.save_dng` has **no UI path that ever
  writes it to anything but its `false` default**
  (`reconfigure_stream`/`start_stream` deliberately preserve it
  unchanged when staging a live-preview profile/settings change; the
  dashboard's DNG checkbox only ever feeds a one-off manual
  `CaptureRequest`, never the staged config) — so every future
  *scheduled* Master Archive frame would have silently lost its DNG.
  Reverted: `fire_capture` still hardcodes `save_dng = true` for
  `MasterArchive` specifically, unchanged from before tonight. Only the
  *manual* capture path (`CaptureRequest.save_dng`, read straight from
  the dashboard checkbox at click time) actually changed. Documented in
  both `optic_scheduler.rs`'s own comment and
  `docs/optic-daemon-scheduler.md` §6 (rewritten to state the split
  explicitly, since it's no longer "scheduled captures exactly follow
  `validate_raw_policy`").
- `src/web.rs::estimated_bytes_per_shot`: added a `MasterArchive`
  no-DNG estimate (7,783,245 B — the already-measured JPEG-only
  component of the existing daytime sample, not a new guess, and
  already the larger/conservative of the two known JPEG samples per
  this function's existing documented philosophy). Two tests updated:
  the old `..._ignores_save_dng_for_profiles_that_force_it_either_way`
  (which asserted MasterArchive-with-DNG == MasterArchive-without,
  no longer true) narrowed to only cover `Binning2k` (still forced);
  a new parametrized test asserts `with_dng > without_dng` for both
  `MasterArchive` and `Dci4k` (profiles where DNG is now optional).
- Docs: `docs/optic-daemon.md`'s profile-comparison prose and
  `docs/optic-daemon-scheduler.md` §6 both updated to state the new
  manual-vs-scheduled split plainly, cross-referencing each other.

### 5. Save/Discard button swap + hidden-vs-disabled
- `src/web/index.html`: `#save-config` now appears before
  `#discard-config` in markup; `discard-config`'s initial attribute
  changed from `disabled` to `hidden` (matching `discard-station`'s
  existing markup exactly).
- Behavior wired in item 3's `refreshStatus()` changes above — both
  items landed together since they touch the same lines.

## Validation

- `cargo test --locked`: **102 passed, 0 failed** (no new Rust tests
  needed for items 1/2/3/5 — pure frontend/CSS; item 4 added one
  parametrized test and narrowed one existing test, net same count as
  before plus the earlier Phase 2f additions already counted).
- `cargo fmt --all -- --check`: clean. `cargo clippy --locked
  --all-targets -- -D warnings`: clean.
- `node --check` on `app.js` and `capture-history.js`: clean.
- `npx --yes @biomejs/biome@2.5.14 check` across every touched web asset
  (`app.js`, `index.html`, `scheduler.js`, `scheduler.html`,
  `capture-history.js`, `capture-history.html`): clean, before deploying.
- Deployed via `./scripts/build-deploy-optic-daemon.sh`.
- **Live verification**, all against the real deployed Pi:
  - `GET /capture-history.html` still 200; quick-range control present
    in the served HTML.
  - `GET /` (dashboard) confirmed to serve the swapped button order and
    the `hidden` (not `disabled`) initial attribute on Discard Changes.
  - `POST /api/capture` with `{profile: "master_archive", settings: {},
    save_dng: false}` — the specific behavior change with real
    consequences if wrong — fired a real capture on hardware and
    succeeded (not the previous 422 "Master Archive requires a
    companion DNG"), with exactly one file (`.jpg`) in the response,
    confirming the relaxed policy actually took effect end-to-end, not
    just in unit tests.
  - Confirmed `/api/status`'s `config_staged` still correctly reflects
    the real preview-vs-cache diff after this capture (unaffected by
    this change, since manual captures don't touch `preview_config.json`
    at all — `capture()` never writes to it, only `reconfigure_stream`/
    `start_stream`/`schedule_preview` do).
  - Regression check: `POST /api/capture` with `{profile: "binning_2k",
    save_dng: true}` still correctly rejected (422, "2K Binning does not
    support companion DNG capture") — confirms only `MasterArchive`'s
    rule changed, `Binning2k`'s is untouched.
  - `GET /` confirmed to serve `save-config` before `discard-config` in
    markup, `discard-config` with `hidden` (not `disabled`); `app.js`
    confirmed to contain `saveConfig.disabled = !status.config_staged`
    and `discardConfig.hidden = !status.config_staged`.
  - `GET /capture-history.html` confirmed to contain
    `id="filter-quick-range"`; `GET /styles.css` confirmed to contain
    `.forecast-table .pill { padding: 1px 8px; }`.
  - `/api/status` confirmed scheduler still `Paused`, original
    `every1min` rule intact, throughout and after all of the above.

## Remaining Limitations / Follow-up

- No interactive browser verification (headless session) of the
  quick-range selector's UI behavior or the Save/Discard button
  swap/hide — verified via served-content checks and a live capture,
  not by clicking through the page. Same stated limitation as Phase 2f.
- `AppConfig.save_dng` remains effectively dead for `MasterArchive` (and,
  pre-existing, still only nominally wired for `Dci4k` — see §6's
  rewritten text) — there is still no dashboard control that stages a
  DNG preference for *scheduled* captures at all, for any profile. Not
  addressed tonight; flagged as a real gap in the design doc rather than
  silently left implicit, in case a future "schedule-time DNG toggle" is
  ever wanted.
