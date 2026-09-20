# Dated Worklog: 2026-09-19 - `Rule.constraints`: `Vec<Constraint>` → Named-Optional-Field Struct

Status: **implemented, deployed, and verified**.

## Objective

Design discussion in chat (not yet in any doc) established that
`rule.constraints: Vec<Constraint>` should really only ever hold *one of
each constraint type*, never two of the same type — two `TimeWindow`s (or
any duplicate) would be redundant since constraints are AND-composed
(`rule.constraints.iter().all(|c| c.holds(when))`): a second instance of a
type can only narrow or exactly duplicate the first, never add anything a
single range/window can't already express.

User decision, after weighing options: enforce this **structurally**, at
the type level, not via a runtime validator (the `Vec` + `validate_*`
pattern already used for rule-slug uniqueness). Concretely: replace
`Vec<Constraint>` with a struct that has one named `Option<T>` field per
constraint type — today just `time_window: Option<TimeWindow>`, since
`TimeWindow` is still the only implemented constraint type. Phase 2+
constraint types (`SunElevationWindow`, `MoonElevationWindow`,
`MoonIlluminationWindow`, `MilkyWayElevationWindow` — design doc §2) get
added as new fields on this struct when each is actually implemented, not
stubbed out now against types that don't exist yet.

## Acceptance Criteria

- `Constraint` enum removed; replaced by a `Constraints` struct with a
  `time_window: Option<TimeWindow>` field, and a standalone `TimeWindow`
  struct carrying what `Constraint::TimeWindow`'s fields used to be.
- Two `TimeWindow`s on one rule becomes a genuine compile error, not a
  runtime-rejected value — there is nowhere to put a second one.
- `occurrences()`'s constraint-filtering behavior is unchanged for every
  currently-expressible case (single `TimeWindow` or none) — this is a
  structural/representational change, not a behavior change.
- Wire format changes from `constraints: [{kind:"TimeWindow", ...}]` to
  `constraints: {time_window: {...} | null}` — confirmed safe against the
  live Pi's actual persisted state first (see Findings), since this is a
  breaking wire-format change with no migration path written.
- `scheduler.js` updated to read/write the new object shape instead of
  searching an array by `kind`.
- All existing tests updated for the new shape; the one test that relied
  on constructing *two* `TimeWindow`s on one rule
  (`dead_rule_produces_zero_occurrences_without_erroring`) is
  re-implemented using a single degenerate (`start == end`, always-false)
  `TimeWindow` instead, since two is no longer constructible — its
  original scenario is now *itself* evidence the refactor worked, not
  something to keep testing.
- Design doc (`docs/optic-daemon-scheduler.md`) updated to reflect the new
  data model shape and record this as a resolved §14 decision.

## Test Plan (written before implementation)

- **Before any code change**: check the live Pi's actual persisted
  `config.json` (durable) and the `/dev/shm` cache mirror for any
  committed rules with constraints, since this is a breaking wire-format
  change with no dual-format migration — if real data existed in the old
  shape, this would need either a migration step or explicit user
  sign-off to discard it.
- `cargo fmt --all -- --check`, `cargo test --locked --all-targets`,
  `cargo clippy --locked --all-targets -- -D warnings` locally (macOS).
- Add a genuine `Constraints`/`TimeWindow` JSON round-trip test — the
  existing suite never actually had one despite a test named
  `rule_with_weekdays_time_window_round_trips_through_json` (misleading
  name predates this change: it round-trips `RecurringTime` + `Weekdays`,
  not a `TimeWindow` constraint at all).
- Full Pi-native build/deploy (`./scripts/build-deploy-optic-daemon.sh`,
  no `--assets` — this touches `src/*.rs`), including its own served-
  content verification.
- Manual end-to-end smoke test against the live daemon: stage a rule with
  a `TimeWindow` constraint through `/api/schedule/preview` in the *new*
  wire shape, confirm `/api/schedule/forecast` respects it, confirm the
  rules-editor UI (unchanged HTML/CSS from the trigger/constraint-box
  work) still saves and displays it correctly through the new
  `scheduler.js` read/write path.

## Findings While Implementing

- **Checked the live Pi before touching anything, since this is a
  genuine breaking wire-format change with no migration path**: both
  `~/.local/state/optic-daemon/config.json` (durable) and
  `/dev/shm/optic-daemon/config.json` (cache mirror) show `"rules": []` —
  no committed rule currently exists in the old array-shaped
  `constraints` format, so there is nothing to migrate or lose. This
  would need a different plan (a migration, or explicit confirmation to
  discard) had any rule existed.
- `dead_rule_produces_zero_occurrences_without_erroring` is the one place
  the old suite depended on *being able to construct* two `Constraint::
  TimeWindow`s on one rule (two disjoint windows, both required, hence
  always false). That construction is now a compile error by design —
  rewrote the test to use one degenerate zero-width window
  (`start == end`, which the existing `holds()` logic already evaluates
  to always-false: `local_time >= start && local_time < end` can never be
  true when `start == end`) to preserve the same "a constraint that can
  never be satisfied produces an empty result, not a panic" intent
  without needing a since-forbidden construction.

## Implementation Summary

- `src/optic_scheduler.rs`:
  - Removed `enum Constraint` and its `impl Constraint { fn holds }`.
  - Added `struct Constraints { pub time_window: Option<TimeWindow> }`
    (`#[serde(default, deny_unknown_fields)]`, mirroring `Station`/
    `ScheduleConfig`'s existing attribute style — not `skip_serializing_if`
    on the field, so the JSON always shows `"time_window": null` rather
    than omitting the key, matching how `ScheduleConfig.station:
    Option<Station>` is already handled) and `impl Constraints { fn
    holds }`, which now chains a fixed check per field instead of
    iterating a `Vec`.
  - Added `struct TimeWindow { days, start, end }` (moved verbatim from
    the old `Constraint::TimeWindow` variant fields) with `impl TimeWindow
    { fn holds }` (the exact overnight-wrap logic moved unchanged from
    `Constraint::holds`'s match arm).
  - `Rule.constraints: Vec<Constraint>` → `Rule.constraints: Constraints`.
  - `occurrences()`: `rule.constraints.iter().all(|c| c.holds(when))` →
    `rule.constraints.holds(when)`.
  - Tests: `rule()` test helper, both `TimeWindow`-behavior tests, and
    `dead_rule_produces_zero_occurrences_without_erroring` updated for the
    new types (see Findings for the last one). Added
    `rule_with_time_window_constraint_round_trips_through_json` — a
    round-trip test that actually exercises a populated `Constraints`
    (the gap noted above).
- `src/web/scheduler.js`:
  - Wire-format doc comment updated: `Constraint::TimeWindow` array entry
    → `Constraints.time_window` object field.
  - `describeConstraints`: `constraints?.find((c) => c.kind ===
    "TimeWindow")` → `constraints?.time_window` (direct property access,
    no search needed — one of the ergonomic wins of this shape).
  - `openRuleForm`: same lookup simplified the same way.
  - Submit handler: `const constraints = []; ... constraints.push({kind:
    "TimeWindow", ...})` → `const constraints = {}; ...
    constraints.time_window = {...}` (object, not array).
- `docs/optic-daemon-scheduler.md`: §2's `Constraint` enum replaced with
  the `Constraints` struct-of-optionals shape; the "second, independent
  half of the data model" intro paragraph updated to describe the new
  representation; §14 gained a new resolved-decision entry recording this
  choice and why (structural enforcement over runtime validation, deferred
  until there was an actual invariant to violate).
- No changes to `src/web.rs`, `src/web/scheduler.html`, or
  `src/web/styles.css` — the box/field IDs from the prior trigger/
  constraint-container work are unchanged; only the JS layer reading/
  writing them changed shape.

## Validation

- **macOS dev target:** `cargo fmt --all -- --check` clean; `cargo test
  --locked --all-targets` — 68/68 passed, including the rewritten
  `dead_rule_produces_zero_occurrences_without_erroring` and the new
  `rule_with_time_window_constraint_round_trips_through_json`; `cargo
  clippy --locked --all-targets -- -D warnings` clean.
- `npx @biomejs/biome check` on all four web files — clean.
- **Pi-native full build/deploy** (`./scripts/build-deploy-optic-daemon.sh`,
  no `--assets` since this touches `src/*.rs`): bootstrap, test, clippy,
  release build, install, and the script's own served-content
  verification all passed — no rollback triggered.
- **Real end-to-end smoke test against the live daemon**, in the *new*
  wire shape: staged a rule with `"constraints":{"time_window":{"days":
  {"kind":"Weekdays","value":["Mon",...,"Fri"]},"start":"08:00:00",
  "end":"18:00:00"}}` (object, not array) via `/api/schedule/preview`;
  confirmed `/api/schedule/forecast?hours=168` returned 100 shots, every
  one on a Mon–Fri (`Friday, Monday, Thursday, Tuesday, Wednesday` — no
  Saturday/Sunday), confirming the constraint filters correctly through
  the new representation; confirmed `/api/status` echoes the identical
  `constraints: {time_window: {...}}` shape `scheduler.js` now expects to
  read (`rule?.constraints?.time_window`); discarded afterward, confirmed
  `/api/status` shows `rules: []` again — the Pi is left in the same clean
  state it was in before this change.

## Remaining Limitations / Follow-up

- **Not visually verified in an actual rendered browser** — the HTML/CSS
  from the prior trigger/constraint-box work is untouched by this change
  (only `scheduler.js`'s read/write logic changed), so the same standing
  limitation applies: worth a look in the browser to confirm the rules
  editor still saves/loads/displays a time-window constraint correctly
  end-to-end through the new wire format, not just via direct API calls.
- This was a genuine breaking wire-format change with no migration path —
  safe here only because the live Pi had zero committed rules at the time
  (confirmed before touching any code, see Findings). A future schema
  change of this kind, made after real rules exist, would need an actual
  migration step or explicit user sign-off to discard existing data.
- Phase 2+ constraint types (`SunElevationWindow` etc.) still don't exist
  as real Rust types — `Constraints` currently has exactly one field. Each
  gets added the same way, one at a time, when actually implemented.
