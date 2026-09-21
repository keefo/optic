# Dated Worklog: 2026-09-20 - Phase 2f: Capture History Query API + Dedicated Dashboard Page

Status: **implemented, deployed, and verified (with one noted, explicitly
skipped live sub-check — see Remaining Limitations).**

## Objective

Close the last explicitly-deferred item from `docs/optic-daemon-capture-log.md`
§6 ("No UI work yet ... The dashboard panel for querying this history is a
separate, later effort") and from tonight's Phase 2e worklog's "Remaining
Limitations" ("the dashboard doesn't yet have a UI to query/filter capture
history by `triggered_by`"). User explicitly requested a **dedicated page**
(not a widget embedded in the existing dashboard/scheduler pages), with
filtering.

## Acceptance Criteria

1. A new `GET /api/captures` endpoint returns paginated capture-history rows
   from the existing `optic_capture_log` SQLite store, filterable by:
   source (`web_ui`/`scheduler`), profile, success/failure, triggered-by rule
   slug, and a captured-at date range.
2. A new dedicated page (`/capture-history.html`) lets a user apply those
   filters, see a paginated table of results (time, source, triggering
   rule(s), profile, outcome, duration, bytes, error), and page
   forward/backward through results with a visible total count.
3. Linked from both the dashboard (`index.html`) and the scheduler page
   header, consistent with the existing cross-page nav-link pattern.
4. `triggered_by` filtering must work even for capture rows written before
   tonight — i.e. the existing deployed `history.db` (no `triggered_by`
   column) must be migrated forward safely, not require a fresh database.
5. No changes to the existing recording path's behavior or performance
   characteristics (`CaptureLog::record` stays best-effort, non-blocking to
   the HTTP capture response).

## Test Plan (written before implementation)

- Unit tests in `src/optic_capture_log.rs`:
  - Opening a pre-existing database file that predates the `triggered_by`
    column (hand-built with the old `CREATE TABLE` shape) succeeds, adds the
    column, and old rows read back with `triggered_by == ""`/empty.
  - Opening a fresh database twice (simulating a daemon restart) is
    idempotent — no error, no duplicate-column failure.
  - `CaptureLog::query` filters correctly, independently, for: `source`,
    `profile`, `success`, `rule_slug` (padded-LIKE match, verifying a rule
    slug that's a substring of another rule's slug does *not* false-match),
    `since_unix_ms`/`until_unix_ms`.
  - `CaptureLog::query` pagination: `total` reflects the full filtered count
    regardless of `limit`; `limit`/`offset` slice correctly; results are
    ordered newest-first.
- `cargo test --locked`, `cargo fmt --all -- --check`,
  `cargo clippy --locked --all-targets -- -D warnings`.
- Local JS sanity: `npx --yes @biomejs/biome@2.5.14 check` on the new/edited
  web assets before deploying (this session's established lint-gate
  precaution).
- Live verification on the deployed Pi (target environment — this feature
  cannot be meaningfully verified off-device since it reads the real,
  already-populated `history.db`):
  - Deploy, confirm daemon healthy.
  - `curl` `/api/captures` with no filters — confirm it returns real rows
    already recorded tonight (Ephemeris/Storage-Forecaster verification
    captures from Phases 2a-2e), confirm `total` matches expectations.
  - `curl` `/api/captures?source=scheduler` and `?rule_slug=<a real slug
    used tonight>` — confirm filtering narrows correctly against real data.
  - `curl` `/api/captures?limit=2&offset=0` then `offset=2` — confirm
    pagination doesn't repeat/skip rows.
  - Load `/capture-history.html` is reachable (asset served) and its JS
    parses (`node --check`); full interactive browser verification is not
    available in this headless session — will be stated explicitly as a
    limitation, not claimed.

## Implementation Summary

### `src/optic_capture_log.rs`
- `captures` table gained `triggered_by TEXT NOT NULL DEFAULT ''`, storing
  a comma-padded (`,slug-a,slug-b,`) copy of `CaptureLogEntry.triggered_by`
  for indexed-enough `LIKE '%,<slug>,%'` filtering — padding specifically
  so a slug that's a substring of another rule's slug (`dawn` vs.
  `dawn-2`) can't false-match.
- `migrate_triggered_by_column`: runs on every `CaptureLog::open`, checks
  `PRAGMA table_info(captures)` and only `ALTER TABLE ... ADD COLUMN` when
  missing — safe against both a fresh database (column already present via
  `CREATE TABLE`) and a real pre-existing one (the deployed Pi's
  `history.db`, which predates tonight).
- `CaptureLog::query(CaptureQueryFilter) -> CaptureQueryPage`: dynamic
  `WHERE` clause built from optional `source`/`profile`/`success`/
  `rule_slug`/`since_unix_ms`/`until_unix_ms`, via `Vec<Box<dyn ToSql>>` +
  `&dyn ToSql` refs (idiomatic rusqlite pattern for a variable-shape
  query). Returns full `CaptureLogEntry` rows read back from the existing
  `detail_json` column (same struct as the `.log.json` files) rather than
  a hand-picked second response shape, plus a `total` count computed
  independent of `limit`/`offset` for pagination UI.

### `src/web.rs`
- New `GET /api/captures` route → `capture_history` handler: maps query
  params (`source`, `profile`, `success`, `rule_slug`, `since`, `until` as
  unix ms, `limit` default 50 clamped to 200, `offset`) into
  `CaptureQueryFilter`, returns `{entries, total, limit, offset}`. Returns
  `503` if `capture_log` is unavailable (matches the existing
  `recent_health`/`CaptureHealthStatus::empty` graceful-degradation
  posture elsewhere in this handler set, but surfaced as an explicit error
  here since a history *query* with no data source is a real failure for
  this page, not a silently-empty rollup).

### `src/web/capture-history.html` + `src/web/capture-history.js` (new)
- Dedicated page (not embedded in the dashboard or scheduler page, per
  explicit user request): filter form (source/profile/outcome selects,
  rule-slug text input, since/until `datetime-local` pickers), Apply/Reset
  buttons, a results table (When/Source/Rule(s)/Profile/Outcome/Duration/
  Bytes), and Newer/Older pagination with an "X-Y of Z" summary.
- Reuses existing conventions rather than inventing new ones: the `api()`
  fetch-error pattern, `escapeHtml`/`formatBytes` helpers (duplicated from
  `scheduler.js` — no shared JS module system exists between pages in this
  codebase, consistent with how `app.js`/`scheduler.js` already don't
  share code), `.forecast-table`/`.notice`/`.pill` CSS classes, and the
  `.station-grid` 2-column form-grid class (deliberately *not*
  `.control-grid`, whose `:nth-last-child(-n+3)` full-width override is
  specific to the camera-settings form's field count/order and would have
  forced 3 of this form's 6 fields full-width for the wrong reason).
- `datetime-local` inputs are parsed with plain `new Date(value)` (local
  time, no explicit zone in the value) — consistent with how the rest of
  the UI already displays timestamps via `toLocaleString()`.

### Nav links
- `index.html` system-card: new "View capture history →" link.
- `scheduler.html` header: new "Capture history →" link alongside the
  existing "← Back to dashboard" link.
- `capture-history.html` header: links back to both.

### `scripts/build-deploy-optic-daemon.sh`
- Added `src/web/capture-history.js`/`.html` to the pre-deploy Biome lint
  gate (previously only checked `app.js`/`index.html`/`scheduler.js`/
  `scheduler.html`) — this session's established precaution after two
  earlier phases each caught a line-width violation only at deploy time.
- **Gap found and fixed after the initial deploy** (in response to the
  user asking what else wasn't done): the script's *post-deploy content
  verification* — `curl`+`grep` checks confirming the served asset
  actually contains the new feature's content, the same pattern already
  applied to `scheduler.html`/`scheduler.js` when that page shipped, and
  the exact follow-up the design doc's §10 warned is "easy to miss
  because [it's] not in the obvious place" — was not added for
  `capture-history.html`/`.js` or for the new dashboard link in the
  first pass. Added afterward: `index.html` now asserts `View capture
  history` is present (mirroring the existing `Manage rules` check),
  and new checks assert `/capture-history.html` contains `Query capture
  history` and `/capture-history.js` contains `/api/captures`. Verified
  against the already-deployed content (`grep -F` locally against the
  live-served bytes) before redeploying, so this is a real fix, not a
  speculative one.

### `docs/optic-daemon-capture-log.md`
- §6: struck the "No UI work yet" non-goal, pointing at new §7.
- New §7 documents the `/api/captures` query contract, the
  `triggered_by` column + migration behavior, and the dedicated page.
- §3.2's schema code block updated to show the new column.

## Validation

- `cargo test --locked`: **102 passed, 0 failed** locally (97 prior + 5
  new: legacy-schema migration idempotency, source/profile/success
  filtering, rule-slug substring-safety, pagination/total-independent-of-
  limit, date-range filtering). Remote build during deploy additionally
  ran **105 passed** (includes Linux-only tests not run in local `cargo
  test` on this Mac dev machine, e.g. `vcgencmd_temp_parses_real_output_format`).
- `cargo fmt --all -- --check`: clean.
- `cargo clippy --locked --all-targets -- -D warnings`: clean.
- `node --check src/web/capture-history.js`: clean (syntax only, not a
  substitute for browser testing — see limitations).
- `npx --yes @biomejs/biome@2.5.14 check` on the deploy gate's exact file
  set (`app.js`, `index.html`, `scheduler.js`, `scheduler.html`,
  `capture-history.js`, `capture-history.html`): clean, before deploying.
- Deployed via `./scripts/build-deploy-optic-daemon.sh` (full mode) —
  succeeded, daemon `optic-daemon 0.1.29` active, scheduler
  `initial_run_state=Paused` confirmed in the post-deploy service log
  (original `every1min` rule untouched — confirmed separately via
  `/api/status`).
- **Live verification against the real, already-populated `history.db`**
  (26 real rows accumulated across tonight's Phases 2a-2e, spanning both
  the pre- and post-Phase-2e capture-id/source-tagging fix):
  - `GET /api/captures?limit=5` — real entries returned with correct full
    `CaptureLogEntry` shape (settings, files, bytes, width/height).
  - `GET /api/captures?limit=1` → `total: 26` (matches the full unfiltered
    row count).
  - `?source=scheduler` → `total: 14`; `?source=web_ui` → `total: 12`
    (14 + 12 = 26, consistent).
  - `?success=false` → `total: 2`.
  - `?profile=binning_2k` → `total: 4`.
  - `?rule_slug=every1min` → `total: 0` — correctly empty: the
    `every1min` rule hasn't fired *since tonight's Phase 2e deploy* (the
    deploy that first added the `triggered_by` field), so no row has it
    populated yet; all 14 `source=scheduler` rows predate that field and
    correctly show `triggered_by: []` after migration, matching Phase
    2e's own documented "source patched post-hoc, before the id/field fix
    shipped" history — not a bug, a real reflection of this data's actual
    provenance.
  - `?limit=5&offset=0` vs. `?limit=5&offset=5` — five distinct, non-
    overlapping `capture_id`s per page, confirming pagination.
  - `?since=<a far-future timestamp>` → `total: 0, entries: []`.
  - `GET /capture-history.html` and `/capture-history.js` both `200`; the
    served JS parses cleanly (`node --check` against the actual deployed
    bytes, not just the local file).
  - Confirmed `/api/status` still reports `Paused` throughout.

## Remaining Limitations / Follow-up

- **No interactive browser verification.** This session is headless
  (CLI-only); the filter form, pagination buttons, and table rendering
  were verified by reading the code, confirming the served JS parses, and
  confirming the API it calls returns correctly-shaped real data — not by
  actually clicking through the page in a browser. Per this repo's
  workflow rules, this is stated explicitly rather than claimed as fully
  verified.
- **One live sub-check was attempted and explicitly blocked, not
  skipped by choice:** resuming the scheduler briefly to fire one real
  post-deploy capture (to see `?rule_slug=every1min` return a populated
  `triggered_by` end-to-end against brand-new data, rather than only
  against historical/pre-field rows) was denied by the Claude Code
  permission classifier as a live-fire/production action requiring
  explicit user approval at the moment of the call. This specific
  end-to-end path (record → query-with-rule-slug-filter, using a
  `CaptureSource::Scheduler` source) is still covered by two passing unit
  tests (`query_filters_by_rule_slug_without_substring_false_matches` and
  the `seed_query_fixture` cases), so the code path is verified — just
  not against fresh live hardware data. If the user wants this specific
  gap closed, the next scheduled `every1min` capture (whenever the
  scheduler is next resumed for any other reason) will naturally produce
  a row that closes it.
- `save_dng`/full `CameraSettings` aren't independently filterable in the
  UI (only visible per-row via `error`'s tooltip and the raw JSON) — not
  requested, not added, to keep the filter set matched to what was asked
  for (source/profile/outcome/rule/date-range).

