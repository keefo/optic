# Dated Worklog: 2026-09-20 - Scheduler Phase 2d: Dead-Rule and Overlap Advisories

Status: **implemented, deployed, and verified.**

## Objective

Design doc §8 specifies two Shot Forecaster advisories, both previously
undesigned-and-unimplemented (confirmed via grep before starting — only
a *unit test name* referencing "dead rule" existed, for the underlying
zero-occurrence engine behavior, not a UI-facing advisory):

- **Dead-rule advisory**: flag any *enabled* rule producing zero shots
  across the forecast horizon — usually contradictory constraints.
- **Overlap advisory**: flag when two enabled rules are both "active"
  over a shared span of time without ever merging into one physical
  shot — §3.1's own stated gap ("what this does not solve"): two
  different cadences can both stay active for hours, each rarely landing
  within `MERGE_WINDOW` of the other, so the *combined* rate over that
  span is higher than either rule's stated rate on its own, with nothing
  in the merge+tag mechanism surfacing that fact.

## Acceptance Criteria

- Both advisories are pure functions of `(schedule, shots)` — no new I/O,
  consistent with `occurrences()`'s own "deterministic, fully
  unit-testable" design (§13).
- Dead-rule advisory ignores disabled rules (a disabled rule producing
  zero shots is expected, not a misconfiguration).
- Overlap advisory ignores disabled rules, and correctly reports nothing
  for rules whose occurrence ranges are genuinely disjoint (design doc
  §4's own Peak/Off-hours worked example).
- Both surface in the Shot Forecaster UI, computed against whatever's
  *currently staged* (not just committed) — same "edits are visible in
  the forecast immediately" property §8 already requires of the rest of
  the forecaster.

## Implementation Summary

### `src/optic_scheduler.rs`

- `dead_rule_slugs(schedule, shots) -> Vec<String>`: every enabled rule
  whose slug never appears in any shot's `rule_slugs`.
- `OverlapAdvisory { rule_slugs: [String; 2], window_start, window_end,
  combined_shots }` + `overlap_advisories(schedule, shots) ->
  Vec<OverlapAdvisory>`: for every pair of enabled rules, reads each
  rule's own contributed instants back out of the already-merged
  `shots` (filtering by slug membership in `rule_slugs`), and if their
  own min-to-max ranges overlap at all, reports the overlapping span and
  the combined count of both rules' own instants falling inside it — a
  shot the two rules *did* merge into still counts once per rule here
  (matching how "combined rate" should read: each rule's own
  contribution to the shared window, not the distinct-physical-shot
  count the Shot Forecaster's main table already shows).
- Deliberately built directly on the already-merged `shots` list rather
  than re-deriving raw per-rule occurrences independently — same
  "single implementation, not three" principle §3 states for why
  `occurrences()` itself is shared by the actor/Shot Forecaster/Storage
  Forecaster; these two advisories are just further consumers of the
  same one occurrence list, not a parallel computation path.
- Quadratic in enabled-rule count for the pairwise overlap check — fine
  in practice (schedules have a handful of rules, not thousands, same
  reasoning already applied elsewhere in this codebase for small-N
  loops).

### `src/web.rs`

- `ForecastResponse` gained `dead_rule_slugs: Vec<String>` and
  `overlap_advisories: Vec<OverlapAdvisoryResponse>` (the latter a
  small UTC-converting wrapper around `optic_scheduler::OverlapAdvisory`,
  matching how `ForecastShot` already wraps `optic_scheduler::ForecastedShot`
  the same way).
- `schedule_forecast` now keeps the raw `optic_scheduler::ForecastedShot`
  list around (previously mapped straight into the UTC-only API shape)
  specifically so the two new functions — which need the `Tz`-typed
  `ForecastedShot`, not the API's UTC-only `ForecastShot` — can consume
  it before the UTC conversion happens.

### `src/web/scheduler.html` / `scheduler.js`

- New `#forecast-advisories` container inside the Forecast card, between
  the storage warning banner and the shot table.
- `renderAdvisories(data)`: renders one `.notice[data-kind="warning"]`
  line per dead rule and per overlap pair, using each rule's `label`
  (looked up from the in-memory `rules` array by slug, falling back to
  the raw slug if not found) rather than the raw slug alone, so the
  message reads in the operator's own naming, not internal IDs.
- Wired into the existing `renderForecast()` alongside
  `renderStorageForecast()`, so both advisory types refresh on every
  forecast poll/edit, same as everything else on this page.

## Validation

- `cargo fmt --all` / `cargo test --locked`: **95 passed; 0 failed; 0
  ignored** (91 prior + 4 new: dead-rule flags only the enabled dead
  rule and not a disabled one with the same shape; overlap advisory
  finds nothing for §4's disjoint-time-window worked example; overlap
  advisory reports a shared span with a combined count exceeding either
  rule's own contribution for §3.1's baseline/site-visit example;
  overlap advisory ignores a disabled rule that would otherwise
  trivially overlap).
- One test-writing correction worth recording: an initial version of the
  "no overlap" test used two same-cadence rules with no constraints at
  all, which trivially *does* overlap (same start time) — the test
  passed anyway because I'd forgotten to actually assert the empty
  result, not because the code was right. Caught before it became a
  false-negative regression test; rewritten using disjoint `TimeWindow`
  constraints (design doc §4's real example) so the "no overlap" case is
  genuinely exercised, with the missing assertion added.
- `cargo clippy --locked --all-targets -- -D warnings`: clean.
- `node --check src/web/scheduler.js` + `npx @biomejs/biome@2.5.14 check`:
  clean.
- Deployed via full `./scripts/build-deploy-optic-daemon.sh` (Rust
  change in both `optic_scheduler.rs` and `web.rs`).
- **Live verification** against the real daemon (2026-09-20, ~03:20 UTC):
  staged a rule set with a deliberately dead rule (a zero-width
  `TimeWindow`, the same always-false construction the unit tests use)
  and a genuinely overlapping pair (5-minute "Baseline" + 30-second
  "Site Visit" cadence, no separating constraint — the exact design doc
  §3.1 example). `GET /api/schedule/forecast?hours=1` returned:
  `dead_rule_slugs: ["deliberately-dead"]`, and one `overlap_advisories`
  entry (`["baseline", "site-visit"]`, a ~55-minute shared window,
  `combined_shots: 123`) — correctly identifying only the two rules with
  no separating constraint, correctly excluding the dead rule (which
  produces no shots to overlap with anything). Confirmed served
  `scheduler.html`/`scheduler.js` byte-identical to source. Discarded
  the staged config and confirmed both `dead_rule_slugs` and
  `overlap_advisories` return empty again for the real committed
  `every1min`-only schedule, `run_state` unchanged at `Paused`
  throughout.

## Remaining Limitations / Follow-up

- The overlap advisory reports pairs, not clusters — three or more
  mutually-overlapping rules produce multiple separate pairwise
  advisories rather than one combined N-way summary. Matches the design
  doc's own example (exactly two rules), not extended further here.
- No advisory for "this rule set's combined rate is fine on average but
  spikes briefly" — the overlap window is a single min-to-max span with
  one combined count, not a finer-grained time-series; a burst hidden
  inside a long, mostly-quiet overlap window would report a modest
  combined rate rather than flagging the burst specifically.
