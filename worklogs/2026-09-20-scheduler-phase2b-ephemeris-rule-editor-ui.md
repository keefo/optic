# Dated Worklog: 2026-09-20 - Scheduler Phase 2b: Rule Editor UI for Ephemeris Triggers + Elevation/Illumination Constraints

Status: **implemented, deployed, and verified.**

## Objective

Phase 2a (`worklogs/2026-09-20-scheduler-phase2a-ephemeris-engine.md`)
landed the full Solar/Lunar/MilkyWay `Trigger::Ephemeris` backend and the
four new elevation/illumination `Constraints` fields, verified live
through direct API calls — but the rule editor UI
(`scheduler.html`/`scheduler.js`) had no way to actually create one of
these rules through the form. This phase closes that gap: the Ephemeris
trigger type and all four new constraint types are now selectable in the
same rule editor as every Phase 1 trigger/constraint.

## Acceptance Criteria

- Trigger type dropdown gains "Sun / Moon / Milky Way event"; selecting
  it shows a celestial-body select (Sun/Moon/Milky Way core), an event
  select whose options depend on the chosen body, an offset-from-event
  field, and — only for the three events that need them
  (`FixedElevation`, `CoreElevation`, `Orientation`) — a degrees field
  (relabeled "Azimuth" specifically for `Orientation`) and a Rising/
  Setting/Both direction select (not shown for `Orientation`, which has
  no rise/set direction concept).
- Four new `.constraint-box`es (Sun elevation, Moon elevation, Moon
  illumination, Milky Way elevation), each an enable checkbox + min/max
  fields, matching the existing `time_window` box's shape exactly.
- Editing an existing Ephemeris-triggered rule (or one with any of the
  new constraints) correctly re-populates every field from the rule's
  actual data — not just creating new ones.
- The rule list's one-line summary describes an Ephemeris trigger and
  the new constraints in plain language, not a raw enum dump.
- Wire format sent to the backend matches what Phase 2a's live testing
  already confirmed the backend accepts (verified again here from the
  UI-construction code path specifically, not just re-asserting Phase
  2a's result).

## Implementation Summary

### `src/web/scheduler.html`

- Trigger-kind `<select>` gained an `Ephemeris` option.
- New `#ephemeris-fields` block inside the existing `.trigger-box`
  (sibling to `#interval-fields`/`#recurring-time-fields`, same
  `.trigger-fields` grid class, same show/hide-by-`hidden` convention
  already established) — body select, event select (populated by JS,
  starts empty in markup), degrees field, direction field, offset field,
  and a one-line reminder that this trigger needs a Station (with a
  direct link to the dashboard, where Phase 2a's Station card lives).
- Four new `.constraint-box` elements, one per new constraint field on
  `Constraints`, following the exact structural convention the design
  doc's original comment specified ("multiple constraint should be
  inside multiple constraint-box container... one box per constraint
  type") — extended here for the first time since that convention was
  established in Phase 1.

### `src/web/scheduler.js`

- `EPHEMERIS_EVENTS`: one entry per named event per body — `[wireValue,
  label]`, or `[wireValue, label, "degrees"]` / `[..., "degrees",
  "direction"]` for the three events needing extra fields. Drives both
  the dynamically-populated event `<select>` and which extra fields show.
- `populateEphemerisEventOptions()`: rebuilds the event dropdown from
  `EPHEMERIS_EVENTS[selectedBody]` whenever the body changes (or when a
  rule is opened for editing), preserving the current selection if it's
  still valid for the new body.
- `updateEphemerisFieldVisibility()`: shows/hides the degrees/direction
  fields based on the selected event's metadata, and relabels the
  degrees field "Azimuth (degrees, 0-360)" specifically for
  `Orientation` vs. "Elevation (degrees)" for everything else.
- `updateTriggerFieldVisibility()`: extended (not rewritten) to cover the
  three trigger types uniformly by `kind`, and to toggle all four new
  constraint boxes' fields the same way the existing `time_window` box
  already was — one function, one place, matching the file's own stated
  reason for the trigger/constraint-box split in the first place (so a
  visibility case like this can't be missed).
- `describeEphemerisTrigger()`: renders e.g. `"Sun: Sunset"` or `"Milky
  Way: 15° (Rising) +30min"` for the rule list's one-line summary,
  reusing `EPHEMERIS_EVENTS`' labels for named events and formatting the
  struct-variant cases (`FixedElevation`/`CoreElevation`/`Orientation`)
  directly from their fields.
- `describeConstraints()`: rewritten from a single-constraint `if` into
  an array-of-parts builder so all five constraints (the original
  `time_window` plus the four new ones) can each contribute independently
  without the function becoming an unreadable chain of string
  concatenation.
- `openRuleForm()`: the trigger-population `if/else` became `if/else
  if/else` to add the `Ephemeris` branch — populates body/event/
  degrees/direction/offset from the rule's actual `trigger.target`
  (handling both the plain-string unit-variant case and the
  keyed-object struct-variant case), and always calls
  `populateEphemerisEventOptions()` first (defaulting to `Solar` for a
  brand-new rule) so the event dropdown has real options ready the
  moment the form opens, not only after the user touches the body
  select. Also populates all four new constraint checkboxes/fields the
  same way `time_window` already was.
- Submit handler: builds the `Ephemeris` trigger's wire shape
  (`{kind, target: {type, event}, offset_secs}`, with `event` as a
  keyed object only for the three events that need one) and the four new
  constraint objects, alongside the existing `Interval`/`RecurringTime`/
  `time_window` construction it already had.

### `src/web/styles.css`

No changes needed — the new fields reuse `.trigger-box`/`.trigger-fields`
and `.constraint-box`/`.constraint-fields`, already generic since Phase
1's box-per-type redesign.

## Validation

- `node --check src/web/scheduler.js`: syntax valid.
- `npx @biomejs/biome@2.5.14 check src/web/{app.js,index.html,scheduler.js,scheduler.html}`:
  clean (ran the project's actual linter locally via `npx` this time,
  after Phase 2a's deploy caught two line-width violations at the Pi-side
  lint gate — verifying with the real tool locally first this time rather
  than only `node --check`, which can't catch formatting-only rules).
  Two rounds of line-width violations fixed via `biome check --write`
  (long ternaries/object literals exceeding the 100-char configured
  width), re-verified clean with a plain `check` afterward.
- `cargo test --locked` / `cargo fmt --all -- --check`: unaffected by this
  phase (no Rust changes) — still 88/88 passing, confirmed re-run anyway
  since deploying always rebuilds from the current tree.
- Deployed via `./scripts/build-deploy-optic-daemon.sh --assets` (web-only
  change, no Rust touched) — see the deploy log for the exact command and
  result.
- **Live verification** (2026-09-20, ~03:05 UTC): deployed via
  `./scripts/build-deploy-optic-daemon.sh --assets`, then confirmed every
  served asset (`scheduler.html`, `scheduler.js`, `index.html`, `app.js`,
  `styles.css`) is byte-identical to source.
  - Staged a `FixedElevation` rule (15° rising, +30min offset) — the
    struct-variant shape this phase's UI is what makes reachable at all
    (Phase 2a's own live testing only exercised plain-string named
    events like `Sunset`/`Moonrise`/`CoreRise`). Forecast returned one
    occurrence per day at a plausible, consistent time.
  - Staged an `Orientation` rule (Milky Way core crossing azimuth 180°)
    combined with a `sun_elevation_window` constraint (`-90°` to `-6°`,
    "must be at least astronomically dark"). Without the constraint, the
    trigger alone fired correctly once per night, drifting earlier each
    night by the same ~4-minute sidereal amount Phase 2a's `CoreRise`
    test already confirmed. *With* the constraint, it produced **zero**
    occurrences — investigated rather than assumed correct: the
    crossing happens at 18:59 PDT, which is *before* the already-
    verified sunset time of 19:14 PDT (Phase 2a), so the Sun is well
    above -6° at that instant — the constraint is correctly excluding
    an astronomically real "not dark enough yet" case, not a bug. A
    useful reminder that a zero-occurrence result needs checking against
    the actual astronomy before being read as a defect either way.
  - Discarded both test rules and confirmed the committed config
    reverted to exactly `every1min`, `run_state` stayed `Paused`
    throughout (never resumed — verification was entirely through
    staging/forecast, matching Phase 2a's "brief supervised check, then
    pause" discipline, more conservatively even).

## Remaining Limitations / Follow-up

- No client-side validation on the degrees/azimuth/offset number inputs
  beyond the browser's native `min`/`max`/`step` attributes (e.g. nothing
  stops entering a `min_deg` greater than `max_deg` for an elevation
  window) — matches the existing `time_window` constraint's own level of
  validation (none beyond HTML attributes), not a new gap introduced
  here.
- The Shot Forecaster's "N shots in 48h" / "avg interval" stats can now
  be dominated or skewed by a monthly lunar-phase event or a once-a-year
  Milky-Way-season boundary in ways that weren't possible with only
  Phase 1's interval/recurring-time triggers — no UI change made for
  this, since the Storage/Bandwidth Forecaster and dead-rule/overlap
  advisories (design doc §8, still separate follow-up phases) are the
  actual place this kind of thing should surface, not this phase.
