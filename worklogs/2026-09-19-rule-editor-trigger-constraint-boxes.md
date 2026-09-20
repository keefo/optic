# Dated Worklog: 2026-09-19 - Rule Editor: Explicit Trigger/Constraint Containers

Status: **implemented, deployed, and verified** (see Limitations for what
"verified" does and doesn't cover without a real rendered browser).

## Objective

User-driven redesign of `/scheduler.html`'s rule editor form, following a
real bug: the trigger-level `#weekday-picker` fieldset was visibly showing
under a Fixed Interval trigger despite `updateTriggerFieldVisibility()`
correctly setting `.hidden = true` on it — root-caused to a global
`fieldset { display: flex; ... }` CSS rule always beating the browser's own
`[hidden] { display: none }` rule (author-origin declarations always beat
user-agent-origin declarations, regardless of specificity). This was the
third time this exact bug class was hit this session (previously:
`.preview-frame img[hidden]`/`.preview-placeholder[hidden]`, then
`.rule-form label[hidden]`), each time patched for one specific selector.

The user's diagnosis: the underlying HTML has no structural grouping of
"this belongs to the trigger" vs "this belongs to the constraint" — every
field is a flat sibling inside one `.control-grid`, so which elements need
which visibility toggle is only knowable by reading JS, not by looking at
the markup. Their proposed fix: give `Trigger` and `Constraint` their own
containers (`.trigger-box`, `.constraint-box`) in the DOM, with visibility
toggled at the container level, not per-field.

## Acceptance Criteria

- Every trigger-specific field lives inside `.trigger-box`; every
  constraint-specific field lives inside `.constraint-box`. Rule identity
  fields (label, slug) stay outside both.
- Both boxes are full-width, with a subtle visual boundary (border) and a
  subtle background tint (`rgba(255, 255, 255, 0.04)`, per the user's
  follow-up), and at least 10px of gap between the two boxes.
- `updateTriggerFieldVisibility()` toggles container-level `hidden` flags
  (`#interval-fields`, `#recurring-time-fields`, `#window-fields`) instead
  of per-field ones — down from 7 individually-tracked elements to 4
  (2 containers + the 2 still-genuinely-nested weekday pickers, which are
  a real sub-choice within a sub-choice, not something the container
  restructuring can eliminate).
- The `[hidden]`-vs-`display` bug class is fixed **permanently**, not
  patched again for a fourth selector: consolidate the three existing
  scoped overrides into one global rule, since every one of the three
  prior fixes was structurally identical (an author `display` declaration
  beating the UA's `[hidden]` rule) and there is no legitimate case on this
  site where a `[hidden]` element should still render.
- No behavior change to the actual rule data model, wire format, or
  validation — this is a markup/CSS/visibility-wiring change only.
- `.trigger-box`/`.constraint-box` are classes, not IDs, specifically so
  multiple constraints render as multiple sibling `.constraint-box`
  elements (one per `Constraint` in `rule.constraints`), not multiple
  field-groups stuffed inside one shared box — user-specified requirement,
  added mid-implementation. Today's UI only has one constraint type
  (`TimeWindow`) so there is exactly one `.constraint-box` on the page; no
  placeholder boxes are added for the Phase 2+ constraint types that don't
  have a UI yet (`SunElevationWindow` etc.) — that would be speculative UI
  for a feature that doesn't exist. The class-based styling means adding
  their editors later is additive (new sibling boxes), not a restructure.

## Test Plan (written before implementation)

- Local static check: after editing, grep the new HTML for every element
  ID `scheduler.js` still references (`elements.*`) to confirm nothing was
  renamed out from under the script.
- `bash`/`node`-free static reasoning check of the CSS specificity/origin
  fix (same approach used for the two prior `[hidden]` bugs this session,
  since a real rendered-browser check isn't available in this
  environment): confirm the new global `[hidden] { display: none
  !important; }` rule cannot lose to any other rule in this stylesheet, by
  construction (`!important` + universal attribute selector beats any
  normal-importance author declaration regardless of its specificity).
- Biome check on `scheduler.html`/`scheduler.js`.
- Local `cargo run` smoke test: load `/scheduler.html`, confirm the new
  `.trigger-box`/`.constraint-box` markup and IDs are served correctly.
- Deploy via `./scripts/build-deploy-optic-daemon.sh --assets` (exactly the
  fast path built for this kind of change) and confirm the served HTML/CSS
  reflect the new structure.
- Explicitly flagged to the user: I cannot click through the rendered page
  in a real browser from this environment, so final visual confirmation
  (does it actually look right, are the two boxes visually distinct, does
  toggling actually work end-to-end) is the user's to do.

## Implementation Summary

- `src/web/scheduler.html`: the rule-form markup restructured — Label/Slug
  now direct children of `.rule-form` (previously wrapped in a shared
  `.control-grid` with everything else); trigger type select +
  `#interval-fields`/`#recurring-time-fields` (each `.trigger-fields`)
  nested inside one `.trigger-box`; the window-enabled checkbox +
  `#window-fields` (`.constraint-fields`) nested inside one
  `.constraint-box`, with an HTML comment documenting the one-box-per-
  constraint convention for when Phase 2+ constraints arrive. Per-field
  wrapper IDs that existed only for individual visibility toggling
  (`interval-every-field`, `interval-align-field`, `recurring-time-field`,
  `recurring-days-field`, `window-start-field`, `window-end-field`,
  `window-days-field`) were removed — no longer needed now that toggling
  happens at the container level. `#weekday-picker`/`#window-weekday-picker`
  fieldsets and every input/select ID are unchanged.
- `src/web/scheduler.js`: `elements` trimmed from 7 individually-tracked
  field-wrapper references down to 2 container references
  (`intervalFields`, `recurringTimeFields`) plus `windowFields`, and
  `updateTriggerFieldVisibility()` now sets `.hidden` on those 3 containers
  plus the 2 still-genuinely-nested weekday pickers — 5 assignments total,
  down from 9, with a comment explaining why the container split makes the
  original bug structurally harder to repeat.
- `src/web/styles.css`:
  - Added one global `[hidden] { display: none !important; }` rule near
    the top of the file, consolidating three previously-separate scoped
    fixes for the identical bug (author `display` beating the UA's
    `[hidden]` rule): removed `.preview-frame img[hidden], .preview-
    placeholder[hidden] { display: none; }` and `.rule-form
    label[hidden] { display: none; }` (and its explanatory comment) as
    now-redundant.
  - Added `.trigger-box, .constraint-box` (border, 10px border-radius,
    12px padding, `background-color: rgba(255, 255, 255, 0.04)` per the
    user's follow-up, `display: grid; gap: 12px` for their own internal
    content) and `.trigger-fields, .constraint-fields` (the 2-column field
    grid previously provided by `.control-grid`, now scoped to these two
    classes instead of a shared generic one).
  - Removed `.rule-form .control-grid { grid-template-columns: 1fr 1fr; }`
    as dead CSS — `.control-grid` is no longer used anywhere inside
    `.rule-form` after the restructure. `.rule-form .full-width` kept
    (still used by both weekday-picker fieldsets).
  - Extended the 520px mobile breakpoint's column-collapse rule to include
    `.trigger-fields`/`.constraint-fields` alongside the existing
    `.control-grid`.

## Validation

- Grepped every `#id` `scheduler.js` references against the new
  `scheduler.html` — zero missing (the exact regression class this change
  is meant to prevent).
- `python3` brace-balance check on `styles.css` — 135 open / 135 close.
- `npx @biomejs/biome check` on all four web files — clean.
- Local smoke test (`cargo run`-equivalent against the existing release
  binary, since no Rust changed): confirmed `/scheduler.html` serves the
  new `.trigger-box`/`.constraint-box`/`.trigger-fields`/`.constraint-fields`
  markup and `/styles.css` serves the new global `[hidden]` rule and box
  styling.
- Deployed via `./scripts/build-deploy-optic-daemon.sh --assets` — Biome
  clean, install succeeded, no rollback.
- Confirmed on the live Pi via `curl`: served HTML/CSS match the local
  source exactly. Confirmed via `systemctl --user show ... MainPID,
  ExecMainStartTimestamp` that the daemon was **not** restarted by this
  deploy (unchanged from the prior deploy) — expected and correct for
  `--assets`, and incidentally also proves the running daemon needed no
  changes to serve the new page correctly, consistent with assets being
  served fresh from disk on every request.

## Remaining Limitations / Follow-up

- **Not visually verified in an actual rendered browser** — everything
  above is HTTP-level content verification (served bytes match source) and
  static reasoning about CSS cascade rules, not a screenshot or click-
  through. This is the same limitation flagged on every prior CSS fix this
  session; a real look in the browser is the user's to do.
- The global `[hidden] { display: none !important; }` rule is a broad,
  site-wide change, not scoped to the rule editor — by design, since the
  bug it fixes has independently recurred in three unrelated parts of the
  page already. Worth knowing if any *future* CSS work on this site ever
  has a genuine reason to override `[hidden]` (none currently exists) —
  that would need `!important` too, or an explicit exception added here.
- Multiple simultaneous constraints (one `.constraint-box` per constraint)
  is a structural convention, not yet an exercised feature — there is
  still only one constraint type (`TimeWindow`) and no "add constraint"
  UI, since that doesn't exist until Phase 2+ per
  `docs/optic-daemon-scheduler.md`.

(filled in as applied)
