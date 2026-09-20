# Dated Worklog: 2026-09-20 - Rule Editor Polish: Stale Preview Bug, Notice Placement, Unified Run Toggle, Save/Discard Visibility

Status: **implemented, deployed, and verified** (see Limitations for what
"verified" does and doesn't cover without a real rendered browser).

## Objective

Four issues found by the user exercising the real `/scheduler.html` UI in
a browser, addressed together since they're all small, related fixes to
the same page found in one continuous testing session:

1. **Real bug**: unchecking a rule (without clicking Save) and refreshing
   `/scheduler.html` correctly showed the unsaved unchecked state — but
   navigating to the dashboard and back silently reverted it to checked,
   discarding the unsaved edit without any explicit discard action.
2. Rule-form validation errors (e.g. "Slug ... is already used by another
   rule.") appeared in the Run Control section's shared notice instead of
   next to the rule editor form they're actually about.
3. Separate "Pause"/"Resume" buttons (one always disabled) should be one
   button whose label/action flips with current state.
4. "Save rules" should come before "Discard changes", "Save rules" should
   be disabled when there's nothing staged, and "Discard changes" should
   be hidden entirely when there's nothing to discard.

## Findings While Implementing (Item 1 — the real bug)

- **Root-caused to `stop_stream` (`src/web.rs`)**, not anything in the
  scheduler code itself: it unconditionally deleted `preview_config.json`
  whenever the camera preview stream stopped —
  `let _ = tokio::fs::remove_file(&*state.preview_config_path).await;`.
  `app.js`'s `pagehide` listener calls `/api/stream/stop` via
  `sendBeacon` on **any** navigation away from the dashboard, including a
  plain refresh — not just an explicit "stop preview" action.
- `preview_config.json` is no longer just camera-preview scratch space —
  design doc §9 deliberately extended it to be the general staging file
  for the whole `AppConfig`, including schedule rules, specifically so
  rule edits "reuse the existing preview/commit/discard config flow."
  `stop_stream`'s delete predates that decision and was never revisited
  when it landed — it was silently discarding *any* unsaved edit (not
  just schedule rules; camera profile/settings edits too) the moment the
  live preview merely stopped, even though nothing else in the file ever
  resolves staged config except an explicit `/api/config/commit` or
  `/api/config/discard`. Confirmed by reading `commit_config`/
  `discard_config`: neither deletes the file outright — both treat it as
  something to overwrite-in-sync-with, never blow away as a side effect
  of an unrelated action.
- Exact reproduction traced end to end: uncheck rule → staged edit written
  to `preview_config.json` → navigate to dashboard → dashboard's own
  auto-start-preview flow (`start_stream`) reads the *existing* preview
  file and only overrides `profile`/`settings`, correctly preserving the
  staged schedule at this point → navigate back to `/scheduler.html` →
  **that page-leave** fires `pagehide` on the dashboard → `stop_stream` →
  preview file deleted → `/scheduler.html` loads → `current_app_config()`
  finds no preview file → falls back to the durable (committed) config →
  the unchecked rule appears checked again, edit silently gone.

## Acceptance Criteria

- `stop_stream` no longer deletes `preview_config.json`; staged edits
  (schedule or camera) survive navigating anywhere, including repeated
  dashboard visits, until an explicit Save or Discard.
- Rule-form validation messages render inside `#rule-editor-section`
  (next to the form's own Save/Cancel), not in Run Control's shared
  notice.
- One `#run-toggle-btn` replaces `#pause-btn`/`#resume-btn`; label reads
  "Pause" when running (posts `/api/schedule/pause`) and "Resume" when
  paused (posts `/api/schedule/resume`).
- "Save rules" precedes "Discard changes" in the DOM (and therefore
  visually, `.button-row` is a plain flex row). "Save rules" is
  `disabled` and "Discard changes" is `hidden` whenever there is no
  staged edit (fresh load, right after a save, right after a discard);
  both flip to their active state the moment any rule is added, edited,
  removed, or toggled.

## Implementation Summary

- `src/web.rs`: removed the `remove_file` call from `stop_stream`,
  replaced the stale comment with one explaining why it's deliberately
  gone (see Findings) so this doesn't get silently reintroduced by a
  future refactor that doesn't know the history.
- `src/web/scheduler.html`:
  - Added `<p id="rule-form-notice" class="notice" role="status">`
    inside `#rule-editor-section`, just above its button row.
  - Replaced `#pause-btn`/`#resume-btn` with a single `#run-toggle-btn`.
  - Reordered `#save-rules` before `#discard-rules`; added `disabled` to
    `#save-rules` and `hidden` to `#discard-rules` as their default
    (pre-JS) markup state, matching the same "clean by default" starting
    point the JS enforces once it runs.
- `src/web/scheduler.js`:
  - New `showRuleFormNotice`/`clearRuleFormNotice` helpers; the three
    rule-form validation call sites (missing fields, bad slug format,
    duplicate slug) now use them instead of the page-level `showNotice`.
    Cleared on form open, form close, and successful submit.
  - `renderRunState` now drives one button's `textContent`/`className`/
    `dataset.action` instead of toggling `disabled` on two separate
    buttons; one click handler reads `dataset.action` to decide which
    endpoint to call.
  - New `isDirty` flag + `renderSaveDiscardButtons()`. Set `true` at the
    top of `stageAndRefreshForecast()` (the one function every rule
    add/edit/remove/toggle already funnels through) and reset to `false`
    on a fresh `loadInitial()` load and after a successful save.
    (Discard already calls `loadInitial()`, which resets it too — no
    separate handling needed there.)

## Validation

- `cargo fmt --all -- --check`, `cargo test --locked --all-targets`
  (68/68 passed — no test specifically covered `stop_stream`'s file
  behavior before or after, see Limitations), `cargo clippy --locked
  --all-targets -- -D warnings` — all clean.
- Grepped every `#id` `scheduler.js` references against the new
  `scheduler.html` — zero missing.
- `npx @biomejs/biome check` on all four web files — clean.
- Pi-native full build/deploy (`./scripts/build-deploy-optic-daemon.sh`,
  full path since `src/web.rs` changed) — no rollback.
- **Real end-to-end reproduction of the original bug, before and after**,
  against the live daemon: staged a rule (`enabled: false`) via
  `/api/schedule/preview`, confirmed it staged; called `/api/stream/stop`
  directly (the exact call `pagehide`'s `sendBeacon` makes); confirmed via
  `/api/status` that the rule was **still** `enabled: false` afterward —
  the staged edit survived, where before this fix it would have reverted
  to the committed (`enabled: true`) state. Discarded the test rule
  afterward and confirmed `/api/status` correctly showed the real
  committed rule (`everymin`, the user's own), not empty — proof discard
  reverts to genuine committed state, not a blank slate.
- Confirmed via `curl` against the live daemon that `scheduler.html`
  serves `#rule-form-notice` inside `#rule-editor-section`, the single
  `#run-toggle-btn` (replacing the two old buttons), and `#save-rules`
  before `#discard-rules` with their correct default `disabled`/`hidden`
  attributes.

## Incidental Finding: This Deploy Also Shipped an Unrelated, Previously-Held-Back Change

The full deploy for this worklog's fixes also picked up
`systemd/optic-daemon.service`'s `OPTIC_SYNC_REMOTE_HOST=imacpro.local`
change from the same day's earlier, *separate* iMac-IP investigation
(`worklogs/2026-09-20-stale-iMac-ip-beszel-and-sync.md`) — that change was
sitting uncommitted in the working tree, explicitly not yet deployed
pending a known_hosts prerequisite that got blocked by the permission
system. A full deploy installs the entire tracked unit file regardless of
which specific line motivated running it, so it went out anyway. Confirmed
live: `ssh liam@optic.local "grep OPTIC_SYNC_REMOTE_HOST ...`" now shows
`imacpro.local`. Sync is currently `idle`/`queued_files: 0` (no active
connection attempted yet, so no failure has surfaced), but the known
known_hosts mismatch from that other worklog still applies — this needs
the same follow-up action (add the `[imacpro.local]:2222` known_hosts
line) before `optic_sync` next has something real to transfer, or it will
fail host-key verification. See that worklog for the exact command.

## Remaining Limitations / Follow-up

- **Not visually verified in an actual rendered browser** — same standing
  limitation as every frontend change this session. The dirty-state
  disable/hide logic and the unified toggle button's label-flip are
  logically verified (code inspection + the underlying API calls
  exercised directly) but not seen rendered.
- No dedicated Rust unit test added for `stop_stream`'s file behavior
  specifically — the existing test module has no lightweight way to
  construct a full `AppState` (camera actor construction is Linux/
  libcamera-gated the same way `optic_scheduler`'s actor tests are).
  Relied on direct live-endpoint verification instead (see Validation),
  which exercises the real file I/O rather than a mock.
