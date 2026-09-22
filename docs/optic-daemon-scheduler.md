# Design Note: Timelapse Scheduler (`optic_scheduler`) — Composable Rules

> **Status:** Implemented and deployed. Phase 1 (core engine, reboot-durable
> config, live wiring, rules editor) and Phase 2 (ephemeris triggers and
> constraints, storage forecaster, overlap/dead-rule advisories, capture
> history page, config page) are recorded in
> `worklogs/2026-09-19-timelapse-scheduler-phase1*.md` and
> `worklogs/2026-09-20-scheduler-phase2*.md`. Every decision in §14 is
> resolved. This doc was first written as a design proposal before any
> code; the text below keeps that framing where it explains why.
>
> **Relationship to prior work:** supersedes `docs/optic-daemon.md` §5's
> single-mode design (fixed interval *or* solar-adaptive bands as one
> global setting) and Decision #1 in
> `worklogs/2026-09-18-timelapse-scheduler.md` ("fixed interval only for
> v1"). That worklog's other findings still hold and are reused here:
> captures already go through `OpticCamera::capture_to_stage` with no new
> hardware-arbitration logic needed (§5 below), and the 256 MiB
> `/mnt/capture` tmpfs capacity risk it flagged is a first-class part of
> this design (the Storage Forecaster, §8). Decision #3 (which capture
> profile/settings an autonomous shot uses) is confirmed here: **whatever
> the currently committed profile/settings are** — no scheduler-specific
> override, including for DNG (§6). *Update 2026-09-21:* exposure is the
> one exception — `ScheduleConfig.exposure` can switch every scheduled
> capture to global auto-ramped exposure
> (`docs/optic-daemon-exposure-ramping.md`); its default, `Dashboard`,
> keeps this behaviour. Decision #4 (full-disk runtime
> behavior) is resolved in §7 (skip the tick and report it). Decision #6
> (UI placement as a nested section) is superseded by a dedicated page
> (§10). Also new since the
> original worklog: rule-conflict resolution (merge simultaneous
> occurrences into one tagged capture, §3.1) — the user's own proposal,
> made in response to this doc's first draft lacking it entirely.

## 1. Motivation: Rules, Not Modes

The original design treated scheduling as a single global *mode*: fixed
interval, or solar-adaptive (golden hour / daytime / night bands). The
user's actual requirement is broader — expressed as a taxonomy of
**Primary Triggers** (fixed interval, recurring wall-clock/calendar time,
solar ephemeris) that can be **modulated by constraints** (time-of-day/
day-of-week windowing, tiered rates across the day).

Building each combination as its own special-cased "mode" doesn't scale —
"golden-hour interval," "weekday work-hours interval," and "high-rate
daytime / low-rate night" all sound like distinct features but are really
the same two primitives (*trigger* + *constraint*) combined differently.
The right shape is a small set of orthogonal building blocks the user
composes into **rules**, and a final schedule that's the union of every
enabled rule's occurrences. §5 works through this with the user's own
examples to show the primitives are sufficient.

## 2. Core Data Model

```rust
struct ScheduleConfig {
    station: Option<Station>,   // required only if any rule needs it (§4, §5)
    rules: Vec<Rule>,
    exposure: ScheduleExposure, // Dashboard (default) | AutoRamp{..} — global, not per rule;
                                // see docs/optic-daemon-exposure-ramping.md
    // No `enabled: bool` here — run state moved to its own durably-stored
    // type, kept separate from this staged/edited config on purpose. See
    // §2.1.
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ScheduleRunState {
    Running,
    Paused,
}

struct Station {
    latitude: f64,
    longitude: f64,
    elevation_m: f64,           // meters above sea level; refines twilight/elevation math
    timezone: String,           // IANA name, e.g. "America/Vancouver" — see §11
}

struct Rule {
    id: RuleId,                 // stable id (uuid), survives edits/reordering
    label: String,               // user-facing name, e.g. "Daytime interval"
    slug: String,                 // user-entered, unique across all rules — see §3.1
    enabled: bool,
    trigger: Trigger,
    constraints: Constraints,       // struct of Option<T>, not Vec — see below
}

enum Trigger {
    Interval {
        every: Duration,
        align_to_wall_clock: bool,   // see §3 — recommend true by default
    },
    RecurringTime {
        // purpose-built recurrence, not cron/RRULE — see §11
        days: RecurringDays,          // Every | Weekdays(Vec<Weekday>) | NthWeekdayOfMonth{n, weekday}
        time: NaiveTime,               // wall-clock time-of-day, interpreted in Station.timezone
    },
    Ephemeris {
        target: CelestialTarget,
        offset: chrono::Duration,     // signed — e.g. -30min = "30 min before sunset".
                                       // Fixes a latent bug in this doc's earlier draft:
                                       // std::time::Duration can't be signed at all: this
                                       // needs a signed duration type, and chrono::Duration
                                       // is one already, since chrono is a planned dependency
                                       // anyway (§11) — no new type needed for this alone.
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "event")]
pub enum CelestialTarget {
    Solar(SolarEvent),
    Lunar(LunarEvent),
    MilkyWay(MilkyWayEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CrossingDirection {
    Rising,   // Morning / Ascending
    Setting,  // Evening / Descending
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SolarEvent {
    // --- Anchor Points (Singular daily extrema) ---
    SolarNoon,               // Sun at highest daily elevation (transit)
    Nadir,                   // Sun at lowest daily elevation (solar midnight)

    // --- Horizon Transitions (Geometric Center / Upper Limb) ---
    Sunrise,                 // Sun crossing horizon in morning
    Sunset,                  // Sun crossing horizon in evening

    // --- Twilight Phases ---
    CivilDawn,               // Sun at -6° (morning)
    CivilDusk,               // Sun at -6° (evening)
    NauticalDawn,            // Sun at -12° (morning)
    NauticalDusk,            // Sun at -12° (evening)
    AstronomicalDawn,        // Sun at -18° (morning)
    AstronomicalDusk,        // Sun at -18° (evening)

    // --- Photographic Light Windows ---
    GoldenHourMorningStart,  // Sun at -4° rising
    GoldenHourMorningEnd,    // Sun at +6° rising
    GoldenHourEveningStart,  // Sun at +6° setting
    GoldenHourEveningEnd,    // Sun at -4° setting

    BlueHourMorningStart,    // Sun at -6° rising
    BlueHourMorningEnd,      // Sun at -4° rising
    BlueHourEveningStart,    // Sun at -4° setting
    BlueHourEveningEnd,      // Sun at -6° setting

    // --- Dynamic / Custom Zenith Triggers ---
    FixedElevation {
        degrees: f64,
        direction: CrossingDirection,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LunarEvent {
    // --- Horizon Transitions & Daily Extrema (diurnal position — see note) ---
    Moonrise,
    Moonset,
    LunarTransit,       // Moon at highest daily elevation ("lunar noon")
    LunarAntitransit,   // Moon at lowest daily elevation ("lunar midnight" / nadir)

    // --- Phase Events (synodic/monthly cadence, not daily — see note) ---
    NewMoon,
    FirstQuarter,
    FullMoon,
    LastQuarter,
    // No LunarFixedElevation/LunarDirection for v1 — deliberately scoped
    // out, see the implementation notes below.
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum MilkyWayEvent {
    /// Galactic Center crosses the horizon, rising.
    CoreRise,
    /// Galactic Center crosses the horizon, setting.
    CoreSet,
    /// Galactic Center at its highest point for the night ("core transit").
    CoreTransit,
    // No CoreAntitransit/"CoreNadir" — see implementation notes: nobody
    // schedules around the core's lowest point, unlike Solar's `Nadir`.

    /// Custom elevation-crossing threshold for the Galactic Center —
    /// same escape-hatch role `SolarEvent::FixedElevation` plays for the
    /// sun (e.g. a practical "usably visible above the local horizon
    /// obstruction" altitude like 10-15°, rather than the literal 0° of
    /// `CoreRise`/`CoreSet`).
    CoreElevation {
        degrees: f64,
        direction: CrossingDirection,
    },

    /// Fires when the core's azimuth (compass bearing) crosses a target
    /// value — for framing/composition against a specific foreground
    /// alignment, not a visibility check.
    Orientation {
        azimuth_degrees: f64,
    },
}
```

`Rule.constraints: Constraints` is the second, independent half of the
data model — orthogonal to `Trigger`, not a variation on it. A trigger
decides *when* raw candidate instants get generated at all; each
populated field on `Constraints` is then applied as an independent
pass/fail filter over those instants (`constraints.holds(when)` — every
populated field must hold, not just one). **Deliberately a struct with one
named `Option<T>` field per constraint type, not `Vec<Constraint>`** — see
the resolved decision in §14 for why: it makes "at most one of each
constraint type per rule" a structural, compile-time guarantee rather than
something a runtime validator has to catch, since duplicating a type would
be redundant anyway (two `SunElevationWindow`s, say, could only narrow or
exactly duplicate each other under AND-composition, never add anything a
single range can't already express). All five fields are
implemented (`time_window` in Phase 1, the celestial windows in Phase 2;
`src/optic_scheduler.rs`), each added as its own named field when it was
implemented rather than stubbed out in advance. They're introduced alongside
`Trigger`'s celestial targets above, rather than scattered later in this
document, so the full data model reads as one piece:

```rust
struct Constraints {
    time_window: Option<TimeWindow>,
    sun_elevation_window: Option<SunElevationWindow>,
    moon_elevation_window: Option<MoonElevationWindow>,
    moon_illumination_window: Option<MoonIlluminationWindow>,
    milky_way_elevation_window: Option<MilkyWayElevationWindow>,
}

struct TimeWindow {
    days: RecurringDays,
    start: NaiveTime,
    end: NaiveTime,                // supports overnight windows (start > end wraps past midnight)
}
struct SunElevationWindow {
    min_deg: f64,
    max_deg: f64,
}
struct MoonElevationWindow {
    min_deg: f64,
    max_deg: f64,                  // e.g. { -90°, 0° } = "moon below horizon"
}
struct MoonIlluminationWindow {
    min_pct: f64,
    max_pct: f64,                  // 0.0-100.0; e.g. { 0°, 20° } = dark-sky window
}
struct MilkyWayElevationWindow {
    min_deg: f64,
    max_deg: f64,                  // e.g. { 10°, 90° } = "core usably above the horizon"
}
// future: DateExclusion (holidays) — not designed here, out of scope (§12)
```

**Implementation notes on `SolarEvent`** (this specific enum is written
closer to real Rust than the rest of this doc's pseudocode, at the user's
request — worth keeping that precision when this becomes actual code):

- **Occurrence count per day is not uniform.** Every named variant fires
  at most once per calendar day, *except* `FixedElevation{direction:
  Both}`, which fires up to twice (once per crossing). §3's `occurrences()`
  must not assume "one Ephemeris occurrence per rule per day" — that
  assumption holds for every variant except this one case.
- **Named events must use a real solar library's corrected values, not a
  naive elevation-crossing search.** `Sunrise`/`Sunset` in particular are
  conventionally defined by the sun's *upper limb* crossing the horizon
  with atmospheric refraction accounted for (≈ -0.833° geometric center,
  not exactly 0°) — reimplementing them by hand as `FixedElevation(0.0)`
  would be subtly wrong. Whatever solar crate is chosen (§11) needs to
  either expose dedicated sunrise/sunset/twilight functions directly, or
  its raw elevation calculation needs to be composed with the correct
  standard offsets for each named tier (-6°/-12°/-18° for civil/nautical/
  astronomical, the golden/blue-hour boundaries above) — confirming the
  chosen crate actually covers all of these (nautical/astronomical
  twilight in particular) is real implementation-time due diligence, not
  a given.

**Implementation notes on `LunarEvent`** (why it isn't just "`SolarEvent`
again, but for the moon" — the mechanics genuinely differ):

- **The four diurnal position events can legitimately occur zero times,
  or occasionally twice, on a given calendar day.** The lunar day
  (~24h50m) is longer than the solar day, so the moon routinely fails to
  rise *or* fails to set within some 24h calendar window (it's already up,
  or already down, for the whole day) — unlike solar sunrise/sunset,
  which are effectively daily-guaranteed outside polar latitudes. §3's
  day-scan must treat "no occurrence today" as the ordinary case for
  these four variants, not a bug to work around.
- **The four phase events are monthly, not daily, and need a different
  search strategy.** They recur on the ~29.53-day synodic cycle (the
  Sun-Moon-Earth angle crossing 0°/90°/180°/270°). Finding "the next full
  moon" is a "scan forward days-to-weeks for an angle crossing," not the
  "scan today for a horizon/elevation crossing" logic the diurnal events
  and all of `SolarEvent` use. These two `LunarEvent` groups cannot share
  one occurrence-computation code path the way every `SolarEvent` variant
  can share one.
- **No lunar analog to `FixedElevation`/`CrossingDirection` for v1 —
  deliberate, not an oversight.** Solar's fixed-elevation trigger earns
  its place from real photography conventions (the golden/blue-hour
  thresholds above are literally defined that way). An arbitrary "moon
  crossing 23°" has no equivalent forcing use case yet. Adding
  `LunarEvent::FixedElevation{degrees, direction: CrossingDirection}` later
  (the same `CrossingDirection` type is directly reusable — direction is
  direction, regardless of body) is a small, contained addition if a real
  need shows up, not a re-architecture.
- **`spa` (already flagged for solar) is very likely solar-only** — the
  name itself ("Solar Position Algorithm") suggests it has no lunar
  ephemeris or phase calculation at all. Supporting `LunarEvent` almost
  certainly needs a second, separate crate (or a hand-rolled lunar
  position/phase calculation) — this needs real implementation-time
  research, not an assumption that "whatever solar crate we pick also
  does the moon."
- **Illumination (`MoonIlluminationWindow`, introduced above)
  and the phase events above likely share one underlying primitive.** Illumination fraction is a continuous function
  of the same Sun-Moon-Earth angle the phase events are named points on —
  an implementation computing "current illumination %" and "next full
  moon" probably wants to share that angle calculation, the same way
  `SunElevationWindow` and `SolarEvent` both lean on one shared elevation
  primitive for the sun.

**Implementation notes on `MilkyWayEvent`** (mechanically the *simplest*
of the three target types, despite sounding the most exotic):

- **The Galactic Center has essentially fixed coordinates** (right
  ascension/declination) — it isn't orbiting anything nearby the way the
  Moon orbits Earth, so unlike `SolarEvent`/`LunarEvent` this needs no
  orbital ephemeris at all. Its rise/set/transit at a given `Station` is
  plain spherical astronomy from local sidereal time — the same category
  of calculation as "when does this fixed star rise," standard textbook
  formulas. This is plausibly hand-rollable without a dedicated crate,
  unlike Lunar — worth actually attempting before reaching for a
  dependency, the reverse of §11's Lunar risk note.
- **Because its position is fixed, its rise/set time drifts predictably
  by sidereal-vs-solar-day difference (~4 min earlier per day, ~24h/year)**
  — no equation-of-time wobble, no monthly phase cycle. Simpler to
  forecast than either Solar or Lunar.
- **"Milky Way season" needs no special modeling.** The core is only
  visible at night for part of the year (roughly Feb-Oct at northern
  mid-latitudes); composing `MilkyWayElevationWindow`
  (introduced above, alongside `Trigger`) with the existing solar
  dark-sky `SunElevationWindow` automatically
  produces zero occurrences in the off-season — the same "composition
  already handles it" property §4 demonstrated for tiered/overlapping
  rules, not a new mechanism.
- **`Orientation`'s azimuth is assumed to cross a given value once per
  visible arc, not twice** (unlike elevation, which rises then falls,
  crossed twice) — the core's azimuth sweeps in one direction across a
  single night's arc at practical observing latitudes. Flagged as an
  assumption to verify once implemented, not a certainty — extreme
  latitude edge cases aren't derived here.
- **No `CoreAntitransit` ("Core Nadir")** — deliberately asymmetric with
  `SolarEvent::Nadir`. Nobody schedules a timelapse around the galactic
  core's lowest point; it's a real omission for Solar (solar midnight is
  a legitimate anchor for some use cases) but not one here.
- **A missing continuous constraint, addressed above:** the reference this
  was based on only offered discrete instant-events (rise/set/transit/
  elevation-crossing) — with no way to say "keep shooting *while* the
  core is up," which is the actually-common desire for a Milky Way
  timelapse, not a single shot at the moment of rise.
  `MilkyWayElevationWindow` (introduced with the rest of the data model,
  above) fills this, mirroring `SunElevationWindow`/`MoonElevationWindow`
  exactly.
- **Generalizes cheaply later if wanted:** nothing here is Milky-Way-
  specific *math* — any fixed RA/Dec target (a specific bright star, the
  Andromeda Galaxy, etc.) would use identical rise/set/transit/elevation
  logic, just parameterized by coordinates instead of the Galactic
  Center's hardcoded ones. Not needed now; flagged since it's a small
  future step, not a re-architecture, same as Lunar's deferred
  `FixedElevation`.

`ScheduleConfig` becomes a new field on the existing `AppConfig`
(`src/camera.rs`), alongside `profile`/`settings`, and rides the same
commit/discard staging flow (§9) — but that flow's *underlying storage*
does need to change, and a second small durable file is added
specifically for run state. See §2.1.

### 2.1 Reboot-Durable Run State & SD-Card-Friendly Caching

The user's requirement — start survives reboot as running, pause
survives reboot as paused — surfaced a real, pre-existing gap while
checking how to satisfy it: `config.json`/`preview_config.json` currently
live on `/mnt/capture`, which is a **256 MiB tmpfs** (confirmed live on
the Pi: `findmnt` reports `tmpfs`, and after this session's earlier
reboot tests, both files are simply gone — RAM-backed, wiped on every
real reboot). This means *no* committed config — not just anything
schedule-related, but the existing profile/settings too — actually
survives a real hardware reboot today, despite
`worklogs/2026-09-17-timelapse-settings-persistence.md`'s title claiming
otherwise; that worklog only ever validated daemon *process* restarts and
browser refresh, never a real reboot. User-confirmed fix direction:
relocate the durable config to the already-persistent
`~/.local/state/optic-daemon/` directory (real ext4 SD-card storage,
already used by `optic_capture_log`'s `history.db` for exactly this
reason, already in the systemd unit's `ReadWritePaths` — no sandboxing
change needed).

**But `status()` (`src/web.rs`) currently does a raw
`tokio::fs::read_to_string` on the config path on *every* call — and
it's polled every 3s by the dashboard, per browser tab.** That's free on
tmpfs (RAM) today; relocated to the real SD card with no other change,
it becomes genuine flash reads every 3 seconds, indefinitely, on an
always-on device — exactly the SD-card wear/read concern the user raised.
The fix is a read-through cache, not a bare relocation:

- **Durable source of truth (SD card, `~/.local/state/optic-daemon/`,
  written rarely):**
  - `config.json` — `AppConfig` (`profile`/`settings`/`schedule.station`/
    `schedule.rules`) — written only on `POST /api/config/commit`, an
    infrequent, deliberate operator action.
  - `schedule_run_state.json` — just the `ScheduleRunState` (§2) —
    written only on `POST /api/schedule/pause`/`resume` (new, immediate-
    action endpoints, mirroring `/api/sync/{pause,resume}` — **not**
    staged through preview/commit/discard).
  - **Deliberately two separate files, not one, to avoid a real race
    between them**: if run state lived inside the same staged
    `AppConfig`, an operator with an in-progress, uncommitted rule edit
    (forked from an older config) who then paused/resumed the scheduler,
    then finally hit "commit" on their rule edit, could silently revert
    the pause/resume — commit would overwrite the whole file from a
    preview snapshot that predates it. Two independent files with two
    independent write paths make that impossible by construction, rather
    than requiring `commit_config` to become field-aware about which
    parts of the file it's allowed to touch.
- **Fast read-through cache (tmpfs, `/dev/shm/optic-daemon/`, read
  constantly, written only in lockstep with the durable files above):**
  mirrors both durable files. Hydrated once at daemon startup (read the
  SD card exactly once per boot, not per request) and updated write-
  through immediately after every durable write (durable file written
  first, then its cache mirror — if the daemon crashes between the two,
  the cache is merely stale, never ahead of a value that was never
  actually durably committed; the next boot re-hydrates it correctly
  regardless). **Every frequent/polled read — `status()` included —
  reads only this tmpfs cache, never the SD card directly.** Net result
  vs. today: zero added SD-card read load (all hot-path reads stay
  RAM-speed, exactly as now), and SD-card writes only happen on rare,
  deliberate actions (commit, discard, pause, resume) — negligible wear.
- **`preview_config.json` is unaffected by any of this** — it stays
  exactly on `/mnt/capture` (tmpfs), exactly as today. It's disposable
  staged-edit state by design (a live-preview slider drag writes to it
  constantly while calibrating); it never needs to survive a reboot, and
  its *existing* high write frequency is precisely why leaving it on
  tmpfs is correct, not a compromise being made here.
- **Boot sequence**: read `schedule_run_state.json` from the SD card
  once at startup (alongside `config.json`); if `Running`, the scheduler
  actor (§5) starts actively ticking against the durably-loaded rules
  immediately — no operator action needed after a reboot. If `Paused`,
  it starts idle and waits for an explicit `/api/schedule/resume`.

## 3. Composition Semantics

A **schedule** is not a materialized calendar; it's a pure function:

```rust
fn occurrences(
    schedule: &ScheduleConfig,
    from: DateTime<Tz>,
    horizon: Duration,
) -> Vec<ForecastedShot>   // { at: DateTime<Tz>, rule_id: RuleId, rule_label: String }
```

For each enabled rule, compute its own occurrences in `[from, from +
horizon)` (a `Trigger::Interval` yields a regular sequence; `RecurringTime`
yields the next matching wall-clock instants; `Ephemeris` requires a solar
calculation *per calendar day* in the window, not a cached single value —
sunrise/sunset shift by roughly a minute a day). Filter each rule's raw
occurrences against its own `constraints` (all must pass). Union every
rule's surviving occurrences into one timeline, sorted by time.

### 3.1 Rule Conflicts: Merge, Don't Suppress

When two or more rules' occurrences land within a small merge window of
each other (proposed: 5s — shorter than any real capture takes, so it's
physically impossible for the sensor to have honored them as separate
shots anyway), they collapse into **one physical capture, tagged with
every rule that contributed to it** — not a priority system where one
rule wins and the other's request is silently dropped. This was the
user's own resolution to the conflict question, and it's a better answer
than picking a winner: nothing is ever lost, and the record stays
truthful about *why* a shot exists.

- **Tag = the rule's own `slug` field — user-entered, not derived.**
  Every rule requires a `slug` at creation time (§2); it's a separate
  field from the free-text `label`, not computed from it. This replaces
  the earlier "auto-derive from label, disambiguate on collision" idea:
  **no two rules may share a slug, full stop — enforced as a hard
  uniqueness constraint, not resolved after the fact.**
  - Validated against a filesystem-safe pattern (proposed:
    `^[a-z0-9][a-z0-9-]*$` — lowercase alphanumeric and hyphens only,
    must start with a letter/digit), independent of whatever the `label`
    contains.
  - Uniqueness is checked **case-insensitively** across *all* rules
    (enabled or disabled — a disabled rule keeps its slug reserved, so
    re-enabling it later can't silently collide with something created
    in the meantime) and enforced at both the rules editor (immediate
    inline feedback while typing, blocking "save rule" on a duplicate/
    invalid slug) and at `POST /api/config/commit` as the authoritative
    check (rejects the whole commit with an error naming the colliding
    rules if two ever reach it with the same slug — matching how the
    same endpoint already fails atomically on other invalid config
    today).
  - Every rule needs a slug even if the operator never expects it to
    merge with anything — a lone shot's filename should still name its
    triggering rule, not just merged multi-rule shots.
- **Tags are sorted deterministically** (alphabetically) before being
  joined into the filename — not by fire order, which is arbitrary for
  genuinely simultaneous rules — so the same combination always produces
  the same filename shape.
- **Capped**, so a pathological many-rules-collide case doesn't produce
  an unusably long filename: show the first 3 tags + `+N` for the rest
  (exact cap is an implementation detail, not a design blocker).
- **Filename shape**: manual/web UI captures keep the original convention,
  `testshot-<profile-slug>-<millis-timestamp>.{jpg,dng}`, unchanged.
  Scheduler-triggered captures use a distinct `scheduler-` prefix (§14
  decision #4, resolved 2026-09-20 — implemented opposite to this doc's
  original recommendation of keeping `testshot-` universally, at the
  user's explicit request), with tags inserted between the profile slug
  and the timestamp: `scheduler-<profile-slug>-<tag1>-<tag2>-<millis-timestamp>.{jpg,dng}`.
  `optic_capture_log.rs::derive_capture_id` treats the whole basename as
  opaque (only strips the extension), so neither the prefix nor the tag
  insertion needed any change there — confirmed by reading it, not
  assumed. `optic_sync.rs::is_capture_filename` **does** need both
  prefixes allowlisted, and does — the one required change that comes
  with introducing a second prefix.
- **The capture-log record should also carry the tag list as structured
  data** (e.g. a `triggered_by: Vec<String>` field alongside the existing
  per-capture record), not only as filename text — so the dashboard can
  show/filter "what triggered this shot" without re-parsing filenames.
- **What this does *not* solve**: two rules with different cadences can
  still both be "active" over a long overlapping window (e.g. a 30s rule
  and a 5min rule both satisfied for two hours) without their individual
  occurrences ever landing inside the same 5s merge window — most shots
  in that overlap stay separately attributed, and the *combined rate* is
  simply higher than either rule alone implies. That's a genuine
  possibility, not a bug, and it's what the Forecaster's overlap advisory
  (§8) is for: surfacing it to the operator before they discover it as an
  unexpectedly full tmpfs.
- **A rule whose constraints can never simultaneously hold** (e.g. a
  `TimeWindow` and an `SunElevationWindow` that never overlap for this
  station) silently produces zero occurrences forever. The Forecaster
  (§8) should flag any enabled rule with zero occurrences in its horizon
  as a likely misconfiguration.

**This same function powers three things**, deliberately kept as one
implementation rather than three:

1. The live scheduler actor's "when do I wake up next?" (§6) — called with
   a short horizon (e.g. until the next occurrence) each time it needs to
   arm its next tick.
2. The Shot Forecaster's next-48h table (§8) — called with a 48h horizon
   for display.
3. The Storage/Bandwidth Forecaster (§8) — consumes the same occurrence
   list, just multiplies by an estimated per-shot size.

Keeping this as a single pure, deterministic function (no hidden state,
no I/O) makes it fully unit-testable without hardware or wall-clock
mocking tricks beyond passing in a fixed `from` — see §13.

`Interval`'s `align_to_wall_clock` (default true) means "every 5 minutes"
produces `:00, :05, :10, ...` rather than a sequence offset by whatever
second the rule was created or the daemon last restarted at — this keeps
the forecast stable and predictable across restarts/edits, which matters
for a table the user is meant to actually read and trust.

## 4. Worked Example: The Original Design Is Just Three Composed Rules

To validate the architecture actually covers the original spec (not just
the new asks), here's the old "solar-adaptive" single-mode design
(golden-hour/daytime/night interval bands) expressed as three ordinary
rules, with nothing special-cased:

| Rule | Trigger | Constraint |
|---|---|---|
| Golden hour | `Interval { every: 60s }` | `SunElevationWindow { -4°, +6° }` |
| Daytime | `Interval { every: 300s }` | `SunElevationWindow { +6°, 90° }` |
| Night | `Interval { every: night_interval }` | `SunElevationWindow { -90°, -4° }` |

(`-4°`/`+6°` matches the `GoldenHourMorningStart`/`GoldenHourEveningEnd`
boundaries in §2's `SolarEvent`, not the earlier ±6° this table used
before that enum was refined — kept in sync so the doc doesn't state two
different definitions of "golden hour.")

And the user's own "multi-rate/tiered" example ("1 frame/30s 08:00–16:00,
1 frame/15min off-hours") is two rules:

| Rule | Trigger | Constraint |
|---|---|---|
| Peak hours | `Interval { every: 30s }` | `TimeWindow { Weekdays(all), 08:00, 16:00 }` |
| Off hours | `Interval { every: 15min }` | `TimeWindow { Weekdays(all), 16:00, 08:00 }` (wraps past midnight) |

No "mode" enum, no per-mode code path — both are just data.

Neither example above actually overlaps (the constraints partition time
cleanly), so neither exercises §3.1's merge+tag mechanism. A case that
does: a "Weekday baseline" rule (`Interval { every: 5min }`, `TimeWindow`
Mon–Fri 08:00–18:00) and a one-off "Site visit" rule (`Interval { every:
30s }`, `TimeWindow` today only, 10:00–10:30) both enabled on a Tuesday.
Most of the Site Visit rule's 30s-spaced shots between 10:00–10:30 don't
land within 5s of the Weekday Baseline rule's 5-minute-spaced shots, so
they mostly stay separately attributed — but at (or near) 10:05, 10:10,
10:15, etc., the two rules' occurrences can land close enough to merge
into one shot tagged `weekday-baseline-site-visit`, and the combined rate
for that half hour is visibly higher than either rule states on its own
— exactly the situation §3.1's Forecaster overlap advisory exists to
surface up front.

The new lunar `Constraint`s compose the same way, with no new mechanism —
an astrophotography "dark-sky" rule is just:

| Rule | Trigger | Constraints |
|---|---|---|
| Dark-sky interval | `Interval { every: 60s }` | `SunElevationWindow { -90°, -6° }` (sun down, past civil dusk) **and** `MoonElevationWindow { -90°, 0° }` (moon below horizon) |

— or, wanting shots during a dim-but-present moon instead of moon-down
entirely, swap the second constraint for `MoonIlluminationWindow { 0°,
20° }`. Same `Rule`/`Trigger`/`Constraint` shape as every solar example
above; nothing lunar-specific needed at the composition level.

Adding the Galactic Center is the same pattern again — a "Milky Way
arch" rule needs no new mechanism either, just one more composed
constraint:

| Rule | Trigger | Constraints |
|---|---|---|
| Milky Way arch | `Interval { every: 30s }` | `SunElevationWindow { -90°, -18° }` (astronomically dark) **and** `MoonIlluminationWindow { 0°, 20° }` (dim/no moon) **and** `MilkyWayElevationWindow { 10°, 90° }` (core usably above the horizon) |

This rule automatically produces zero occurrences for the months when
the core never clears 10° during full darkness at this station's
latitude — "Milky Way season" falls out of the composition for free,
per §2's `MilkyWayEvent` implementation notes, rather than needing a
season concept built in anywhere.

## 5. Runtime Engine (`optic_scheduler.rs`)

Same bounded-actor shape as `optic_camera` and `optic_sync` (a cheap
`Clone` handle over an `mpsc::Sender` + `watch::Receiver`, one owning
`tokio::task`) — consistent with the rest of this codebase, not a new
concurrency style.

- Startup: load `ScheduleRunState` from its durable file (§2.1). If
  `Paused`, the actor starts idle — no ticking, no captures — until an
  explicit `/api/schedule/resume`. This is what makes "paused, then
  reboot, stays paused" and "running, then reboot, resumes running"
  actually true, not just a config value that happens to be remembered.
- Loop (only while `Running`): compute `occurrences(schedule, now,
  until_next)` to find the next due instant, `tokio::time::sleep_until`
  it (not a fixed-interval poll — this scales fine even with rules
  firing every 30s, and trivially handles a rule that only fires once a
  day without busy-waiting).
- `pause`/`resume` commands (from the new immediate-action endpoints,
  §2.1) transition the actor between idle and ticking, and durably
  persist the new state (SD card, then its tmpfs cache) before
  acknowledging the request — an operator who pauses and immediately
  reboots must not lose that pause to a write that hadn't landed yet.
- On wake: re-read `AppConfig` (hot-reload, same as the original design
  called for) in case it changed since the sleep was armed — a rule edit
  mid-sleep should take effect on the next wake, not require a restart.
- Exposure (added 2026-09-21): in `AutoRamp` mode the capture's
  shutter, gain and colour gains come from the one global ramp state,
  which every scheduled frame's metadata and meter update; the shutter is
  also capped by the gap to the following shot. Merged rules share that
  one exposure by construction. See
  `docs/optic-daemon-exposure-ramping.md` §5–§7.
- Fire: gather every rule whose occurrence falls within the §3.1 merge
  window of the due instant (not just the one rule that triggered the
  wake), collect their `slug`s, and call
  `camera.capture_to_stage(&capture_dir, request)` **once** with those
  tags attached to the request — the same call `POST /api/capture`
  already makes, just carrying an extra `tags: Vec<String>` the filename-
  building code in `native_camera.rs` needs to thread through (§3.1).
  Per yesterday's worklog, this call already stops any active preview
  automatically and the frontend already self-heals, so **no new
  hardware-arbitration logic is needed** regardless of how much richer
  the scheduling logic above gets — this finding is unaffected by moving
  from "fixed interval" to "composable rules," since it was always about
  the capture call itself, not what decides when to make it.
- Record: publish a status snapshot (`run_state`, next capture time +
  which rule, last capture result/time/rule, running total) the same way
  `optic_sync`'s `SyncStatus` is published, folded into `GET
  /api/status` — reading the §2.1 tmpfs cache, never the SD card
  directly, same as every other frequent read.

## 6. DNG: Follows the Committed Preference (Confirmed; revised 2026-09-20, twice)

Scheduled captures request DNG according to a per-profile policy in
`optic_scheduler.rs::fire_capture`:

- `Binning2k` never includes a DNG — `validate_raw_policy` (`src/camera.rs`)
  forbids it outright, unchanged all along.
- `MasterArchive` and `Dci4k` both read the committed `AppConfig.save_dng`
  — i.e. a scheduled capture uses whatever was last **saved** on the
  dashboard, exactly like every other camera setting (`profile`,
  `settings`) already works. `MasterArchive`'s companion DNG changed from
  mandatory to a default-on preference earlier the same night
  (`validate_raw_policy` no longer rejects `save_dng: false` for it), and
  briefly (within this same session) `fire_capture` kept it hardcoded
  `true` regardless of `config.save_dng` — because at that point nothing
  in the UI could actually *set* `AppConfig.save_dng` to anything but its
  `false` default, so reading it would have silently dropped the DNG from
  every scheduled frame. That gap is now closed:
  `POST /api/config/save-dng` (`src/web.rs`) gives the dashboard's DNG
  checkbox a real staged/committed path (its own endpoint, not piggybacked
  on `reconfigure_stream`, since saving a DNG has nothing to do with the
  live preview pipeline and that path only stages while a preview is
  actually running). With that in place, reading `config.save_dng` for
  `MasterArchive` is safe and correct — "Save Settings" now means the
  same thing for every setting, DNG included.

There is no separate "scheduler DNG" toggle — it's the same committed
preference the dashboard checkbox stages. This means the Storage
Forecaster (§8) must surface a clear warning when `MasterArchive` with
DNG enabled (~30 MB/shot measured) is paired with an aggressive rule
set, since that combination is what actually drives tmpfs risk; unlike
before, that combination is no longer unconditional — a user who saved
`save_dng: false` for Master Archive gets the smaller (~7.8 MB/shot)
estimate instead, same as the forecast already computes per
`web.rs::estimated_bytes_per_shot`.

## 7. Full-Disk Behavior — Resolved

`/mnt/capture` is a bounded 256 MiB tmpfs. Yesterday's worklog measured
~8 captures' worth of headroom at MasterArchive+DNG, vs. thousands at
Binning2k JPEG-only. **Confirmed: skip the tick and log/report it** —
never miss the *next* opportunity, don't jam more into a full disk.
(The other two options considered — blocking/retrying, or deleting the
oldest un-synced file to make room — are not used.)

The Storage Forecaster (§8) changes the shape of this decision somewhat:
it gives the *operator* proactive visibility ("this rule set will fill
the tmpfs in ~40 minutes if sync stalls") *before* they commit a
dangerous rule set, which reduces how often the runtime policy actually
gets exercised — but doesn't eliminate the need for one (sync can still
stall unexpectedly at any time). Skip+report is the backstop; the
forecaster's warning is the primary defense.

## 8. Shot Forecaster & Storage/Bandwidth Forecast (UI)

A read-only panel below the rules editor, computed from `occurrences()`
(§3) run over the next 48h against the *currently staged* config (§9 —
including uncommitted edits, so users see the effect of a rule change
before committing it):

- **Shot table:** exact timestamp + which rule(s) fired, for the next
  48h. A merged shot (§3.1) shows all its contributing rule tags on one
  row, not two adjacent rows — the table must count physical captures,
  not raw per-rule occurrences, or the frame count below would
  over-count. Long lists should probably page or cap-and-summarize (e.g.
  "312 more shots today" for a 30s-interval rule) rather than render
  thousands of rows — an implementation detail to work out, not a design
  blocker.
- **Frame Count & Temporal Cadence:** total *physical* shot count in the
  window (post-merge, per the note above), min/max/average spacing
  between consecutive shots. Doubles as a sanity check — a misconfigured
  rule that fires every second is immediately obvious here rather than
  discovered live on hardware.
- **Storage & Bandwidth Forecast:** per-shot size estimated from the
  committed capture profile (reusing yesterday's measured numbers as a
  starting table: MasterArchive+DNG ≈ 30 MB, Dci4k ≈ TBD/measure,
  Binning2k JPEG-only ≈ 90–95 KB) × forecasted frame count = total bytes
  over the window, plus a bytes/hour rate. Cross-referenced against the
  256 MiB tmpfs capacity to show an explicit warning banner when the
  projected fill time is short enough to be a real risk (e.g. "fills in
  ~40 min at this rate if sync stalls") — this is the proactive-warning
  half of §7's answer, not a substitute for the runtime policy.
- **Overlap advisory:** when two or more *enabled* rules are both
  satisfied over a shared span of time (not just a single merged instant
  — §3.1's "what this does not solve" case), flag it: "Weekday Baseline
  and Site Visit are both active 10:00–10:30 today; combined rate in
  that window is ~1 shot/28s." Gives the operator the same visibility
  into sustained-overlap situations that §3.1's merge+tag gives for
  simultaneous ones.
- **Dead-rule advisory:** any *enabled* rule that produces zero
  occurrences across the full forecast horizon is flagged as a likely
  misconfiguration (contradictory constraints, e.g. a `TimeWindow` and
  `SunElevationWindow` that never both hold for this station) — per §3.1.

## 9. Config Editing: Resolved — Reuses the Existing Preview/Commit/Discard Flow

`AppConfig` (profile + settings) is already edited through a staged
preview: client-side changes write to `preview_config.json` (via the
existing live-editing path), and `POST /api/config/commit` /
`/api/config/discard` finalize or revert. **Confirmed:** `ScheduleConfig.
rules` (add/edit/remove/reorder/enable-disable) goes through the exact
same flow — the rules array is just another field in the same staged
object — rather than dedicated `POST/PUT/DELETE
/api/schedule/rules/{id}` endpoints. This means:

- Add/edit/remove all happen client-side against the staged config; the
  Shot Forecaster (§8) reads whatever is currently staged, so edits are
  visible in the forecast immediately, before committing.
- Discard reverts rule edits exactly like it already reverts profile/
  settings edits — no new discard logic needed.
- One API surface to reason about for *editing*, not two parallel
  mechanisms.

**Explicitly not part of this flow: `ScheduleRunState`.** Per §2.1,
start/pause/resume are immediate actions against their own separate
durable file, never staged, never subject to discard — clicking "pause"
takes effect now, the same way `/api/sync/pause` does, not "after you
also hit commit." This is deliberate, not an inconsistency: it's what
avoids the commit-clobbers-a-pause race §2.1 describes.

## 10. UI Placement — Resolved: Dedicated Page

Yesterday's worklog decided the (then much smaller — one enabled toggle +
one interval field) schedule panel would be a nested section inside the
existing merged `.controls-card`. Given the actual scope now (a rules
editor plus a two-part Forecaster), the user opted to skip both the
nested-section idea *and* a standalone-card-on-the-main-dashboard idea in
favor of a genuinely separate page.

This is cheaper than it sounds in this codebase specifically: `/{*asset}`
(`src/web.rs`) already serves *any* file dropped into the asset
directory dynamically, from disk, on every request — no new Rust route
or handler needed. A `scheduler.html` (plus its own script, see below)
just works by existing at `src/web/scheduler.html`, servable at
`/scheduler.html`.

- **New page**: `src/web/scheduler.html` — the rules editor (§9) and the
  Shot Forecaster / Storage Forecast (§8). Not a fragment injected into
  the main dashboard; a real page with its own `<title>`/layout,
  reachable via a nav link from the main dashboard.
- **New script**: recommend a separate `src/web/scheduler.js` rather than
  growing the existing `app.js` with scheduler-only logic guarded behind
  DOM-presence checks — cleaner separation for a feature this size, and
  `app.js` stays focused on the camera/preview/system concerns it
  already owns. (Either is workable; this is the recommended default,
  not a hard requirement.)
- **The main dashboard keeps a small status summary, not the full
  editor** — matching how the System card already shows always-visible
  status alongside its own controls: something like "Scheduler: enabled,
  next shot in 4m12s — [Manage →]" linking to `/scheduler.html`. Keeps
  at-a-glance visibility on the page an operator is already looking at,
  without duplicating the rules editor there.
- **No config-flow complication.** §9's "reuse the existing preview/
  commit/discard flow" decision composes fine with a separate page —
  `preview_config.json` is server-side, authoritative state, not
  browser-held draft state, so navigating between the main dashboard and
  `/scheduler.html` doesn't lose an in-progress edit.
- **Two concrete follow-ups for whoever implements this**, both easy to
  miss because they're not in the obvious place: `scripts/build-deploy-
  optic-daemon.sh`'s Biome lint step (`biome check src/web/app.js
  src/web/index.html`) lists exact filenames, not a glob — it needs
  `scheduler.html`/`scheduler.js` added explicitly. The same script's
  post-deploy content verification (the `grep -F 'Preview responsiveness'`-
  style checks against `curl`'d `/app.js`/`/`) should probably gain an
  equivalent check against `/scheduler.html` once it has real content, to
  keep the "deploy verifies the thing it just shipped" property the rest
  of that script already has.

## 11. New Dependencies

- **`chrono` + `chrono-tz`**: nothing in this codebase currently does
  calendar-date or timezone-aware arithmetic (grep confirms only
  `std::time::{SystemTime, Instant, Duration}` are used anywhere today).
  `RecurringTime` rules need real calendar/DST-safe local-time handling —
  "every day at 13:00" must stay 13:00 local through DST transitions, not
  drift by an hour twice a year. The Pi's system timezone is currently
  `America/Vancouver` (confirmed via `timedatectl`, NTP-synced, RTC kept
  in UTC — good practice already in place); `Station.timezone` should
  default to the system zone but be stored explicitly in config rather
  than silently reading the OS zone at run time, so the schedule stays
  correct/self-describing even if the Pi's system timezone is ever
  changed independently.
- **A solar-ephemeris crate** (`spa`, as already named in the original
  design doc, or an equivalent) for `Ephemeris` triggers and
  `SunElevationWindow` constraints — must cover the full `SolarEvent` tier
  list in §2 (nautical/astronomical twilight included, not just civil),
  per that section's implementation notes. Not re-evaluated here in
  detail — that's an implementation-time task — but flagged since it's
  still a new dependency, same as originally scoped.
- **A lunar ephemeris/phase crate — separate research item, not assumed
  to be the same crate as the solar one.** `spa`'s name alone suggests
  it's solar-only. Needs a Rust crate (or a hand-rolled calculation)
  covering moonrise/set/transit/antitransit and lunar phase angle for
  `LunarEvent` and the two new `Moon*Window` constraints — genuinely
  unresearched as of this doc; flagged rather than guessed at.
- **`MilkyWayEvent` likely needs no new crate at all** — per §2's
  implementation notes, the Galactic Center's fixed coordinates make
  rise/set/transit/elevation/azimuth plain sidereal-time spherical
  astronomy, plausibly implementable directly rather than sourced from a
  library. Still an implementation-time thing to actually attempt before
  assuming, but the risk direction is opposite Lunar's — cheaper than
  expected, not a new dependency to research.
- **Recurrence representation — purpose-built enum, not cron/RRULE**:
  considered and rejected two standard alternatives. Plain 5-field cron
  can't express "every 2nd Wednesday" without non-standard extensions.
  RFC 5545 RRULE can express it, and there are Rust crates for it, but
  it's a heavier general-purpose recurrence grammar than this project
  needs, and its own syntax isn't meaningfully friendlier for a UI to
  round-trip than a purpose-built `RecurringDays` enum covering exactly
  the cases in §2 (`Every` / `Weekdays(...)` / `NthWeekdayOfMonth{n,
  weekday}`). The narrower enum is easier to validate, easier to render
  as UI controls, and easier to forecast deterministically. If genuinely
  richer recurrence is needed later, swapping in an RRULE crate is a
  contained change (it would replace `RecurringDays`'s implementation,
  not the `Trigger`/`Rule`/composition architecture around it).

## 12. Explicitly Out of Scope for This Design

- Holiday/date-exclusion lists (`DateExclusion` constraint) — noted as a
  plausible future `Constraint` variant, not designed in detail here.
- Any remote-authoritative config / config-mirroring-to-remote concept —
  same as both prior worklogs deferred.
- Implementation-level UX details (exact rule-editor form layout, table
  pagination for the shot forecaster, etc.) — belong in the
  implementation worklog once the decisions below are confirmed.

## 13. Testability

The core win of the pure-function design (§3): `occurrences()` takes no
hardware, no I/O, and no camera — given a fixed `ScheduleConfig` and a
fixed `from` instant, its output is fully deterministic and assertable
exactly, the same way `optic_sync`'s `ActorState` is unit-tested without
a real transport. This means the bulk of the scheduler's real complexity
(trigger math, constraint filtering, composition/dedup, the §4 worked
examples) is testable on macOS with no Pi, no camera, no wall-clock
mocking beyond passing in a fixed `from`. Only the final
`capture_to_stage` call and the actual tick-sleep loop need Pi-native/
integration testing. A full test plan belongs in the implementation
worklog, not here, but this property is worth stating up front since it
substantially de-risks a feature this much larger than the original v1
scope.

## 14. Open Decisions Requiring Confirmation Before Implementation

**Resolved:** rule-conflict resolution (§3.1) — merge simultaneous
occurrences into one physical capture, tag the filename with every
contributing rule's slug, never suppress/drop a rule's request. User's
own proposal, adopted in place of the priority-ordering idea originally
floated.

**Resolved:** tag/slug scheme (§2, §3.1) — `slug` is a required,
user-entered field on every `Rule` (separate from the free-text `label`),
validated against a filesystem-safe pattern and enforced globally unique
(case-insensitively, across enabled *and* disabled rules) at both the
rules editor and `POST /api/config/commit`. Replaces the earlier
"auto-derive from label, disambiguate on collision" idea — the user
asked for entered-and-unique instead of derived-and-disambiguated.

**Resolved:** `Trigger::Ephemeris` generalizes to any celestial body via
a new `CelestialTarget = Solar(SolarEvent) | Lunar(LunarEvent)` wrapper
(§2), and gains `LunarEvent` (moonrise/set/transit/antitransit + the four
phase events) and two new lunar `Constraint`s (`MoonElevationWindow`,
`MoonIlluminationWindow`). No `LunarEvent::FixedElevation` for v1 —
deliberately scoped out (§2's implementation notes), easy to add later if
a real need appears.

**Resolved:** `CelestialTarget` gains `MilkyWay(MilkyWayEvent)` — Galactic
Center rise/set/transit/custom-elevation/azimuth-orientation, plus a new
`MilkyWayElevationWindow` constraint (added beyond the original reference,
which only offered discrete events — needed for "keep shooting while the
core is up," §2's implementation notes). Renamed `SolarDirection` →
`CrossingDirection` throughout, since it's now shared by `SolarEvent`,
(deferred) `LunarEvent`, and `MilkyWayEvent` — no longer solar-specific
in practice, so the name shouldn't be either.

**Resolved:** full-disk runtime behavior (§7) — skip the tick, log/report
it. Forecaster's proactive warning is the primary defense; this is the
backstop.

**Resolved:** UI placement (§10) — a dedicated `/scheduler.html` page
(not a card on the main dashboard), with a small always-visible status
summary + link left on the main dashboard.

**Resolved:** rule editing surface (§9) — reuses the existing
preview/commit/discard config flow; no dedicated rule CRUD endpoints.

**Resolved:** recurrence representation (§11) — the purpose-built
`RecurringDays` enum, not cron or RRULE.

**Resolved:** `Rule.constraints` representation (§2) — a struct with one
named `Option<T>` field per constraint type (`Constraints`), not
`Vec<Constraint>`. Decided in chat after weighing both: a `Vec` plus a
commit-time runtime validator (the same pattern already used for rule-slug
uniqueness) would work, but only a named-optional-field struct makes "at
most one of each constraint type per rule" a genuine compile-time
guarantee — there is structurally nowhere to put a second `TimeWindow`.
Two of the same constraint type would be redundant anyway under
AND-composition (a second instance can only narrow or exactly duplicate
the first). Implemented immediately for the one constraint type that
exists (`time_window: Option<TimeWindow>`) rather than deferred — the
struct shape costs nothing to adopt now and avoids a later migration; the
four Phase 2+ fields are *not* stubbed out in advance, since there's
nothing to gain from `Option<T>` fields for types that don't exist as real
Rust types yet. Side benefit found while implementing: this sidesteps the
serde-tagging class of bug that already bit `RecurringDays::Weekdays`
once — named struct fields don't need a tagging strategy at all.

**Resolved:** `testshot-` filename prefix (§3.1) — reversed from this
doc's original recommendation. Scheduler-triggered captures now get a
distinct `scheduler-` prefix (`scheduler-<profile-slug>[-<rule-tags>]-<millis>.{jpg,dng}`),
not `testshot-` universally, at the user's explicit request (2026-09-20) —
a scheduler-fired production timelapse frame reading as a "test shot" was
worse than the cost of the one required follow-on change: widening
`optic_sync.rs::is_capture_filename`'s allowlist to accept both prefixes,
which is done. `CaptureRequest` gained a `source: CaptureSource` field
(`WebUi` default, or `Scheduler { rule_slugs }`) so `native_camera.rs`'s
`publish_capture` — the one place that actually names the file — can
make this decision directly, rather than threading it through as a
separate parameter. `optic_capture_log.rs::derive_capture_id` needed no
change (confirmed it treats the whole basename as opaque). Both items
originally left as follow-up here are now also done (2026-09-20, same
session as the Phase 2 work below):
`optic_capture_log.rs::failure_capture_id` now takes the real
`CaptureSource` and mints a `scheduler-`-prefixed synthetic id for a
scheduler-sourced failure (previously always `testshot-` regardless of
source), and `CaptureLogEntry` gained a `triggered_by: Vec<String>`
field (empty for `WebUi`, the firing rule slugs for `Scheduler`) —
`#[serde(default)]` so old `.log.json` files/database rows written
before this field existed still deserialize.

**Resolved:** merge-window duration (§3.1) — bumped from the originally
proposed 5s to **10s** (2026-09-20), after real measured capture
durations (`docs/optic-daemon-capture-performance.md`) turned out to be
~5.1-5.6s end to end — at or above the original "shorter than any real
capture" assumption, not safely under it. 10s gives real margin above
the slowest measured profile (MasterArchive+DNG, ~5.46s).

**Resolved:** Station config entry — a plain manual lat/long/elevation/
timezone form, no map picker, no IP-geolocation, confirmed sufficient.
Implemented as a "Station" card, originally on the dashboard (not the
Scheduler page — explicit user direction), staged through the same
shared preview/commit/discard flow every other config section already
uses. **Moved 2026-09-20** to a new dedicated `/config.html` page
(alongside a Time & NTP section) — see
`worklogs/2026-09-20-scheduler-phase2i-config-page-station-ntp-timezone.md`.
Timezone changed from free text to a `<select>` sourced from
`GET /api/timezones` (the backend's own `chrono_tz::TZ_VARIANTS`), fixing
a real latent bug: a typo'd free-text zone previously fell back to UTC
silently (`Station::tz`'s `.unwrap_or(chrono_tz::UTC)`), now structurally
impossible since only backend-recognized names are selectable.

**Resolved:** new dependencies (§11) — added exactly one new crate,
`astro` v2.0.0 (pure Rust, MIT, a Meeus' *Astronomical Algorithms*
implementation), covering **both** Sun and Moon — the "separate lunar
crate" research item this section flagged turned out unnecessary once
`astro` was actually evaluated, and its Galactic-coordinate support
(`coords::gal_frm_eq`) meant MilkyWay needed no dependency at all either,
confirming this doc's own suspicion. Full detail in
`worklogs/2026-09-20-scheduler-phase2a-ephemeris-engine.md`.

**Resolved:** `Trigger::Ephemeris` (§2) is implemented in full —
`SolarEvent`/`LunarEvent`/`MilkyWayEvent`, `CrossingDirection`, and the
four new `Constraints` fields (`sun_elevation_window`,
`moon_elevation_window`, `moon_illumination_window`,
`milky_way_elevation_window`), built on two shared generic search
primitives (threshold-crossing and extremum-finding) rather than three
separate per-taxonomy implementations, per this doc's own hint that any
fixed-or-moving RA/Dec target can share one rise/set/transit/elevation
engine. Verified live against the real Pi/Station (not just unit tests)
— a `Sunset` trigger, a `Moonrise` trigger, and a `CoreRise` trigger each
produced physically correct, night-to-night-consistent occurrence times,
including MilkyWay core rise showing the exact ~4-min/day sidereal drift
this doc's §2 implementation notes predicted. Rule-editor UI for all of
this (trigger type, event picker, the four constraint boxes) is also
done. Both phases fully documented in
`worklogs/2026-09-20-scheduler-phase2a-ephemeris-engine.md` and
`worklogs/2026-09-20-scheduler-phase2b-ephemeris-rule-editor-ui.md`.

**Resolved:** Storage/Bandwidth Forecaster (§8) — per-shot byte
estimates for all three profiles (including `Dci4k`, previously flagged
"TBD/measure" and now measured live against real hardware), a fill-time
projection counting up from the *actual current* `/mnt/capture` queue
(not zero), and a two-tier UI warning. Full detail, including the real
measured byte counts and the documented scene-dependence caveat for
JPEG sizing, in
`worklogs/2026-09-20-scheduler-phase2c-storage-forecaster.md`.

**Resolved:** dead-rule and overlap advisories (§8) — both pure
functions of the already-computed forecast (`dead_rule_slugs`,
`overlap_advisories` in `optic_scheduler.rs`), surfaced in the Shot
Forecaster UI. Detail in
`worklogs/2026-09-20-scheduler-phase2d-overlap-and-dead-rule-advisories.md`.
