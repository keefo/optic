# Dated Worklog: 2026-09-20 - Rule Toggle Silently Stuck Unsaved After a Page Refresh

Status: **implemented, deployed, and verified**.

## Objective

User reported: "rule toggle button, still not go through 'Save rules'
button" — the per-rule enabled/disabled checkbox in the Timelapse
Scheduler's rule list. Clarified via follow-up question: this is about
the checkbox in the rules list (not the Run control Pause/Resume button,
which is intentionally instant/live and unrelated to staged config).

## Root Cause

Toggling the checkbox already correctly stages the edit
(`stageAndRefreshForecast()` → `POST /api/schedule/preview`, which writes
`preview_config.json` and sets the in-memory `isDirty = true`, enabling
"Save rules" and revealing "Discard changes" — this part was fixed
earlier in the session). The gap is what happens on a **page reload**
before that edit is saved:

- `loadInitial()` fetches `/api/status`, whose `config` field is
  `current_app_config()` — the staged preview if one exists, else the
  committed config (`src/web.rs` doc comment on `current_app_config`).
  So a reload correctly shows the toggled-but-unsaved checkbox state —
  this part already worked.
- But `loadInitial()` then unconditionally set `isDirty = false` and
  called `renderSaveDiscardButtons()`, regardless of whether the config
  it just loaded was the staged preview or the real committed config.
- Net effect: after toggling a rule and refreshing without clicking
  "Save rules", the checkbox looks right, but "Save rules" goes back to
  *disabled* and "Discard changes" back to *hidden* — even though a real,
  uncommitted edit is sitting in `preview_config.json` and the live
  scheduler (which only re-reads config on `commit_config`'s
  `notify_config_changed()`) is still running the old, committed rule
  set. The user has no way left in the UI to either commit or discard
  that edit; it just silently persists in limbo until something else
  (e.g. a future explicit save/discard, or manual file deletion) resolves
  it.
- Confirmed by reading `src/optic_scheduler.rs`'s `SchedulerHandle::spawn`
  / `run_actor`: the scheduler actor is constructed with
  `config_cache_path` (the *committed* tmpfs cache), never the preview
  path, and only re-reads on an explicit `ConfigChanged` command sent
  from `commit_config`. So the staged-but-stuck edit never takes effect
  on the actual running scheduler either — it's not just a UI cosmetic
  gap, it's a real "this edit is nowhere: not applied, not visibly
  saveable" state.

`StatusResponse` had no field distinguishing "this `config` came from a
staged preview" from "this is the real committed config", so the
frontend had no way to tell the two apart on load.

**Second layer, found while implementing the first fix and verifying it
live:** the obvious fix — a `config_staged: bool` computed from
`preview_config_path.exists()` — is itself wrong, and the live
verification against the Pi caught it immediately (`config_staged: true`
on a freshly-committed, nothing-pending daemon). Root cause:
`discard_config` does **not** delete `preview_config.json` — it
overwrites it with a copy of the committed cache's content (`src/web.rs`
`discard_config`'s own doc comment: "Revert preview state by copying the
committed config's tmpfs cache back into preview_config.json"), and
`commit_config` doesn't touch `preview_config_path` at all. So the file
exists on disk from the moment *anything* is ever staged for the first
time, for the rest of the daemon's uptime — existence can never be the
"is something pending" signal, only **content** can: whether
`preview_config.json`'s content currently differs from
`config_cache.json`'s.

## Acceptance Criteria

- Toggling a rule's enabled checkbox stages the edit as before (already
  correct, unchanged).
- Refreshing the scheduler page while an edit is staged but not yet
  saved must leave "Save rules" enabled and "Discard changes" visible —
  not silently drop back to a "clean" state.
- No change to the already-correct behavior when there is no staged
  edit (fresh load, or after a save/discard) — `isDirty` stays `false`.

## Implementation Summary

- `src/web.rs`:
  - Added `config_staged: bool` to `StatusResponse`.
  - New `config_is_staged(preview_path, cache_path) -> bool`: reads
    `preview_config.json`; if it doesn't exist or can't be read, `false`.
    Otherwise compares its content against `config_cache.json`'s
    (`durable_state::read_cached`) — `true` if they differ (including
    when the cache doesn't exist yet, i.e. the very first staged edit
    with nothing committed to compare against), `false` if they match.
    Takes explicit paths rather than `&AppState` specifically so it's
    unit-testable without constructing a full `AppState` (camera/sync/
    scheduler actors), matching this file's existing `serve_asset`-style
    test pattern.
  - `status()` handler now calls `config_is_staged(&state.preview_config_path,
    &state.config_cache_path)` instead of a plain `.exists()` check.
- `src/web/scheduler.js` (`loadInitial()`): `isDirty` now set to
  `Boolean(status.config_staged)` instead of unconditionally `false`.

## Validation

- `cargo fmt --all`: applied, no outstanding diff.
- `cargo test --locked`: **78 passed; 0 failed; 0 ignored** (74 prior +
  4 new `config_is_staged_*` tests covering: no preview file, preview
  differs from cache, preview matches cache after being "resolved" the
  way `commit_config`/`discard_config` actually resolve it — by content
  convergence, not deletion — and preview exists with no cache yet).
- `cargo clippy --locked --all-targets -- -D warnings`: clean.
- Full Pi-native build/deploy (`./scripts/build-deploy-optic-daemon.sh`,
  no `--assets` — `StatusResponse`/`config_is_staged` are Rust changes):
  build succeeded, rollback-protected install succeeded, service came
  back up healthy at `optic-daemon 0.1.29` (no version bump this round).
- Real end-to-end verification against the live Pi (2026-09-20):
  - First pass (existence-based `config_staged`) immediately caught
    itself as wrong: `GET /api/status` showed `config_staged: true` on a
    daemon with nothing pending, because a *prior* session's staging
    calls had left `preview_config.json` on disk (as expected per the
    root cause above) — this is what led to the content-comparison
    rewrite before redeploying.
  - After the content-comparison fix and redeploy: confirmed
    `config_staged: false` at rest; staged a real rule-enabled toggle via
    `POST /api/schedule/preview` (same call the checkbox makes) →
    `config_staged: true` with `config` showing the toggled state; called
    `POST /api/config/discard` → `config_staged: false` again with rules
    back to their original state, confirming discard's copy-not-delete
    behavior is now correctly read as "resolved," not "still staged."

## Remaining Limitations / Follow-up

- No handler-level automated test for `status()` itself — it requires a
  fully constructed `AppState` (real camera/sync/scheduler actors),
  which this codebase's existing test suite doesn't build for
  handler-level tests, consistent with `commit_config`/`discard_config`/
  `schedule_preview` also having no handler-level unit tests today.
  `config_is_staged`, the actual logic this fix adds, is unit-tested
  directly; the handler wiring was verified live instead.
- `config_staged` is shared between the dashboard's camera-settings
  staging and the scheduler's rule staging (`preview_config.json` is one
  file for the whole `AppConfig`) — an unsaved camera-setting edit made
  on the dashboard will now also show "Save rules" as enabled on the
  scheduler page if the user navigates there without saving/discarding
  first. This is consistent with the fact that "Save rules" already
  commits the *entire* staged config (both sections share one
  commit/discard pair), not a scheduler-only save — not a new gap
  introduced by this fix, just newly visible now that the indicator is
  accurate. Not addressed here since it reflects the existing shared
  staging design, not a defect in it.
- The dashboard's own Save/Discard buttons (`src/web/app.js`) use a
  different, client-only dirty-check (`baselineSettings` compared against
  live form values, seeded from hardcoded `defaults` at script load, not
  from `status.config_staged`) — untouched by this fix, and out of scope
  for the rule-editor bug reported here. Worth a follow-up look since it
  likely has the same "resets to clean on refresh" gap for a staged-but-
  uncommitted camera setting.
