# Dated Worklog: 2026-09-20 - Phase 2h: DNG Checkbox Becomes a Real Staged Setting

Status: **implemented, deployed, and verified.**

## Objective

User reported, after Phase 2g shipped: "toggle Save companion DNG does
not make Save Button active?" Correct observation — the DNG checkbox
was never actually wired into the staged-config flow. It only ever fed a
one-off manual `CaptureRequest` at the moment "Capture Now" was clicked;
toggling it had no effect on `config_staged`, so Save/Discard never
reacted to it, and the preference never persisted anywhere.

## Acceptance Criteria

1. Toggling the DNG checkbox stages a real, server-side edit — Save
   Settings becomes enabled, Discard Changes becomes visible, matching
   every other camera setting.
2. Clicking Save Settings actually persists the DNG preference; a page
   reload shows the real committed value, not a decorative default.
3. No regression to the fix this depends on: Binning2k must still be
   forced off + disabled regardless.
4. Resolve the coherence question this reopens: now that
   `AppConfig.save_dng` has a real UI path to be set, should
   `optic_scheduler.rs::fire_capture` go back to reading it for
   `MasterArchive` (matching `Dci4k`), rather than the hardcoded `true`
   Phase 2g deliberately kept as a safety net against exactly this gap?

## Test Plan

- `cargo test --locked` (no new Rust tests needed — this is a thin new
  endpoint following an existing, already-tested pattern
  (`schedule_preview`'s read-merge-write), plus a scheduler match-arm
  change with no direct unit test, same as Phase 2g's).
- `cargo fmt`/`clippy`, `node --check`, Biome — same gates as every prior
  phase tonight.
- Live: toggle DNG via the new endpoint, confirm `config_staged` flips
  true; commit; confirm the committed config actually shows the new
  value; confirm Binning2k still rejects DNG.

## Implementation Summary

### `src/web.rs`
- New `POST /api/config/save-dng` (`SaveDngRequest { save_dng: bool }` →
  `stage_save_dng` handler): reads `current_app_config`, overwrites just
  `.save_dng`, writes to `preview_config_path` — identical read-merge-
  write pattern to `schedule_preview`. **Deliberately not** added to
  `StreamRequest`/routed through `reconfigure_stream`: that path only
  stages while `livePreview` is true (client-side `schedulePreviewUpdate`
  guard) — toggling DNG while the preview isn't running would have
  silently no-op'd. A DNG preference also has nothing to do with the live
  MJPEG pipeline at all, so piggybacking on it was the wrong vehicle
  semantically as well as unreliable.

### `src/optic_scheduler.rs`
- `fire_capture`'s DNG-selection `match` reverted back to reading
  `config.save_dng` for **both** `MasterArchive` and `Dci4k` (only
  `Binning2k` stays hardcoded `false`) — the exact change Phase 2g made
  and then deliberately reverted, now safe to make for real because the
  root cause (no way to ever set `config.save_dng` to `true`) is fixed by
  this phase's new endpoint. Comment rewritten to explain the full
  history rather than just the current state, since a future reader
  hitting this code mid-session would otherwise see two contradictory
  worklogs claiming opposite "final" answers.

### `src/web/app.js`
- `applyServerConfig`: `elements.saveDng.checked = config.save_dng`
  (real value, replacing Phase 2g's `defaultSaveDngFor` heuristic, which
  existed specifically because there was nothing real to read yet).
- New `stageSaveDng()`: POSTs the checkbox state to the new endpoint,
  then `refreshStatus()` — same shape as `stageStation()`.
- `#save-dng`'s own `change` listener now calls `stageSaveDng()`
  directly (it isn't inside `.control-grid`, so it needed its own
  listener, not swept up by the existing delegated one).
- Profile-radio `change` handler: removed the "apply a fresh default"
  step (`defaultSaveDngFor` deleted entirely) — `save_dng` is now a
  persistent setting like rotation/gain, not reset on profile switch.
  Still calls `stageSaveDng()` after `updateProfile()` so Binning2k's
  forced-off correction (inside `updateProfile()`) gets synced to the
  server immediately, not left stale if you switch away from a profile
  that had DNG checked.

### Docs
- `docs/optic-daemon-scheduler.md` §6 rewritten a third time tonight to
  state the final, coherent behavior plainly and explain the two
  intermediate states it passed through in this same session, rather
  than presenting it as though it were always this way.
- `docs/optic-daemon.md`'s profile-comparison prose updated to state the
  checkbox is a real staged/saveable setting now, not a one-off choice.

## Validation

- `cargo test --locked`: **102 passed, 0 failed.**
- `cargo fmt --all -- --check` / `cargo clippy --locked --all-targets --
  -D warnings`: clean.
- `node --check src/web/app.js`: clean. Biome across all six touched web
  assets: clean.
- Deployed via `./scripts/build-deploy-optic-daemon.sh`.
- **Live verification** against the real deployed Pi:
  - Confirmed the committed config started at `save_dng: false`,
    `config_staged: false`.
  - `POST /api/config/save-dng {"save_dng": false}` (the same as the
    already-committed value) → 200, but `config_staged` correctly stayed
    `false` — the content-comparison `config_is_staged` check correctly
    treats "staged an identical value" as "nothing actually changed,"
    not a false-positive dirty flag.
  - `POST /api/config/save-dng {"save_dng": true}` (a genuinely different
    value) → 200; `GET /api/status` immediately after → `config_staged:
    true`, `config.save_dng: true` in the staged preview — confirms the
    new endpoint's write is picked up with no special-casing needed.
  - `POST /api/config/commit` → 200; `GET /api/status` afterward →
    `config_staged: false` and `config.save_dng: true` (committed) —
    confirms the preference actually persisted through commit, not just
    staged. Left committed at `true`, Master Archive's recommended
    default, matching the Pi's pre-existing intended state.
  - Regression check: `POST /api/capture {"profile": "binning_2k",
    "save_dng": true}` still 422 — Binning2k's policy unaffected.
  - Confirmed scheduler still `Paused`, original `every1min` rule intact,
    throughout.

## Remaining Limitations / Follow-up

- No interactive browser verification (headless session), same stated
  limitation as every prior phase tonight — verified via the API and a
  real commit round-trip, not by clicking the checkbox in a browser.
- This closes the gap flagged in Phase 2g's own "Remaining Limitations"
  section (`AppConfig.save_dng` being effectively dead) more completely
  than that worklog anticipated — worth noting for anyone reading these
  worklogs in order, since Phase 2g's text is now superseded by this one
  on that specific point.
