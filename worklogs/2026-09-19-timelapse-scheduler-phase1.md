# Dated Worklog: 2026-09-19 - Timelapse Scheduler, Phase 1a (Core Rule Engine)

Status: **implemented and validated on both macOS and the real Pi.**
Standalone module, not yet wired into `AppState`/the running daemon's
behavior (that's 1c) — no user-visible change from this slice.

## Objective

Implement the first, self-contained slice of `docs/optic-daemon-scheduler.md`'s
composable-rules scheduler, per the phasing decision recorded in that
doc's status header: build and fully unit-test the core rule engine in
isolation — no astronomy, no wiring into `web.rs`/`main.rs` yet, no
config-persistence relocation yet. This is deliberately narrower than
"Phase 1" as originally described in that doc's status header; it's the
first of four sub-slices needed to deliver Phase 1 itself:

- **1a (this worklog):** `Rule`/`Trigger::{Interval,RecurringTime}`/
  `Constraint::TimeWindow`/`RecurringDays`/`ScheduleRunState`, the
  `occurrences()` composition engine, merge+tag, and slug validation — a
  new `src/optic_scheduler.rs` module, unit-testable on macOS with no
  hardware, no I/O, no wall-clock mocking beyond passing in a fixed
  `now`.
- **1b (separate, later worklog):** relocate `config.json`/
  `preview_config.json` off the `/mnt/capture` tmpfs to the persistent
  `~/.local/state/optic-daemon/` directory, add the `/dev/shm` read-
  through cache, and add the separate durable
  `schedule_run_state.json` — per design doc §2.1. A real fix to a real,
  pre-existing gap (today's config doesn't survive a hardware reboot at
  all), independent of the scheduler's rule logic.
- **1c (separate, later worklog):** wire the module from 1a into
  `AppState`/`main.rs`, add the bounded actor loop that actually fires
  captures on a timer, add `POST /api/schedule/{pause,resume}`, fold
  `schedule` into the existing `AppConfig`/commit/discard flow, add a
  `schedule` snapshot to `GET /api/status`.
- **1d (separate, later worklog):** the dedicated `/scheduler.html` page
  (rules editor + Shot Forecaster).

Splitting 1a out first because it's the actual novel, hard-to-get-right
part (composition semantics, merge/tag, slug uniqueness) and is fully
verifiable in isolation before touching any existing, already-deployed
code path (`web.rs`'s config handling, `AppState`, the live dashboard).

## Acceptance Criteria (1a)

- `src/optic_scheduler.rs` compiles on both macOS (dev target) and Linux,
  matching this project's convention for modules with no
  hardware/platform dependency (`system_status.rs`, `optic_sync.rs`).
- Given a fixed `ScheduleConfig` and a fixed `now`, `occurrences()`
  returns an exact, deterministic list of `(when, contributing rule
  slugs)` pairs over a requested horizon — no hidden state, no real
  clock reads.
- `Trigger::Interval` respects `align_to_wall_clock` (design doc §3):
  aligned mode produces boundary-aligned instants (`:00`, `:05`, ...),
  not instants offset by an arbitrary start time.
- `Trigger::RecurringTime` correctly resolves `RecurringDays::{Every,
  Weekdays, NthWeekdayOfMonth}` against a given timezone, including a
  DST-transition date (a wall-clock time must stay pinned to the correct
  local instant across a spring-forward/fall-back boundary — this is
  exactly the bug class `chrono-tz` exists to avoid, so it needs a test
  that would actually fail on naive UTC-offset arithmetic).
- `Constraint::TimeWindow` correctly gates occurrences, including the
  overnight-wrap case (`start > end`, e.g. 22:00–06:00).
- Rules with occurrences landing within the merge window (5s, per design
  doc §3.1/§14) collapse into one physical shot, tagged with every
  contributing rule's `slug`, sorted alphabetically, deterministically —
  not by arbitrary fire order.
- Slug validation (design doc §3.1, §14 resolved item): rejects empty,
  rejects anything not matching `^[a-z0-9][a-z0-9-]*$`, rejects a
  duplicate against any other rule (case-insensitive), regardless of
  whether either rule is enabled.
- A rule whose constraints can never simultaneously hold produces zero
  occurrences over the test horizon without erroring (feeds the design
  doc's planned "dead rule" Forecaster advisory, §8 — not built this
  slice, but the underlying data needs to support it correctly).

## Test Plan (written before implementation)

All on macOS dev target, `cargo test`, no camera/hardware/network
dependency — this slice has none:

1. **Interval, unaligned vs. aligned**: a `60s` interval from an
   arbitrary `now` produces instants exactly `60s` apart when unaligned;
   produces wall-clock-boundary-aligned instants when
   `align_to_wall_clock: true`, regardless of what second `now` falls on.
2. **RecurringTime, plain daily**: `Every` at a fixed time-of-day
   produces exactly one occurrence per day in the horizon, at the
   correct instant in the configured timezone.
3. **RecurringTime, weekday subset**: `Weekdays([Mon, Wed])` only
   produces occurrences on those weekdays.
4. **RecurringTime, Nth-weekday-of-month**: "every 2nd Wednesday" over a
   multi-month horizon lands on the correct calendar dates (hand-verified
   against a real calendar for the test's fixed date range).
5. **RecurringTime, DST transition**: a daily `Every` rule spanning a
   real DST transition date for a real IANA zone stays pinned to the
   correct local wall-clock time on both sides of the transition (this
   is the test most likely to catch a wrong-by-an-hour bug that naive
   `chrono::Duration` arithmetic on a fixed UTC offset would produce and
   `chrono-tz` is specifically for avoiding).
6. **TimeWindow, same-day**: `08:00–16:00` passes inside, fails outside.
7. **TimeWindow, overnight wrap**: `16:00–08:00` (start > end) correctly
   passes for times after 16:00 *and* before 08:00 the next calendar
   day, fails in between.
8. **Composition/union**: two non-overlapping rules (matching design doc
   §4's first two worked examples) produce the exact union of both,
   correctly sorted.
9. **Merge+tag**: two rules whose occurrences land within the 5s merge
   window collapse into one shot with both slugs present, sorted
   alphabetically — verifies the design doc §4's "Weekday
   baseline"/"Site visit" worked example numerically, not just
   descriptively.
10. **No merge across the boundary**: two rules whose occurrences are
    just *outside* the merge window (5s + 1ms apart) stay as two
    separate shots — an off-by-one/boundary check.
11. **Dead rule**: a rule with a `TimeWindow` that can never be satisfied
    (e.g. `start == end` under some interpretation, or two contradictory
    windows if `Constraint`s beyond one `TimeWindow` are combined)
    produces zero occurrences over the horizon, not an error/panic.
12. **Slug validation**: empty slug rejected; uppercase/space/punctuation
    rejected; two rules with the same slug rejected (including
    differently-cased duplicates, and including when one of the two is
    disabled); otherwise-valid distinct slugs accepted.

## Implementation Summary

- `Cargo.toml`: added `chrono` (`clock`, `serde` features) and `chrono-tz`
  (`serde` feature) — first calendar/timezone-aware dependency in this
  codebase (previously only `std::time::{SystemTime, Instant, Duration}`
  anywhere). Not under the Linux-only dependency section — pure Rust,
  same on both targets.
- `src/optic_scheduler.rs` (new): `ScheduleRunState`, `Station`,
  `ScheduleConfig`, `Rule`, `Trigger::{Interval, RecurringTime}`,
  `RecurringDays::{Every, Weekdays, NthWeekdayOfMonth}`,
  `Constraint::TimeWindow`, `ForecastedShot`, the `occurrences()`
  composition engine (with `merge()` for §3.1's merge+tag), and
  `validate_rule_slugs()`. Module-level `#![allow(dead_code)]` since
  nothing calls into it yet (honest about the "not wired in" state,
  matching the existing `native_camera.rs` convention for a similar
  situation).
- `src/main.rs`: registered `mod optic_scheduler;`. No other wiring.
- Design doc corrections made while implementing (kept in sync,
  code is the source of truth where they diverge): `RecurringDays`
  needed `Eq`-safe matching logic (`NthWeekdayOfMonth` computed via
  `(day - 1) / 7 + 1`, verified against real 2026 calendar dates in
  tests, not just described); the overnight `TimeWindow` wrap needed
  explicit "which calendar day owns this window" logic the design doc
  described only at the concept level.
- Not version-bumped for this deploy (stayed `0.1.27`) — this slice adds
  no reachable behavior (dead-code-allowed, unwired module), so there's
  nothing an API-version check could distinguish. Future slices that add
  real, reachable behavior (1c's endpoints/status fields) will bump per
  the usual convention.

## Validation

- **macOS dev target:** `cargo fmt --all -- --check` — clean.
  `cargo test --locked --all-targets` — 56/56 passed (18 new
  `optic_scheduler` tests, 0 regressions). `cargo clippy --locked
  --all-targets -- -D warnings` — clean after fixing two real issues
  clippy caught: an unused `TimeZone` import in the main module (moved
  to where it's actually needed, the test module's `.with_ymd_and_hms`
  calls) and a collapsible-if in `recurring_time_occurrences` (rewritten
  with a `let`-chain, which also let me delete a genuinely dead/
  impossible defensive branch — `date == end_date && date > end_date` —
  I'd written by mistake).
  One test caught a real bug in the *test itself*, not the code: a
  "case-insensitive duplicate" slug test used `"Daytime"` as the
  duplicate, which format validation rejects for containing uppercase
  before the duplicate check ever runs — correct behavior, since slugs
  must already be lowercase-only per §3.1, meaning two format-valid
  slugs can never differ only by case. Fixed the test to use a genuine
  lowercase duplicate and renamed it to reflect what it actually
  verifies (a disabled rule's slug still blocks reuse).
- **Pi-native build/deploy (`scripts/build-deploy-optic-daemon.sh`,
  still `optic-daemon` 0.1.27):** remote `cargo fmt --check`, `cargo test
  --locked --all-targets` (64/64 passed — the extra 8 over macOS are the
  pre-existing Linux-only `native_camera`/`native_codec` suites,
  unaffected by this change), `cargo clippy --locked --all-targets -- -D
  warnings` all clean. Release build linked correctly; install-with-
  rollback completed with no rollback triggered; `is-enabled`/
  `is-active`, `/healthz`, `/api/status` version match, `/app.js`/`/`
  content checks all passed. Confirms `chrono`/`chrono-tz` (and their
  transitive deps — `num-traits`, `iana-time-zone`, `phf`, `phf_shared`,
  `siphasher`) resolve and cross-compile cleanly for aarch64, not just on
  macOS.

## Remaining Limitations / Follow-up

- 1b/1c/1d (above) are not part of this worklog. Until 1b lands, nothing
  from this slice is reachable from the running daemon or the dashboard
  — it's a standalone, tested library module.
- `Ephemeris`/`SolarEvent`/`LunarEvent`/`MilkyWayEvent` are out of scope
  here per the phasing decision — `src/optic_scheduler.rs`'s `Trigger`/
  `Constraint` enums in this slice only contain the Phase-1 variants;
  adding astronomy later is an additive change to these enums, not a
  rewrite (per the design doc's repeated point that the composition
  architecture doesn't change shape as target types are added).
