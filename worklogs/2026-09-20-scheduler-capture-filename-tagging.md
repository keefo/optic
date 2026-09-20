# Dated Worklog: 2026-09-20 - Scheduler Captures: `scheduler-` Prefix + Rule-Slug Filename Tags

Status: **implemented, deployed, and verified**.

## Objective

User found two related bugs testing the live scheduler: a capture fired by
the scheduler landed on the iMac as `testshot-master-archive-<millis>.jpg`
— indistinguishable from a manual "Capture & transfer" test shot, and
missing which rule actually triggered it.

Both are real implementation gaps relative to **already-agreed design**,
not new feature requests: `docs/optic-daemon-scheduler.md` §3.1 fully
specifies rule-slug filename tagging (deterministic sort, capped at 3
tags + `+N` overflow, inserted between profile-slug and timestamp) — it
was just never wired up to the actual filename in `native_camera.rs`. The
`scheduler-` vs `testshot-` prefix was §14's **open decision #4**, and the
doc's own recommendation was the opposite of what's being done here —
worth being explicit about, even while implementing what the user asked
for (see Findings).

## Acceptance Criteria

- Scheduler-triggered captures produce files named
  `scheduler-<profile-slug>-<tag1>-<tag2>-<millis>.{jpg,dng}` (tags = the
  sorted, deduplicated, comma-free rule slugs that fired this shot, capped
  at 3 + `+N` overflow per §3.1). Manual/web UI captures are unaffected:
  `testshot-<profile-slug>-<millis>.{jpg,dng}`, unchanged.
- `optic_sync`'s allowlist (`is_capture_filename`) recognizes both
  prefixes — this is the one thing that *must* change alongside the
  prefix switch, per §14's own warning, or scheduled captures would stop
  being synced to the iMac entirely.
- `optic_capture_log`'s basename-sharing (`derive_capture_id`) keeps
  working unmodified — confirmed it treats the whole basename as opaque
  (strips only the extension), so neither the new prefix nor the inserted
  tags need any change there.
- Design doc §14's open decision #4 marked resolved with the actual
  choice made, not the one it originally recommended.

## Findings While Implementing

- **§14 explicitly recommended keeping `testshot-` universally**, specifically
  to avoid touching `is_capture_filename`'s allowlist. The user asked for
  the opposite (a distinct `scheduler-` prefix) in this session — flagging
  the reversal here per this repo's own change-principles rather than
  silently overriding a recorded decision. Implementing it correctly
  requires exactly the allowlist update §14 called out as the cost of this
  path, so that's done as part of this change, not left as a follow-up.
- **`CaptureRequest` had no concept of "what triggered this" at all**
  before this change — `source: "scheduler"` vs `"web_ui"` only ever
  existed in `CaptureLogEntry` (the capture-history *database* record),
  set by the caller after the fact, with zero visibility into the actual
  filename-writing code in `native_camera.rs`. Added a proper
  `CaptureSource` enum on `CaptureRequest` itself so the one function that
  actually names the file (`publish_capture`) can make this decision
  directly, instead of threading a separate parameter through.
- **`optic_capture_log.rs`'s `failure_capture_id`** (the synthetic ID
  minted only when a capture *fails*, so there's no real file to name)
  still hardcodes `testshot-` regardless of source. Deliberately left
  alone — no real file exists in the failure case, so it never reaches
  `optic_sync`'s allowlist or the iMac; it's purely a cosmetic string in
  the capture-log database. Noted as a minor, non-blocking follow-up
  (same "not blocking" framing §14 already used for the original
  decision), not fixed here to keep this change scoped to the two actual
  reported bugs.
- Confirmed via grep across `src/*.rs` that `is_capture_filename` in
  `optic_sync.rs` is the *only* place with a hardcoded, exhaustive
  `testshot-`-prefix check that could silently break — every other
  `"testshot"` reference is either the write side itself (being changed
  here) or test fixtures/assertions (updated alongside).

## Implementation Summary

- `src/camera.rs`: new `CaptureSource` enum:
  ```rust
  #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(tag = "kind")]
  pub enum CaptureSource {
      #[default]
      WebUi,
      Scheduler { rule_slugs: Vec<String> },
  }
  ```
  Added `#[serde(default)] pub source: CaptureSource` to `CaptureRequest`
  — `#[serde(default)]` so the existing manual-capture JSON body from
  `app.js` (which never sends this field) keeps deserializing exactly as
  before, defaulting to `WebUi`.
- `src/native_camera.rs` (`publish_capture`): basename construction now
  branches on `request.source`. Added `format_rule_tags(rule_slugs) ->
  Option<String>` (sorts is *not* needed here — `ForecastedShot.rule_slugs`
  is already sorted/deduplicated by `occurrences()`/`merge()` before it
  ever reaches here — caps at 3 tags, appends `+N` for the rest, joined
  with `-`). `WebUi` keeps the exact previous format; `Scheduler` produces
  `scheduler-<profile-slug>[-<tags>]-<millis>.{jpg,dng}` (tags segment
  omitted entirely for a single-rule shot with 0 tags — can't happen in
  practice since a scheduler-fired shot always has ≥1 contributing rule
  slug, but handled cleanly regardless).
- `src/optic_scheduler.rs` (`fire_capture`): `CaptureRequest` now includes
  `source: CaptureSource::Scheduler { rule_slugs: shot.rule_slugs.clone() }`.
- `src/optic_sync.rs` (`is_capture_filename`): prefix check widened from
  `starts_with("testshot-")` to `starts_with("testshot-") ||
  starts_with("scheduler-")`. Existing tests extended with `scheduler-`
  cases alongside the original `testshot-` ones (both prefixes must keep
  working, not one replacing the other).
- `docs/optic-daemon-scheduler.md`: §14 decision #4 marked **Resolved**
  with the actual outcome (distinct `scheduler-` prefix, allowlist
  updated) instead of the original recommendation; §3.1's filename-shape
  example updated from `testshot-` to `scheduler-` to match (the tag-
  insertion mechanics it describes were already correct and unchanged).

## Validation

- `cargo fmt --all -- --check`: clean, no diff.
- `cargo test --locked`: **74 passed; 0 failed; 0 ignored** (includes the
  new `format_rule_tags_*`, `capture_request_*`, and
  `is_capture_filename_also_allowlists_scheduler_triggered_captures`
  tests added for this change).
- `cargo clippy --locked --all-targets -- -D warnings`: clean, no
  warnings.
- Full Pi-native build/deploy: `./scripts/build-deploy-optic-daemon.sh`
  (no `--assets`, since this touches `src/*.rs`, including the
  Linux-gated `native_camera.rs` code that only compiles on the target).
  Build succeeded (203/203 crates), rollback-protected install succeeded,
  service came back up healthy: `optic-daemon 0.1.29 is active at
  http://optic.local:8000/`.
- Real end-to-end verification against the live Pi + iMac (2026-09-20,
  ~08:53 UTC):
  - Resumed the scheduler (`POST /api/schedule/resume`) with the existing
    `every1min` rule enabled.
  - Waited for a real scheduled capture to fire (`GET /api/status` showed
    `schedule.last_capture = {"rule_slugs": ["every1min"], "success":
    true}`).
  - Confirmed the transferred files on the iMac
    (`/Users/admin/Pictures/Optic/`):
    `scheduler-master-archive-every1min-1789894386246.jpg`,
    `scheduler-master-archive-every1min-1789894386246.dng`, and the
    matching `.log.json` — correct `scheduler-` prefix (not `testshot-`)
    and the triggering rule slug (`every1min`) embedded, exactly per
    §3.1's spec.
  - `GET /api/status` showed `sync.last_error: null` and
    `sync.queued_files: 0` after the transfer — confirms
    `is_capture_filename`'s widened allowlist picked up the new prefix
    and the Pi's capture-stage queue drained normally, no files stuck.

## Remaining Limitations / Follow-up

- `optic_capture_log.rs`'s `failure_capture_id` still says `testshot-` for
  a *failed* scheduler capture's synthetic log-only ID (see Findings) —
  cosmetic, no real file involved, left as a minor follow-up.
- The design doc's §3.1 "capture-log record should also carry the tag
  list as structured data (`triggered_by: Vec<String>`)" suggestion is
  still not implemented — `CaptureLogEntry.source` remains a plain
  `"scheduler"`/`"web_ui"` string with no structured rule-slug field. Not
  needed for the filename fix; a separate enhancement if the dashboard
  ever wants to show/filter "what triggered this" without parsing
  filenames.
