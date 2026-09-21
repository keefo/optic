# Dated Worklog: 2026-09-20 - Phase 2j: System Timezone Sync, Celestial Times Preview, System-Status Footer

Status: **implemented, deployed, and verified.**

## Objective

Three follow-on requests after Phase 2i shipped the Config page:

1. User noticed changing Station timezone didn't update `timedatectl` —
   asked why, then chose to actually wire Station-timezone-save into the
   Pi's system clock timezone (rather than just clarifying the labels).
2. A new Config-page section previewing celestial times (sunrise/sunset
   etc.) for the currently-entered Station coordinates, plus "what other
   celestial times are worth showing?"
3. Footer redesign: first a copy change, then — mid-conversation — a
   bigger idea to fold the Dashboard's System card (Pi health + Restart/
   Reboot controls) into a compact, persistent footer on all four pages.

## Acceptance Criteria

1. Saving Station on the Config page also sets the Pi's system timezone
   (`timedatectl set-timezone`), via a newly-verified, narrowly-scoped
   PolicyKit rule — not the broad `manage-units` action.
2. A "Celestial times" section on the Config page shows real sunrise/
   sunset/twilight/golden-hour/blue-hour/Moon/Milky-Way-core times for
   whatever Station coordinates are currently in the form (including an
   unsaved edit), formatted in the selected Station timezone.
3. Every page shares one compact system-status + control footer (CPU
   temp, memory %, disk %, uptime, Restart daemon, Reboot Pi); the
   Dashboard's old detailed System card is retired.

## Research / Design Decisions

- `org.freedesktop.timedate1.set-timezone` is its own narrowly-scoped
  PolicyKit action (the same D-Bus method `timedatectl set-timezone`
  itself calls) — unlike the NTP-restart case, no `manage-units`
  detail-matching needed. Verified live (installed rule → `pkcheck`
  authorized → real `timedatectl set-timezone America/Vancouver` from
  inside a scratch unit matching the daemon's exact sandbox flags
  succeeded) *before* any Rust code was written, same discipline as
  every prior PolicyKit addition tonight.
- Celestial preview reuses `optic_scheduler.rs`'s existing
  `ephemeris_occurrences` (made `pub(crate)`) directly — zero new
  astronomy code, only new orchestration. It's a GET with lat/long/
  elevation as query params, deliberately *not* reading the committed
  `Station` from `AppConfig`: the whole point is previewing whatever is
  currently typed, unsaved edits included. `timezone` isn't a preview
  input — none of the ephemeris math depends on it (positions are
  computed in absolute UTC/Julian-day terms; `Station.timezone` only
  matters for the scheduler's local-wall-clock trigger types) — the
  frontend applies the Station's selected timezone purely for display
  via `Intl.DateTimeFormat`.
- Curated event list (not every `SolarEvent`/`LunarEvent`/`MilkyWayEvent`
  variant — the three parametrized ones need an explicit degrees/azimuth
  input this preview doesn't take): Sun (sunrise/sunset/solar noon/civil
  dawn+dusk/astronomical dawn+dusk/golden hour ×2/blue hour ×2), Moon
  (moonrise/moonset/transit/current illumination %+waxing-waning/next
  new+full moon), Milky Way (core rise/transit/set) — chosen because this
  project's own Ephemeris-trigger work this session was largely in
  service of Milky Way timelapses, and golden/blue hour are the two
  headline photography-specific windows. Daily-cadence events use a 48h
  search horizon (matches the Shot Forecaster's own convention);
  New/Full Moon use 35 days (consecutive same-phase events are ~29.5
  days apart).
- Footer: user explicitly wants compact (not the old card's full disk/
  network/capture-health breakdown) but wants the Restart/Reboot buttons
  kept. Implemented as one new shared `footer.js`, loaded via a second
  `<script>` tag on all four pages (the same sharing pattern
  `styles.css` already uses via `<link>`) rather than duplicating ~40
  lines four times — the one deliberate exception to this codebase's
  established "no shared JS module between pages" convention, since this
  widget is now used verbatim on every page.

## Test Plan

- PolicyKit: same live-verification discipline as the NTP rule (see
  Validation).
- `cargo test --locked`: two new tests for `celestial_preview` (finds
  real Sun/Milky-Way events for real Vancouver coordinates; rejects
  out-of-range lat/long) plus the existing full suite.
- `node --check` + Biome on every touched/new JS/HTML file, including
  the new `footer.js`.
- `bash -n` on the deploy script after editing its content-verification
  checks (removed the stale "View capture history" text check — that
  link moved into the nav tab bar — added checks for `app-tabs`,
  `system-footer`, and `footer.js`'s content instead).
- Live: stage+commit a Station timezone change, confirm `timedatectl`
  actually reflects it; load the Config page's celestial preview against
  real Station coordinates and sanity-check the numbers against
  `timedatectl`'s own local-time view; confirm the footer's stats/buttons
  work on all four pages; confirm the Dashboard no longer has a System
  card; scheduler untouched throughout.

## Implementation Summary

### PolicyKit (host config, outside the git repo)
- `/etc/polkit-1/rules.d/62-optic-daemon-set-timezone.rules` (new):
  grants `liam` passwordless `org.freedesktop.timedate1.set-timezone`.

### `src/system_status.rs`
- New `set_system_timezone(timezone: &str)`: `timedatectl set-timezone
  <tz>`, no sudo — same D-Bus/PolicyKit pattern as `reboot_host`/
  `sync_ntp_now`, doc comment cross-references both.

### `src/web.rs`
- `POST /api/system/timezone` (`SetTimezoneRequest { timezone }`):
  validates against `chrono_tz::TZ_VARIANTS` (extracted into a small
  testable `is_valid_timezone` helper) before ever invoking the OS
  command, then calls `set_system_timezone`.
- `GET /api/celestial-preview?latitude=&longitude=&elevation_m=`: builds
  an ad-hoc `Station`, computes the curated event list via
  `optic_scheduler::ephemeris_occurrences` (newly `pub(crate)`), plus
  current Moon illumination % and a waxing/waning trend (compares
  illumination now vs. +1 day — the crate has no built-in phase-trend
  helper). Validates latitude/longitude ranges before computing anything.
- `optic_scheduler.rs::ephemeris_occurrences` made `pub(crate)`.

### `src/web/config.js` / `config.html`
- New "Celestial times" card between Station and Time & NTP: grouped
  Sun/Moon/Milky Way `dl` lists, populated from the preview endpoint,
  refreshed on every Station field `change` (lat/long/elevation/
  timezone) and once on initial load. Shows a placeholder until
  latitude+longitude are filled in.
- Station's "Save station" handler: after a successful commit, also
  `POST /api/system/timezone` with the form's current timezone value
  (skipped if the Station was cleared) — new `controls-help` note under
  the timezone field states this plainly.

### Footer → shared system-status + control bar
- `src/web/footer.js` (new): polls `/api/system/status` every 15s,
  renders a compact one-line summary (`CPU °C · Mem % · Disk % · Up`),
  wires Restart daemon/Reboot Pi with the same confirm-dialog pattern
  the old dashboard buttons used.
- All four pages: `<footer class="system-footer">` (summary span +
  button row) + a second `<script src="/footer.js" defer>` tag.
- `src/web/index.html` / `app.js`: the old detailed System card (memory/
  disk usage bars, CPU temp, uptime, network interfaces, 24h capture
  health, the "View capture history" deep-link — now redundant with the
  nav tab bar) removed entirely, along with all its dedicated app.js
  code (`renderSystemStatus`, `setUsageBar`/`resetUsageBar`,
  `refreshSystemStatus`, `systemAction`, related element refs/listeners/
  polling). Dropped from view, not relocated: network interfaces and the
  24h capture-health rollup — the former is a rare diagnostic need, the
  latter now has a much richer home on the Capture History page built
  earlier tonight.
- `styles.css`: `.usage-bar*` rules removed (now fully unused);
  `.station-card`... already removed in 2i, unaffected here;
  `.system-card` removed from the two grid-column-placement selector
  lists (dead — the section no longer exists anywhere); new
  `.system-footer`/`.celestial-group` rules added.
- Footer copy (separate small request, landed before the bigger footer
  redesign): "Local network control plane" → "One Revolution Through
  Time" on Scheduler/Capture History/Config (echoing the Dashboard's own
  subtitle); Dashboard itself got "Project Optic" instead, to avoid
  showing that same phrase twice on one page (its header already has
  it) — both then superseded in content by the system-footer redesign,
  though the *tagline* text itself no longer appears in the footer at
  all now that it shows live stats instead. Mentioning for the record
  since it was a real, if short-lived, decision.

### `scripts/build-deploy-optic-daemon.sh`
- Added `footer.js` to the Biome gate.
- Replaced the stale "View capture history" dashboard-content check
  (that link no longer exists — superseded by the nav tab bar) with
  checks for `class="app-tabs"` and `class="system-footer"` on the
  served dashboard, plus a new `footer.js` content check
  (`/api/system/status`).

## Validation

- **PolicyKit, verified live before writing any Rust code**: installed
  `62-optic-daemon-set-timezone.rules`; `sudo pkcheck --action-id
  org.freedesktop.timedate1.set-timezone` → authorized; real
  `timedatectl set-timezone America/Vancouver` from inside a scratch
  `systemd-run --user` unit matching the daemon's exact sandbox
  (`ProtectSystem=strict`, `ProtectHome=read-only`, `NoNewPrivileges=true`,
  no sudo) → exit 0, `timedatectl show -p Timezone` confirmed the value.
- `cargo test --locked`: **109 passed, 0 failed** (2 new celestial-preview
  tests, `is_valid_timezone` test from the timezone-validation work).
- `cargo fmt --all -- --check` / `cargo clippy --locked --all-targets --
  -D warnings`: clean.
- `node --check` on every JS file; Biome across all 8 tracked web assets
  (including the new `footer.js`): clean (one line-width violation in
  `config.js` caught and auto-fixed with `biome check --write` before
  re-verifying clean).
- `bash -n scripts/build-deploy-optic-daemon.sh`: clean.
- Deployed via `./scripts/build-deploy-optic-daemon.sh`.
- **Live verification** against the real deployed Pi:
  - `GET /` confirmed: `system-footer` present, `app-tabs` present,
    `system-card` gone (0 matches), `footer.js` served (200).
  - `GET /config.html` confirmed the `celestial-card` section is present.
  - `GET /api/celestial-preview?latitude=49.2827&longitude=-123.1207&elevation_m=70`
    returned real, astronomically sane data for Vancouver — sunset
    02:12:07Z (≈19:12 PDT), matching within ~2 minutes of the ~19:14 PDT
    sunset independently confirmed back in Phase 2a's own live
    verification tonight; Moon 71% illuminated and waxing, consistent
    with "Next full moon" being only ~5-6 days out; all 23 curated items
    populated (`at` non-null for every one — no polar-day/night edge case
    at this latitude).
  - Full round-trip for the timezone-sync feature: staged Station
    timezone `America/Vancouver` → `America/New_York`, committed, called
    `POST /api/system/timezone`, confirmed via SSH `timedatectl show -p
    Timezone` → `America/New_York` (genuinely changed, not a no-op).
    Restored back to `America/Vancouver` the same way immediately after;
    confirmed via `timedatectl status` afterward that `NTP service:
    active` and `System clock synchronized: yes` were unaffected by the
    round trip.
  - `GET /api/system/status` confirmed real, sane values for everything
    the footer renders (CPU temp 40.6°C, memory/disk bytes, uptime,
    `time_sync`) — the footer's own click-through (Restart daemon/Reboot
    Pi) was not re-fired live, since those two actions were already
    verified end-to-end in earlier worklogs
    (`2026-09-19-reboot-nonewprivileges-fix.md`) and re-triggering a
    reboot here would be disruptive without new information to gain from
    it — only the new data plumbing feeding them was verified this time.
  - Confirmed scheduler unaffected throughout: stayed `Running` (as it
    already was, untouched by any of tonight's work — see Phase 2i's
    worklog for when this state change was first noticed), original
    `every1min` rule intact, `config_staged: false` at the end.

## Addendum: System Date/Time in Footer and Time & NTP (same session, after initial deploy)

User asked for the footer to also show system date/time, then — before
that landed — asked for the same on the Config page's Time & NTP card.
Both requests are the same underlying need, implemented together.

- `src/system_status.rs`: `SystemStatus` gained `now:
  chrono::DateTime<chrono::Utc>` — a plain `chrono::Utc::now()` read,
  deliberately a top-level field rather than nested inside the
  `Option<TimeSyncStatus>` (which can be `None` on a non-Linux dev
  target where `timedatectl` doesn't exist) — "what time is it" needs no
  platform-specific tool and should stay available even then. Existing
  `snapshot_returns_sane_memory_and_disk_figures` test extended with a
  freshness assertion (`now` within 5s of a fresh `Utc::now()` read) so
  a regression that left it at a fixed/default value wouldn't silently
  pass every other assertion in that test.
- `src/web/footer.js`: new `formatSystemNow`, prepended to the existing
  compact summary line. Formats `system.now` in `system.time_sync.timezone`
  when available (falls back to the browser's own zone otherwise) — the
  point is showing what the *Pi* thinks the time is, not the viewer's
  local time, same reasoning as the Config page's celestial-time
  formatting.
- `src/web/config.html` / `config.js`: new "Current time" row at the top
  of the Time & NTP `dl`, same timezone-aware formatting via
  `Intl.DateTimeFormat`. `renderTimeSync` now takes the whole `system`
  object (previously just `time_sync`) since it needs both `now` and
  `time_sync.timezone` together.

**Validation:** `cargo test --locked` — 112 passed, 0 failed (no new
test count change, one existing test extended). `node --check` + Biome
on `footer.js`/`config.js`/`config.html`: clean. Deployed; live-confirmed
`GET /api/system/status`'s `now` field is a fresh, correct UTC instant;
both `footer.js` and `config.js` reference the new fields; `config.html`
serves the new "Current time" row. Scheduler confirmed still `Running`
throughout (a real scheduled capture fired mid-deploy-verification,
visible in the service log — unrelated to and unaffected by this change).

## Addendum 2: Make the Clock Actually Tick

User asked, reasonably: "isn't date time supposed to be alive?" — both
displays were static snapshots from the last 15s poll, not a ticking
clock.

- `footer.js` / `config.js`: both now keep the last poll's `system`
  object plus the client-side timestamp it was received at as an anchor,
  and a separate `setInterval(..., 1000)` re-renders the clock (and, for
  the footer, the whole summary line, including a locally-extrapolated
  uptime) every second by adding elapsed client time to the anchor —
  no extra network traffic, self-corrects on every real 15s poll, and
  accurate to well within any perceptible drift for a display like this.

**Caught before deploying, not after**: `footer.js` and each page's own
script (`app.js`/`scheduler.js`/`capture-history.js`/`config.js`) are
all loaded as plain, non-module `<script>` tags on the same page, which
means they share one global scope — a fact this session had already
relied on deliberately when `footer.js` was first introduced. This
addendum's first draft declared `lastSystemStatus`/`lastPollClientTime`/
`estimatedNow` in `config.js` with the *exact* same names already used
in `footer.js`, which would have thrown `SyntaxError: Identifier
'lastSystemStatus' has already been declared` and broken every script on
`/config.html` the moment the page loaded. Caught via `node --check` on
the two files before any deploy, and fixed by renaming `config.js`'s
copies (`clockSystemStatus`/`clockPollClientTime`/`estimatedSystemNow`).
Verified concretely, not just by re-reading the diff: fetched the actual
served `footer.js` plus each page's own served script from the deployed
Pi, concatenated each pair exactly as a browser would load them, and ran
`node --check` on the combined result for all four pages — all four
parse cleanly.

**Validation**: `cargo fmt`/`clippy`/`test --locked` (109 passed, 0
failed — pure JS change, no new Rust tests). `node --check` + Biome on
both files: clean. Deployed; live-confirmed `GET /api/system/status`'s
`now` still fresh and correct; the four served-file concatenation checks
above all passed against the real deployed assets, not local copies;
scheduler confirmed still `Running`, unaffected, throughout.

## Remaining Limitations / Follow-up

- No interactive browser verification (headless session) — same stated
  limitation as every prior phase tonight.
- Network-interface listing and the 24h capture-health rollup are no
  longer shown anywhere in the UI (previously on the dashboard's System
  card). Not an oversight — a deliberate compactness tradeoff per the
  user's explicit "compact" request — but worth knowing if either is
  ever missed in practice.
- The new PolicyKit rule (like the two before it) is host config outside
  the git repo — a fresh Pi re-provision would need it reinstalled
  manually. Same pre-existing pattern, not a new gap.
