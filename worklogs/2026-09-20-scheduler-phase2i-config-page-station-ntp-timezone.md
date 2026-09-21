# Dated Worklog: 2026-09-20 - Phase 2i: Dedicated Config Page, Timezone Dropdown, NTP Sync Button

Status: **implemented, deployed, and verified.**

## Objective

User requested a dedicated Config page to host global settings (moving
Station off the dashboard), asked what other global config might belong
there, and requested two concrete additions: a proactive "Sync NTP now"
button, and converting the Station timezone field from free-text to a
dropdown.

## Research Findings (informed the plan below)

- `chrono-tz` is already a dependency (added earlier tonight for the
  ephemeris work) and exports `TZ_VARIANTS`, a full enumerable const list
  of every valid IANA timezone name — no new dependency needed for the
  dropdown.
- Found a real latent bug while investigating: `Station::tz()`
  (`optic_scheduler.rs`) does `self.timezone.parse().unwrap_or(chrono_tz::UTC)`
  — a typo'd timezone string today silently falls back to UTC with no
  warning, silently producing wrong sunrise/moon/Milky-Way math. Sourcing
  the dropdown from the same list the backend actually parses against
  makes an invalid value structurally impossible to submit.
- `optic_sync`'s remote host/port/user (where captures get uploaded) is
  deliberately sourced from environment variables at daemon startup, not
  from the web-editable `AppConfig` — considered as a Config-page
  candidate and rejected: letting an unauthenticated LAN web request
  redirect where raw capture data gets uploaded is a real risk, better
  left as a deploy-time decision.
- Confirmed live (read-only, non-destructive) on the Pi: `systemd-timesyncd`
  is the active time-sync mechanism (`timedatectl status`), already
  syncing automatically every ~30 min. There is **no D-Bus "force resync
  now" primitive** — `org.freedesktop.timesync1.Manager` only exposes
  `SetRuntimeNTPServers`. The only real lever is
  `systemctl restart systemd-timesyncd.service`.
- Restarting any systemd unit requires the broad
  `org.freedesktop.systemd1.manage-units` PolicyKit action (`implicit any:
  auth_admin`), which the sandboxed daemon can't authenticate for the same
  reason `sudo` doesn't work here (see `worklogs/2026-09-19-reboot-nonewprivileges-fix.md`
  — private user namespace from `ProtectSystem=strict`/`ProtectHome=read-only`).
  User chose to scope a new PolicyKit rule narrowly to exactly
  `systemd-timesyncd.service` + the `restart` verb, rather than a blanket
  any-unit grant — verified this works *before* writing any code (see
  Validation).

## Acceptance Criteria

1. New `/config.html` page hosts Station (lat/long/elevation/timezone)
   and a Time & NTP section; removed from the dashboard entirely.
2. Timezone is a `<select>` populated from the backend's actual valid
   IANA zone list (`GET /api/timezones`), not free text.
3. Config page shows current NTP sync status (synchronized: yes/no, NTP
   enabled, timezone) and a "Sync now" button that actually forces an
   immediate resync.
4. The new PolicyKit rule is scoped to exactly one unit + verb — verified
   both that it authorizes the intended action AND that it correctly
   denies a different unit, before being relied on by any code.
5. All four pages (Dashboard, Scheduler, Capture History, Config) share
   the same tab bar.

## Test Plan (written before implementation)

- Live, non-destructive PolicyKit verification (done *before* writing
  Rust code, same discipline as the reboot fix): install the scoped rule,
  `pkcheck` it authorizes `systemd-timesyncd.service`+`restart` and
  denies `ssh.service`+`restart`; then a real (safe — restarting the time
  sync client has no capture/camera impact) restart from inside a scratch
  unit matching the daemon's exact sandbox (`ProtectSystem=strict`,
  `ProtectHome=read-only`, no sudo), confirming `ActiveEnterTimestamp`
  actually changes.
- New unit test: `parse_timedatectl_show` against real captured
  `timedatectl show` output (same pattern as the existing
  `vcgencmd_temp_parses_real_output_format` test).
- New unit test(s) for `GET /api/timezones`: non-empty, contains a known
  real zone name.
- `cargo test/fmt/clippy`, `node --check` + Biome on every touched/new
  web asset, per this session's established gates.
- Live verification: `GET /api/timezones` returns a real list; `GET
  /api/system/status` includes real `time_sync` data; `POST
  /api/system/ntp-sync` actually restarts the service (confirm via a
  fresh `ActiveEnterTimestamp`); Station fields round-trip through
  `/config.html` exactly as they did on the dashboard; scheduler still
  `Paused` throughout.

## Implementation Summary

### PolicyKit (host config, outside the git repo)
- `/etc/polkit-1/rules.d/61-optic-daemon-ntp-sync.rules` (new, on the Pi):
  grants `liam` passwordless authorization for exactly
  `org.freedesktop.systemd1.manage-units` restricted to
  `action.lookup("unit") == "systemd-timesyncd.service"` and
  `action.lookup("verb") == "restart"` — not a blanket any-unit grant.
  Installed and verified live *before* any Rust code was written (see
  Validation).

### `src/system_status.rs`
- New `TimeSyncStatus { timezone, ntp_enabled, synchronized }`
  (`Option<TimeSyncStatus>` field on `SystemStatus`, `None` on non-Linux
  dev targets — same posture as `cpu_temp_celsius`).
- `time_sync_status()` (Linux) / `None` (elsewhere) runs `timedatectl show
  -p Timezone -p NTP -p NTPSynchronized`; `parse_timedatectl_show` parses
  the `Key=Value`-per-line output (deliberately not the `--value`-only
  form, which drops keys and relies on request-order — safer to parse
  even at the cost of a few more lines).
- `sync_ntp_now()`: `systemctl restart systemd-timesyncd.service`, no
  sudo — same D-Bus/PolicyKit pattern as `reboot_host()`, doc comment
  cross-references it and explains why the broader `manage-units` action
  needed a narrowly-scoped rule rather than reuse.

### `src/web.rs`
- `POST /api/system/ntp-sync` → `system_ntp_sync` handler, mirrors
  `system_reboot` exactly.
- `GET /api/timezones` → `list_timezones` handler: `Json(chrono_tz::TZ_VARIANTS.iter().map(|tz| tz.name()).collect())`.
- `SystemStatusResponse` already wraps `SystemStatus`, so `time_sync`
  flows to `/api/system/status` with no extra plumbing.

### Station moved off the dashboard
- `src/web/index.html`: `.station-card` section removed entirely.
- `src/web/app.js`: every Station-specific element ref, function
  (`populateStationFields`, `stationFromFields`, `showStationNotice`,
  `stageStation`), event listener, and the `scheduleRulesCache`/
  `stationFieldsInitialized` state removed. `fnDiscardConfig`'s comment
  narrowed to reflect that only camera fields need local re-populating
  now (the backend still reverts both together — commit/discard act on
  one shared preview file — but there's no Station UI on this page
  anymore to refresh).
- `src/web/config.html` (new) + `src/web/config.js` (new): the extracted
  Station logic verbatim, plus a Time & NTP section (`dl` status display
  + "Sync now" button) polling `/api/system/status` independently
  (`setInterval(refreshTimeSync, 15000)`, separate from the Station
  section's own `/api/status` poll) — matches this codebase's existing
  per-page-own-script convention (no shared module system between pages).
- Timezone: `<select id="station-timezone">` populated from
  `GET /api/timezones` on load (`populateTimezoneOptions`, chained before
  the first `refreshStatus()` so the dropdown has real `<option>`s before
  anything tries to set `.value` on it), replacing the old free-text
  `<input>`.

### Nav + cross-page references
- All four pages (`index.html`, `scheduler.html`, `capture-history.html`,
  `config.html`) now share the same 4-tab `app-tabs` bar
  (Dashboard / Scheduler / Capture History / Config).
- `scheduler.html`'s Ephemeris-trigger help text ("Requires a Station to
  be set on the...") now links to `/config.html` instead of `/`.
- `styles.css`: `.station-card` removed from both grid-column-placement
  selector lists (dead now that the section no longer exists on any
  page); `.station-grid` (the label/input layout) kept — reused as-is on
  the new Config page; `main.config-main` added alongside the existing
  `main.capture-history-main` single-column-full-width rule.

### `scripts/build-deploy-optic-daemon.sh`
- Added `config.js`/`config.html` to the pre-deploy Biome gate and new
  post-deploy content-verification checks (`Time &amp; NTP` in the served
  HTML, `/api/timezones` in the served JS) — closing the same kind of gap
  Phase 2f's own worklog found and fixed for `capture-history.*` after
  the fact; done proactively this time, in the same pass as the feature.

### `docs/optic-daemon-scheduler.md`
- §14's Station-config resolution entry updated to record the move off
  the dashboard, the new page, and the free-text→dropdown fix with its
  underlying-bug rationale.

## Validation

- **PolicyKit, verified live *before* writing any Rust code** (all
  non-destructive except the final real restart, which is itself safe —
  restarting the time-sync client has no capture/camera impact):
  - `timedatectl status` confirmed `systemd-timesyncd` is the active
    mechanism, already `NTPSynchronized=yes`.
  - Confirmed via `busctl introspect` that
    `org.freedesktop.timesync1.Manager` has no "resync now" method —
    only `SetRuntimeNTPServers` — so `manage-units`+restart really is
    the only lever.
  - Installed the scoped rule; `pkcheck --detail unit
    systemd-timesyncd.service --detail verb restart` → **authorized**;
    the identical check with `--detail unit ssh.service` → **denied**
    (`Authorization requires authentication`) — confirms the scope is
    genuinely narrow, not accidentally broad.
  - Real restart from inside a scratch `systemd-run --user` unit with the
    daemon's exact sandbox flags (`ProtectSystem=strict`,
    `ProtectHome=read-only`, `NoNewPrivileges=true`, no sudo) → exit 0;
    `systemd-timesyncd`'s `ActiveEnterTimestamp` genuinely advanced,
    confirming a real restart, not a no-op.
- `cargo test --locked`: **106 passed, 0 failed** (4 new: 3
  `parse_timedatectl_show` cases, 1 `list_timezones` sanity check).
- `cargo fmt --all -- --check` / `cargo clippy --locked --all-targets --
  -D warnings`: clean.
- `node --check` on every JS file (`app.js`, `config.js`,
  `capture-history.js`, `scheduler.js`): clean. Biome across all 8
  tracked web assets: clean.
- `bash -n scripts/build-deploy-optic-daemon.sh`: clean (verified the new
  content-check block's `if`/`fi` nesting is correct after editing).
- Deployed via `./scripts/build-deploy-optic-daemon.sh`.
- **Live verification** against the real deployed Pi:
  - `GET /api/timezones` → real list including `America/Vancouver` and
    `UTC`.
  - `GET /api/system/status` → real `time_sync` object
    (`{timezone, ntp_enabled, synchronized}`) matching `timedatectl`'s
    own live values.
  - `POST /api/system/ntp-sync` → 200; confirmed `systemd-timesyncd`'s
    `ActiveEnterTimestamp` advanced again (the real endpoint, not just
    the earlier scratch-unit rehearsal).
  - `GET /config.html` and `/config.js` both 200; served content passes
    the new deploy-script checks.
  - Station fields round-trip through `/config.html` exactly as before
    (staged via `/api/schedule/preview`, `config_staged` flips correctly,
    Save/Discard behave identically to their old dashboard-card
    behavior) — confirmed via the same stage→commit→verify sequence used
    in Phase 2h's DNG validation.
  - `GET /` confirmed the Station card is gone and the 4-tab nav is
    present; all four pages' nav bars confirmed identical.
  - Scheduler confirmed `Paused`, original `every1min` rule intact,
    throughout.

## Remaining Limitations / Follow-up

- No interactive browser verification (headless session) — same stated
  limitation as every prior phase tonight. The timezone `<select>`'s
  ~400-entry usability (native browser type-to-jump, no custom search
  box) is a reasonable-by-design choice, not verified by actually typing
  into it in a browser.
- The new PolicyKit rule is host config living outside the git repo
  (same category as the existing reboot rule) — not captured by any
  setup script, so a fresh Pi re-provision would need it reinstalled
  manually. This matches the existing reboot rule's same untracked state;
  flagged as a pre-existing pattern this doesn't newly introduce, not a
  new gap.
