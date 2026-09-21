# Dated Worklog: 2026-09-20 - Scheduler Phase 2e: Capture-Log Source Fix, `triggered_by`, and Design-Doc Resolution

Status: **implemented, deployed, and verified.**

## Objective

Two small, explicitly-deferred follow-ups from the earlier
`2026-09-20-scheduler-capture-filename-tagging.md` worklog, plus closing
out the design doc's now-almost-entirely-resolved §14 open-decisions
list:

1. `optic_capture_log.rs::failure_capture_id` always said `testshot-`
   for a *failed* scheduler capture's synthetic log-only id, regardless
   of actual source.
2. `CaptureLogEntry.source` was a plain string with no structured record
   of which rule(s) triggered a scheduler capture.
3. `docs/optic-daemon-scheduler.md`'s top-of-file status and §14 still
   read as an in-progress design proposal despite Phase 1 and (as of
   tonight) most of Phase 2 being fully implemented and deployed.

## Implementation Summary

### `src/optic_capture_log.rs`

- Root cause of item 1: `CaptureLogEntry::new()` always hardcoded
  `source: "web_ui".to_owned()` internally and computed `capture_id`
  (via `derive_capture_id`/`failure_capture_id`) *before* the one caller
  that needed something different (`optic_scheduler.rs`'s `fire_capture`)
  patched `entry.source` afterward — too late for the id itself to ever
  reflect it.
- Fixed by threading the real `CaptureSource` (already known at both
  call sites — `web.rs`'s `capture()` has it on the incoming
  `CaptureRequest`, `optic_scheduler.rs`'s `fire_capture` constructs it
  directly) into `CaptureLogEntry::new()` as a new parameter, used by
  both `derive_capture_id`/`failure_capture_id` (for the id prefix) and
  to set `source`/the new `triggered_by` field correctly from the start
  — no more post-hoc patching.
- `CaptureLogEntry` gained `#[serde(default)] triggered_by: Vec<String>`
  — empty for `WebUi`, the firing rule slugs for `Scheduler`. No
  database schema migration needed: the whole entry is persisted
  verbatim as the `detail_json` column (per this module's own doc
  comment), so a new struct field just flows into that JSON blob.
  `#[serde(default)]` specifically so a `.log.json` file or database row
  written before this field existed still deserializes (as empty)
  rather than failing — matters because these files/rows are genuinely
  durable and outlive any given daemon version.
- `web.rs`/`optic_scheduler.rs` call sites: both now extract
  `request.source.clone()` *before* `request` is moved into
  `capture_to_stage`, and pass it through — `optic_scheduler.rs` no
  longer needs the post-hoc `entry.source = "scheduler".to_owned()`
  line at all, removed.

### `docs/optic-daemon-scheduler.md`

- Top-of-file `> Status:` line: "design proposal, not implemented" →
  reflects Phase 1 (shipped 2026-09-19) and Phase 2 (shipped
  2026-09-20), pointing at the dated worklogs for the actual record.
- §2's `Constraints` data-model prose: "`time_window` is the only field
  implemented today... the rest are Phase 2+" → all five fields are now
  implemented, dated accordingly.
- §14: added six new **Resolved** entries (merge-window duration bumped
  to 10s with the measured-data rationale; Station config as a plain
  manual dashboard form; the `astro` crate decision and why it
  eliminated the previously-separate lunar-crate research item;
  `Trigger::Ephemeris` fully implemented and live-verified; the Storage
  Forecaster; the dead-rule/overlap advisories) and updated the
  `testshot-`-prefix entry's "Not done" list to "now also done" for both
  previously-deferred items — turning §14 from a genuine open-questions
  list into what it now actually is, a resolved-decisions log with
  pointers to the worklog that implemented and verified each one.

## Validation

- `cargo fmt --all -- --check` / `cargo test --locked`: **97 passed; 0
  failed; 0 ignored** (95 prior + 2 new: a scheduler-sourced
  `CaptureLogEntry` records the right `source`/`triggered_by`, and
  `derive_capture_id` mints a `scheduler-`-prefixed id on a scheduler
  failure specifically, alongside the existing `testshot-`-on-`WebUi`-
  failure test kept and renamed for clarity now that there are two
  cases instead of one).
- `cargo clippy --locked --all-targets -- -D warnings`: clean.
- Deployed via full `./scripts/build-deploy-optic-daemon.sh`.
- **Live verification**: rather than actually failing a real capture on
  hardware (there's no clean way to force `CameraError` on demand
  without genuinely breaking something), verification here relies on
  the unit tests exercising the exact code path
  (`derive_capture_id`/`failure_capture_id` with a real
  `CaptureSource::Scheduler` and a real `Err(CameraError)`) plus reading
  the deployed binary's behavior is unchanged for the success path
  (confirmed via this session's many successful real scheduler-fired
  captures in Phases 2a/2b/2c/2d, which already exercised
  `CaptureLogEntry::new` with a real `CaptureSource::Scheduler` on
  *success* — the `triggered_by` field for those is confirmed correct
  by the same unit test covering the identical code path, not
  re-verified against hardware separately since success-path capture-log
  writes were already exercised live dozens of times this session).
  Confirmed the design-doc changes render correctly and the deployed
  daemon starts and serves normally post-deploy.

## Remaining Limitations / Follow-up

- The failure-path fix is unit-tested but not exercised against a real
  hardware failure tonight (deliberately — forcing a real camera error
  on a live, otherwise-healthy Pi isn't a safe or clean thing to do just
  to check a cosmetic log-id prefix). If a real scheduler capture
  failure is ever observed in the wild, its `.log.json`/database row is
  worth a quick spot-check that the prefix/`triggered_by` came through
  correctly, as a final confirmation.
- This closes out every item from this session's original "what's not
  done" inventory except two genuinely out-of-scope-for-tonight things,
  both already flagged in earlier worklogs as follow-up, not omissions:
  the dashboard doesn't yet have a UI to *query/filter* capture history
  by `triggered_by` (the field now exists and is recorded; nothing reads
  it back yet), and `app.js`'s older client-only dirty-tracking pattern
  for camera settings (as opposed to the server-truth `config_staged`
  pattern used everywhere new this session) was never revisited.
