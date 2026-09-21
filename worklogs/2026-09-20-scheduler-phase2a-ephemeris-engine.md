# Dated Worklog: 2026-09-20 - Scheduler Phase 2a: Ephemeris Engine (Solar/Lunar/MilkyWay) + Station Config UI

Status: **implemented, deployed, and verified live.**

## Objective

Design doc `docs/optic-daemon-scheduler.md` §2/§11 specifies `Trigger::Ephemeris`
(Solar/Lunar/MilkyWay events) and four elevation/illumination-window
constraints as Phase 2+ work, explicitly deferred at Phase 1 time. User
requested all of it be implemented in one overnight session, deferring
further questions, with a separate worklog per phase. This is phase 2a:
the astronomy engine itself, the `Trigger`/`Constraints` wiring, and the
`Station` config UI needed to actually use any of it (previously a
backend-only struct with zero UI, confirmed by grep before starting).

## Acceptance Criteria

- `Trigger::Ephemeris { target: CelestialTarget, offset_secs }` fires
  correctly for every named event in `SolarEvent`/`LunarEvent`/
  `MilkyWayEvent` (design doc §2), computed against a `Station`.
- `Constraints` gains `sun_elevation_window`, `moon_elevation_window`,
  `moon_illumination_window`, `milky_way_elevation_window` — each an
  independent AND-composed filter, same shape as the existing
  `time_window`.
- A rule using any of the above with no `Station` configured produces
  zero occurrences (dead-rule treatment), not an error or panic.
- Positions are validated against real astronomical reference data, not
  just internally self-consistent — a genuine correctness bar, not just
  "doesn't crash."
- A dashboard UI exists to actually set/edit/save/discard the `Station`
  (lat/long/elevation/timezone) — previously impossible to configure at
  all outside raw API calls.
- Merge window bumped 5s → 10s (separate small fix, folded in here since
  it was flagged and decided in the same conversation before this phase's
  work started): real measured capture durations (~5.1-5.6s,
  `docs/optic-daemon-capture-performance.md`) are at/above the original
  5s assumption.

## New Dependency: `astro` (v2.0.0)

Researched via crates.io/docs.rs/GitHub before picking anything (not
guessed): confirmed pure Rust (no C/FFI), MIT-licensed, and — critically —
covers **both** Sun and Moon in one crate (`astro::sun`, `astro::lunar`),
so this feature needed only one new dependency instead of the two
originally flagged as separate research items in the design doc. It also
exposes `astro::coords::{asc,dec}_frm_gal`/`gal_frm_eq` for Galactic
coordinates, which meant `MilkyWayEvent` needed **no** dependency at all
beyond `astro` itself, confirming the design doc's own suspicion (§2/§11)
that Milky Way math would be cheaper than expected.

Verified it actually compiles for this project before committing to it:
`cargo add astro@2` + `cargo build` succeeded locally; the real
cross-compile confirmation is the Pi-native deploy in Validation below.

**A genuinely easy-to-get-wrong detail, caught by reading the crate's own
test suite rather than assuming**: `astro`'s formulas (following Meeus'
original book) treat longitude as **positive west** of Greenwich — the
opposite of the standard GPS/ISO-6709 convention (east positive) this
codebase's `Station.longitude` already uses everywhere. Confirmed via
`astro`'s own `tests/transit.rs`, which encodes Boston (71.0833° *west*)
as `+71.0833`. `src/ephemeris.rs`'s `west_longitude_rad` negates
`Station.longitude` specifically to correct for this — documented
prominently in the module's doc comment so it isn't silently
reintroduced later.

## Implementation Summary

### `src/ephemeris.rs` (new)

Pure astronomy math, no camera/hardware/I-O dependency — same
"deterministic function, fully unit-testable" property design doc §13
called out for `occurrences()` itself:

- `julian_day`/`datetime_from_julian_day`: exact conversions via Unix
  timestamp, not `astro::time::Date`'s calendar-field struct.
- `sun_equatorial(jd)`: apparent geocentric RA/Dec (nutation + aberration
  corrected).
- `moon_equatorial(jd)` / `moon_illumination_pct(jd)`: geocentric RA/Dec
  and illuminated-fraction percentage.
- `moon_rise_set_altitude_deg(jd)`: the Moon's own parallax-adjusted
  rise/set threshold (folds topocentric horizon-dip in via the same
  technique `astro::transit` uses internally, without needing a full
  topocentric coordinate conversion).
- `milky_way_core_equatorial(jd)`: Galactic Center (l=0°, b=0°) →
  equatorial via `astro::coords::gal_frm_eq`, precessed from B1950.0 to
  the date of interest (`astro::precess`) — a few arcminutes of
  correction over 76+ years, cheap to include correctly rather than
  ignore.
- `lunar_phase_occurrences`: every New/First/Full/Last Moon in a date
  range, via `astro::lunar::time_of_phase` sampled every 20 days across a
  padded window (synodic month ≈29.53 days) and deduplicated — works
  around `time_of_phase` only returning the single nearest event to a
  given date, not a range query.
- **Two generic search primitives**, shared by all three taxonomies
  (Solar/Lunar/MilkyWay) rather than three separate implementations, per
  the design doc's own hint that "any fixed RA/Dec target... would use
  identical rise/set/transit/elevation logic":
  - `find_crossings`: threshold-crossing search (rise/set/named-twilight-
    tier/fixed-elevation/moon-rise-set), sampled every 4 minutes then
    refined by bisection to sub-second precision.
  - `find_extrema`: local-maximum/minimum search (solar noon/nadir, lunar
    transit/antitransit, Milky Way core transit), sampled then refined by
    golden-section search.
  - `find_azimuth_crossings`: a third, narrower primitive specifically
    for `MilkyWayEvent::Orientation` — azimuth wraps 0°/360°, which would
    make the generic threshold search misfire at the wrap seam; detects
    and skips the wrap rather than generalizing the main primitive for a
    case only one event type needs.
- Elevation/illumination windows don't need any crossing search at all —
  they evaluate the same position functions directly at each candidate
  instant, exactly the same shape as the existing `TimeWindow::holds`.

### `src/optic_scheduler.rs`

- `Trigger::Ephemeris { target: CelestialTarget, offset_secs: i64 }` —
  signed offset as an integer (matching this file's existing `_secs`
  convention for every other duration field) rather than a directly-
  serialized `chrono::Duration`.
- `CrossingDirection`, `CelestialTarget`, `SolarEvent`, `LunarEvent`,
  `MilkyWayEvent` — all exactly as specified in design doc §2's data
  model.
- `Constraints` gained the four new `Option<T>` fields; `Constraints::holds`
  now takes `station: Option<&Station>` (previously didn't need it at
  all) and ANDs all five constraint checks together.
- `SunElevationWindow`/`MoonElevationWindow`/`MilkyWayElevationWindow`:
  each `holds` returns `false` with no station configured (not a panic).
  `MoonIlluminationWindow` doesn't need a station at all — illumination is
  pure Sun-Moon-Earth geometry, independent of the observer's location —
  documented explicitly since it's the one exception among the four.
- `rule_occurrences` threads `station: Option<&Station>` through; an
  `Ephemeris` trigger with no station returns zero occurrences (dead-rule
  treatment, same as an unsatisfiable constraint — not an error).
- `ephemeris_occurrences`/`solar_occurrences`/`lunar_occurrences`/
  `milky_way_occurrences`: the domain mapping from each named event to
  the right `ephemeris` primitive call + threshold/direction (e.g.
  `SolarEvent::CivilDawn` → -6° rising, `SolarEvent::Sunrise` → -0.8333°
  rising per the design doc's own refraction note). A signed
  `offset_secs` shifts the *search window* rather than each found
  instant, which also correctly finds events whose un-offset instant
  falls just outside the requested range but whose offset instant
  belongs inside it.
- `MERGE_WINDOW`: `Duration::seconds(5)` → `Duration::seconds(10)`, with
  the two tests whose fixture data assumed exactly 5s adjusted (6s-apart
  → 11s-apart for "must not merge").

### `src/web/index.html` / `src/web/app.js` (Station config UI)

New `.station-card` on the **dashboard** (per explicit user direction —
not the Scheduler page): latitude/longitude/elevation/timezone inputs,
its own Save/Discard buttons, staged through the *same* shared
`preview_config.json`/commit/discard flow every other config section
already uses (design doc §9 — no new backend endpoint). Reuses
`/api/schedule/preview` (station field), which requires also sending the
current `rules` array unchanged — `scheduleRulesCache`, refreshed on
every status poll, exists specifically so editing Station never
accidentally wipes the rule list.

Follows the *live, server-truth-driven* dirty-tracking pattern this
session's earlier bug fix established (`config_staged` from `/api/status`
— content-comparison based, not existence-based), not `app.js`'s older
client-only `baselineSettings` comparison pattern used for camera
settings — deliberately, since that older pattern was flagged as likely
having the exact same "resets to clean on refresh" bug `config_staged`
was built to fix, and there was no reason to copy a known-suspect pattern
into new code.

Station fields are populated from the server exactly once per page load
(`stationFieldsInitialized`), not on every 3s status poll — the same
reason `app.js`'s camera-setting inputs are never touched by the polling
loop, so mid-edit typing is never clobbered by a background refresh.

### `src/web/styles.css`

- Added `.station-card` to the existing two-column grid-placement rules
  (desktop + the 820px mobile breakpoint that collapses to one column).
- New `.station-grid` (simple 2-column form grid) rather than reusing
  `.control-grid` — `.control-grid` has a `:nth-last-child(-n+3)`
  full-width override tuned specifically for the 6-field camera-controls
  panel's layout; applying it to Station's 4 fields would have forced 3
  of the 4 to span full width for no reason.

## Validation

- `cargo fmt --all`: applied, clean.
- `cargo test --locked`: **88 passed; 0 failed; 0 ignored** (74 pre-phase-2
  + 4 from the earlier same-day `config_is_staged` fix + 10 new
  `ephemeris` tests).
- `cargo clippy --locked --all-targets -- -D warnings`: clean (one
  digit-grouping lint fixed along the way — `2_433_282.4235` →
  `2_433_282.423_5`).
- `node --check src/web/app.js`: syntax valid (no `biome` binary
  available in this dev environment to run the project's actual linter
  locally; the Pi-native deploy script runs the real `biome check` as
  part of its own gate).
- **Astronomical correctness, triangulated against external sources
  rather than self-referential assertions only**:
  - `sun_declination_crosses_zero_at_the_published_equinox_instant`:
    checks the Sun's computed declination is ~0° at 2026-09-23T00:05:00Z
    — the September 2026 equinox instant, corroborated by two
    independently-sourced search results agreeing on the same minute.
    Chosen over a single site's daily sunrise/sunset table entry after
    that table proved unreliable to fetch/verify directly for this
    specific future date (one source returned a 403, another returned an
    ambiguous "today" value of uncertain actual date) — a precise,
    source-triangulated event beats a single scraped number.
  - `sunrise_and_sunset_each_occur_exactly_once_on_a_september_day_in_vancouver`
    and `solar_noon_is_the_elevation_maximum_symmetric_between_sunrise_and_sunset`:
    self-consistency checks (exactly one sunrise/sunset per day, plausible
    day length, solar noon strictly between them and time-symmetric to
    within 60s) that don't depend on any external "expected" clock time at
    all — robust to not having a fully-verified per-date reference table.
  - `milky_way_core_is_a_plausible_fixed_sky_position`: Sgr A*'s computed
    RA/Dec falls within a loose box around its well-known J2000
    coordinates (RA ≈266.4°, Dec ≈-29.0°), accounting for precession
    drift since B1950.
  - `lunar_phase_occurrences_finds_exactly_one_full_moon_in_a_synodic_month`
    / `_finds_all_four_phases_across_two_months`: exact-count checks
    against the known ~29.53-day synodic period.
- **Pi-native deploy**: first attempt failed at the Biome lint gate
  (`app.js` had two lines exceeding the 100-char line width configured in
  `biome.json`) — caught *before* the Rust build/install step, and the
  daemon auto-recovered to the prior good version within the deploy
  script's own stop/restart window (confirmed live: `uptime_seconds: 21`
  immediately after, no downtime beyond the brief planned restart). Fixed
  both lines, verified locally with `npx @biomejs/biome@2.5.14 check`
  (no `biome` binary available in this dev environment otherwise) before
  redeploying. Second attempt: **succeeded** — confirms `astro` v2.0.0
  actually cross-compiles cleanly for aarch64-linux (the real risk this
  dependency carried; pure-Rust with no C/FFI meant it was always likely
  to work, but this is the first real confirmation, not an assumption).
- **Real end-to-end verification against the live Pi** (2026-09-20,
  ~02:55 UTC):
  - Set a real `Station` (49.2827°N, -123.1207°W, 70m, America/Vancouver
    — matching the Pi's confirmed system timezone) via
    `POST /api/schedule/preview` + `POST /api/config/commit`, mirroring
    exactly what the new dashboard Station card sends. Confirmed it
    persists across a fresh `GET /api/status` after commit. Left this
    committed rather than reverted — a real, useful default per the
    user's own instruction ("use PI system timezone and a close enough
    GPS coordinates"), not just a throwaway test fixture; the previously-
    stated plan to always restore prior state doesn't apply here since
    there was no prior Station to restore (it was `null`).
  - Staged three test rules (one each: `Solar(Sunset)`,
    `Lunar(Moonrise)`, `MilkyWay(CoreRise)`) and read
    `GET /api/schedule/forecast?hours=72`:
    - Sunset fired at `2026-09-21T02:14:17Z` = 19:14 PDT — closely
      matches real-world Vancouver sunset for that date, and tracked
      correctly night-to-night (19:12 PDT the next day, consistent with
      the seasonal drift toward earlier sunsets near the equinox).
    - MilkyWay core rise fired at `22:35:17`, `22:31:22`, `22:27:26` UTC
      on three consecutive nights — **3m55s and 3m56s earlier each
      night**, matching the design doc's own predicted sidereal-vs-solar
      drift ("~4 min earlier per day," §2's `MilkyWayEvent`
      implementation notes) almost exactly. A strong independent
      confirmation this isn't a coincidentally-plausible number.
    - Moonrise fired once per night at increasingly later times
      (23:59, 00:25, 00:45 UTC), consistent with the Moon's ~24h50m
      diurnal cycle drifting later each solar day.
  - Discarded the staged test rules (`POST /api/config/discard`) and
    confirmed the committed config reverted to exactly the original
    `every1min` rule, `run_state` stayed `Paused` throughout (never
    resumed for this test — the scheduler actor never actually fired
    anything; verification was entirely through the forecast/staging
    API, per the user's "brief supervised check, then pause" instruction
    — even more conservative here, since it never needed to run at all).
  - Confirmed every served web asset (`index.html`, `app.js`,
    `scheduler.html`, `scheduler.js`, `styles.css`) is byte-identical to
    the source tree, and that the Station form's HTML
    (`#station-latitude`/`#station-longitude`/`#station-timezone`/
    `#save-station`) is present in the served page.

## Remaining Limitations / Follow-up

- Rule-editor UI (`scheduler.html`/`scheduler.js`) has **no way to
  actually create an Ephemeris-triggered rule or the four new
  constraints through the UI yet** — this phase only lands the backend +
  Station config. Rule-editor UI for Ephemeris is explicitly deferred to
  a separate phase (2b), so testing this phase's backend live requires
  direct API calls (`POST /api/schedule/preview`), the same way earlier
  bug-fix verification in this session worked before its own UI existed.
- Precision is deliberately bounded (documented at length in
  `ephemeris.rs`'s module doc comment) — geocentric, not topocentric;
  Moon gets no nutation/aberration refinement (Sun does); Galactic Center
  uses precession only, no proper-motion model. Sub-arcminute/sub-minute
  accuracy, intentionally not arcsecond-grade, since this is a camera-
  scheduling feature, not a precision ephemeris tool.
- `MoonIlluminationWindow` is the only one of the four new constraints
  that doesn't need a `Station` — worth double-checking this reads as
  intentional rather than an oversight if revisited later (it's
  documented inline, but flagging here too).
