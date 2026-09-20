# Dated Worklog: 2026-09-19 - Timelapse Scheduler, Phase 1d (Rules Editor UI) — Phase 1 Complete

Status: **implemented, deployed, and verified on real hardware** (API
level — see Limitations for what "verified" does and doesn't cover for a
browser UI). This closes out Phase 1 as scoped across
`2026-09-19-timelapse-scheduler-phase1{a,b,c}.md`: composable rules,
reboot-durable run state, a live actor firing real captures, and now a
dedicated page to manage it all without hand-writing JSON over SSH.

## Objective

Per the phase-1a worklog's scoping: the dedicated `/scheduler.html` page
(rules editor + Shot Forecaster) that `docs/optic-daemon-scheduler.md`
§8/§10 designed — the only remaining piece of Phase 1. Everything in 1c
was exercised by writing JSON directly over SSH; this is what replaces
that with a real UI, through the same backend endpoints (mostly — see
Implementation Summary for the two new ones this needed).

## Acceptance Criteria

- A real add/edit/remove rules editor: label, unique slug (client- and
  server-validated), trigger type (fixed interval or recurring time of
  day with weekday selection), and an optional daily time-window
  constraint.
- Edits are visible in a live Shot Forecaster (next 48h, frame count,
  average interval) *before* committing — design doc §8's explicit
  requirement — not just after saving.
- Save/Discard work through the existing config commit/discard flow, not
  a parallel mechanism.
- Pause/Resume reachable from the new page, and a lightweight always-
  visible summary (state + next capture + link) on the main dashboard —
  design doc §10.
- No regression to the deploy pipeline's own verification of what it just
  shipped (Biome lint, served-content checks) — extended to cover the
  two new files, per §10's explicitly flagged follow-up from the design
  doc, not left as a gap.

## Test Plan (written before implementation)

- No new Rust unit tests expected for this slice specifically (it's
  primarily two new small endpoints plus frontend) — coverage comes from
  exercising the real HTTP surface directly, the same way 1b/1c's local
  smoke tests did, plus the existing `optic_scheduler`/`web` test suites
  continuing to pass unmodified.
- Local smoke tests (`cargo run`, real HTTP calls) for the two new
  endpoints: `POST /api/schedule/preview` (stages rules without touching
  the camera; rejects invalid/duplicate slugs immediately) and
  `GET /api/schedule/forecast` (reflects staged, not just committed,
  rules).
- End-to-end: stage a rule with a weekday + time-of-day trigger and a
  time-window constraint through the real API (as the UI would), confirm
  the forecast reflects it correctly, commit, confirm status shows it
  committed, stage a destructive change, discard, confirm the original
  committed rule is back — not emptied.
- Pi-native build/deploy, matching every other change this session,
  including the deploy script's own extended content checks for the new
  page/script.

## Findings While Implementing

- **A second real, serious latent bug, caught the same way as 1c's two
  races — by actually exercising the wire format, not just the in-memory
  Rust types.** `RecurringDays::Weekdays(Vec<Weekday>)` is a newtype
  variant wrapping a sequence. Under the pure internal tagging
  (`#[serde(tag = "kind")]`) used since Phase 1a, serde **cannot
  represent that at all** — it panics at serialize time:
  `cannot serialize tagged newtype variant RecurringDays::Weekdays
  containing a sequence`. Every Phase 1a test exercised `RecurringDays`
  only in-memory (constructed directly in Rust, passed straight to
  `occurrences()`) — none of them ever round-tripped it through
  `serde_json`, so this shipped silently in 1a/1b/1c without ever
  failing a test. It would have surfaced the moment any real rule using
  weekday selection was saved. Caught by hand-verifying the exact wire
  shape before writing the frontend against it (`cargo test -- --nocapture`
  against small scratch snippets — removed before committing, kept only
  the two permanent regression tests below). Fixed with adjacent tagging
  (`tag = "kind", content = "value"`) for `RecurringDays` specifically —
  `Trigger`/`Constraint` stay internally-tagged, since their variants are
  all struct-shaped (named fields merge into the tagged object fine;
  only a newtype-wrapping-a-sequence breaks). Added two permanent tests
  (`recurring_days_round_trips_through_json_including_weekdays`,
  `rule_with_weekdays_time_window_round_trips_through_json`) specifically
  so this can't silently regress again.
- **No existing endpoint could stage schedule-only edits without also
  touching the camera.** `reconfigure_stream`/`start_stream` are tied to
  `state.camera.reconfigure_stream(...)`/`start_stream(...)` — using them
  to stage rule changes would have reconfigured the live camera pipeline
  as a side effect of editing a schedule, for no reason. Added
  `POST /api/schedule/preview` instead: reads the current staged-or-
  committed config (reusing 1c's `current_app_config` helper), overrides
  only `schedule.{station,rules}`, writes to `preview_config.json` —
  never calls into `state.camera` at all.
- **The Forecaster's "reflects unsaved edits" promise (design doc §8)
  needed the frontend to eagerly stage on every UI change**, not just on
  an explicit save — otherwise "forecast" would only ever show the last
  *committed* state. `scheduler.js` calls `/api/schedule/preview` after
  every add/edit/remove/toggle, then immediately re-fetches
  `/api/schedule/forecast` — so "Save" reduces to just
  `POST /api/config/commit` (the preview is already current), and
  "Discard" is exactly `POST /api/config/discard` followed by reloading
  from `/api/status` (which now reflects the reverted committed state).

## Implementation Summary

- `src/optic_scheduler.rs`: `RecurringDays` re-tagged (see Findings); new
  `pub fn forecast(schedule, from_utc, horizon)` — a UTC-friendly wrapper
  around `occurrences()` so callers outside this module (the new
  endpoint) don't need to know about `chrono_tz::Tz`. Two new permanent
  regression tests.
- `src/web.rs`: two new endpoints — `POST /api/schedule/preview`
  (`SchedulePreviewRequest{station, rules}`, validates slugs, writes to
  `preview_config.json`, never touches the camera) and
  `GET /api/schedule/forecast?hours=N` (default 48, clamped to a week;
  reads whatever is currently staged, calls `optic_scheduler::forecast`).
- `src/web/scheduler.html` (new): dedicated page — run control (state,
  next capture, pause/resume), rules list with enable-toggle/edit/remove,
  an add/edit form (trigger-type-aware field visibility, weekday
  checkboxes, optional time-window fields), and the Shot Forecaster table
  + stats.
- `src/web/scheduler.js` (new): all of the above's logic — loads current
  state from `/api/status`, stages on every edit, renders the live
  forecast, save/discard through the existing config endpoints,
  pause/resume through 1c's endpoints. Client-side slug format/duplicate
  validation mirrors the backend's, for immediate feedback per design
  doc §3.1.
- `src/web/index.html`/`app.js`: new lightweight "Scheduler" summary card
  on the main dashboard (state pill, next capture, rule count, a link to
  `/scheduler.html`) — design doc §10's explicit requirement, wired into
  the existing `refreshStatus()` poll rather than a separate one.
- `src/web/styles.css`: table/form/rule-row styles for the new page,
  reusing the existing color/spacing variables rather than introducing a
  second stylesheet.
- `scripts/build-deploy-optic-daemon.sh`: Biome lint list and the
  deploy's own served-content verification both extended to cover
  `scheduler.html`/`scheduler.js` — closing the exact follow-up the
  design doc's §10 flagged in advance ("two concrete follow-ups... easy
  to miss because they're not in the obvious place").
- `Cargo.toml`/`Cargo.lock`: version bump `0.1.28` → `0.1.29`.

## Validation

- **macOS dev target:** `cargo fmt --all -- --check`, `cargo test
  --locked --all-targets` (67/67 passed, including the 2 new
  `RecurringDays` regression tests), `cargo clippy --locked --all-targets
  -- -D warnings` — all clean. `npx @biomejs/biome check` on all four web
  files (clean after one auto-fixed formatting nit).
- **Local smoke test** (`cargo run --bin optic-daemon`, real HTTP calls):
  confirmed `scheduler.html`/`scheduler.js` serve correctly; staged a
  `RecurringTime` rule with `Weekdays` + a `TimeWindow` constraint
  through `/api/schedule/preview` (the exact case that would have hit
  the serialization panic pre-fix); `/api/schedule/forecast?hours=168`
  correctly produced weekday-only, time-window-respecting, timezone-
  correct (America/Vancouver, verified the UTC offset landed right)
  occurrences reflecting the *staged* (uncommitted) rule; committed;
  confirmed `/api/status` showed it committed; staged a destructive
  "clear all rules" change, discarded, confirmed the original committed
  rule came back rather than an empty list.
- **Pi-native build/deploy:** 70/70 tests passed, clippy clean, release
  build/install/rollback checks all passed — including the deploy
  script's newly-added content checks for the two new files, which would
  have triggered a rollback had they failed and didn't.
- **Real hardware, post-deploy:** confirmed over the network (not just
  locally) that `/`, `/scheduler.html`, and `/scheduler.js` all serve
  correctly from the live daemon; `/api/status` carries the new fields;
  `/api/schedule/forecast` responds correctly (empty, matching the clean
  state 1c's worklog left the device in).

## Addendum (same day): Real Browser Bug Found on User Review, Fixed

Confirmed the limitation below immediately mattered: the user reviewed
the actual page in a real browser and found that switching the trigger-
type dropdown didn't visually hide/show the type-specific fields at all
(e.g. "Every (seconds)" stayed visible after choosing "Recurring time of
day"). Root cause was a classic, well-documented CSS gotcha, not a JS
logic bug: `.rule-form label { display: flex; ... }` (specificity: one
class + one type) is *more specific* than the browser's own built-in
`[hidden] { display: none; }` rule (one attribute selector), so the
`hidden` DOM property JS was already toggling correctly had no visual
effect — the author stylesheet's `display: flex` always won. `.rule-form`
itself had the same issue via an equal-specificity tie broken in the
author stylesheet's favor. Notably, this exact class of bug had already
been hit and correctly fixed once before in this codebase
(`.preview-frame img[hidden], .preview-placeholder[hidden] { display:
none; }`) — just not carried over to the new form. Fixed the same way:
`.rule-form[hidden], .rule-form label[hidden] { display: none; }`,
verified as strictly more specific by CSS specificity rules (not merely
by a browser check — a headless jsdom check turned out not to reliably
reproduce this exact cascade nuance, so the fix's correctness rests on
the specificity math, which is unambiguous, not on that tool). Deployed
and confirmed the fixed CSS is being served live.

## Remaining Limitations / Follow-up

- **Still not visually verified in an actual browser end-to-end** beyond
  the one specific issue above, which the user found and this addendum
  fixed. Everything else in this worklog is real HTTP-level verification
  against served files and backend responses, not a rendered-page check.
  Worth continuing to look at the actual UI for anything else in this
  category (layout, readability, other `hidden`-toggled elements this
  same bug class could theoretically affect elsewhere).
- ~~Time-window constraint's own `days` field is hardcoded to `Every` in
  the UI~~ — **fixed same day, on user review.** The window now has its
  own independent day-selector (every day / specific weekdays), separate
  from the trigger's own days field — this is what actually makes a
  Fixed Interval rule restrictable to weekdays at all, since Interval
  triggers have no days concept of their own; only the window constraint
  does. Verified end-to-end: staged an `Interval{1800s}` rule with a
  `TimeWindow{Weekdays(Mon-Fri), 08:00-16:00}` constraint and confirmed
  the 168h forecast contained zero Saturday/Sunday occurrences. Also
  added the window's day restriction to the rule-list summary line
  (previously only the trigger was described, silently omitting a
  weekday-restricted window from view).
- **No station lat/long/elevation form** — `station` is currently
  carried through as whatever was already staged/committed (or `null`);
  the UI has no fields to actually set it yet. Irrelevant until Phase 2
  (Solar) actually needs it, per the original phasing decision, but
  worth noting since a user trying to set a timezone for `RecurringTime`
  today has no UI path to do so (would need direct API/JSON, same as
  this worklog's own testing did).
- **Phase 1 is now complete** per the phasing recorded in
  `docs/optic-daemon-scheduler.md`'s status header. Phase 2 (Solar) is
  next whenever wanted, per that doc's own sequencing.
